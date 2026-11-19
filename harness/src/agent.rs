//! The model/tool loop for one attempt.
//!
//! search.rs sets up the sandbox and tools, calls `run`, and does cleanup and
//! verification afterwards. `run` returns an Outcome even when the model fails,
//! so the caller can still save the target file.
//!
//! History is append-only between compactions so prefix caching keeps working. With
//! `--cache-dir`, proof requests are also memoized on disk, keyed by their exact
//! bodies. Compaction summaries are captured with their own artifacts.

use crate::responses::{Client, Completion, ContextLengthExceeded, Item, Usage, parse_arguments};
use crate::tools::{Ctx, Ending, Tool, definition};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// HTTP retries within a turn don't count against this.
    pub max_turns: usize,
    pub cache_dir: Option<PathBuf>,
    pub capture_requests: bool,
    /// A text-only reply is a continuation, not permission to abandon the task.
    pub dont_give_up: bool,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// The model called `submit` (lead) or `finish` (helper). Nothing is verified yet.
    pub submitted: bool,
    pub turns: usize,
    pub usage: Usage,
    pub last_message: Option<String>,
    pub error: Option<String>,
    pub termination: String,
}

/// On error, the Outcome still has the usage and last message from the turns
/// that completed.
pub async fn run(
    client: &Client,
    config: &AgentConfig,
    instructions: &str,
    prompt: &str,
    tools: &[Arc<dyn Tool>],
    ctx: &Ctx,
) -> Outcome {
    let mut outcome = Outcome::default();
    let result = run_loop(
        client,
        config,
        instructions,
        prompt,
        tools,
        ctx,
        &mut outcome,
    )
    .await;
    if let Err(error) = result {
        outcome.error = Some(format!("{error:#}"));
        outcome.termination = "error".into();
        ctx.trace.event(
            &ctx.id,
            "agent_error",
            json!({"turn": outcome.turns, "error": outcome.error, "usage": outcome.usage}),
        );
    }
    if outcome.termination.is_empty() {
        outcome.termination = "turn_limit".into();
    }
    outcome
}

async fn run_loop(
    client: &Client,
    config: &AgentConfig,
    instructions: &str,
    prompt: &str,
    tools: &[Arc<dyn Tool>],
    ctx: &Ctx,
    outcome: &mut Outcome,
) -> Result<()> {
    let specs: Vec<Value> = tools.iter().map(|t| definition(&t.spec())).collect();
    let accepts_report = specs.iter().any(|tool| tool["name"] == "finish_report");
    let mut items = vec![Item::user(prompt)];
    let mut requests = 0;
    let mut compactions = 0;
    let mut compacted_since_response = false;

    'turns: while outcome.turns < config.max_turns {
        if ctx.stop.is_cancelled() {
            outcome.termination = "cancelled".into();
            break;
        }
        ctx.trace.check()?;
        outcome.turns += 1;
        requests += 1;
        let body = client.body(instructions, &items, &specs, Some(&ctx.id));
        let started = Instant::now();
        let request_file = ctx.artifacts.join(format!("request-{requests:04}.json"));
        if config.capture_requests {
            // The API key is sent as a header, so the body is safe to save.
            crate::trace::artifact(&request_file, &serde_json::to_vec(&body)?)?;
        }
        ctx.trace.event(&ctx.id, "model_start", json!({"turn": outcome.turns, "request_number": requests, "request": config.capture_requests.then_some(&request_file)}));
        let (completion, cached) = match cached_completion(config, &body, client.endpoint()) {
            Some(c) => (c, true),
            None => {
                let capture = config
                    .capture_requests
                    .then(|| {
                        crate::trace::Trace::open(
                            &ctx.artifacts.join(format!("stream-{requests:04}.jsonl")),
                        )
                    })
                    .transpose()?;
                let c = tokio::select! {
                    biased;
                    // Check cancellation first so a stopped attempt doesn't take another turn.
                    _ = ctx.stop.cancelled() => { outcome.termination = "cancelled".into(); break; }
                    result = client.complete(&body, &ctx.trace, &ctx.id, capture.as_ref()) => result,
                };
                let c = match c {
                    Ok(c) => c,
                    Err(error)
                        if error.is::<ContextLengthExceeded>() && !compacted_since_response =>
                    {
                        compactions += 1;
                        items = tokio::select! {
                            biased;
                            _ = ctx.stop.cancelled() => { outcome.termination = "cancelled".into(); break; }
                            result = crate::compaction::compact(client, &body, ctx,
                                compactions, config.capture_requests, &mut outcome.usage) => result?,
                        };
                        compacted_since_response = true;
                        // A rejected request did no proof work. Unique request
                        // numbers retain it without spending the agent's turn budget.
                        outcome.turns -= 1;
                        continue;
                    }
                    Err(error) if error.is::<ContextLengthExceeded>() => {
                        return Err(error)
                            .context("compacted conversation still exceeds the context window");
                    }
                    Err(error) => return Err(error),
                };
                store_completion(config, &body, &c, client.endpoint());
                (c, false)
            }
        };
        compacted_since_response = false;
        outcome.usage.add(&completion.usage);
        let response_file = ctx.artifacts.join(format!("response-{requests:04}.json"));
        if config.capture_requests {
            crate::trace::artifact(&response_file, &serde_json::to_vec(&completion)?)?;
        }
        ctx.trace.event(
            &ctx.id,
            "model",
            json!({
                "turn": outcome.turns, "ms": started.elapsed().as_millis(), "cached_response": cached,
                "request_number": requests, "request": config.capture_requests.then_some(&request_file),
                "response": config.capture_requests.then_some(&response_file),
                "usage": completion.usage, "items": completion.items.len(), "incomplete": completion.incomplete,
                "text": completion.items.iter().filter_map(Item::assistant_text).collect::<Vec<_>>().join("\n"),
            }),
        );
        if client.config.logprobs {
            completion.require_logprobs()?;
        }
        if let Some(text) = completion
            .items
            .iter()
            .filter_map(Item::assistant_text)
            .next_back()
        {
            outcome.last_message = Some(text);
        }

        let calls: Vec<(String, String, String)> = completion
            .items
            .iter()
            .filter_map(|item| match item {
                Item::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => Some((call_id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();
        let incomplete = completion.incomplete.clone();
        // Function calls have to come before their outputs in the history.
        items.extend(completion.items);
        if calls.is_empty() {
            if let Some(reason) = incomplete {
                // The turn was cut off before any action; ask for a shorter one.
                items.push(Item::user(format!(
                    "Your previous turn stopped early ({reason}) before calling any tool. Continue from where you were, keep reasoning brief, and act with tools in small steps."
                )));
                continue;
            }
            if accepts_report {
                items.push(Item::user("Record your written findings with finish_report(note), or submit a complete proof if your workflow offers submit. Include the result itself in the note."));
                continue;
            }
            if config.dont_give_up {
                items.push(Item::user(
                    "Continue the assigned workflow. Use the available completion tool for your role \
                     when its requested proof, report or handoff is ready.",
                ));
                continue;
            }
            // A reply with no tool calls ends the attempt without a submission.
            outcome.termination = "no_tool_calls".into();
            break;
        }

        // Model call order is observable. Concurrency is explicit in lean_check
        // batches and independently owned subagents.
        for (call_id, name, arguments) in &calls {
            let output = tokio::select! {
                biased;
                _ = ctx.stop.cancelled() => { outcome.termination = "cancelled".into(); break; }
                output = invoke(tools, call_id, name, arguments, ctx) => output,
            };
            items.push(Item::output(call_id, output));
            if let Some(ending) = ctx.ending.lock().unwrap().as_ref() {
                outcome.submitted = !matches!(ending, Ending::Report(_));
                outcome.termination = if outcome.submitted {
                    "submitted"
                } else {
                    "reported"
                }
                .into();
                break 'turns;
            }
        }
    }
    Ok(())
}

/// Run one tool call. Errors are returned as `ERROR: ...` text for the model to
/// read. If the watchdog fires, the tool may be wedged, so the whole attempt is
/// cancelled.
async fn invoke(
    tools: &[Arc<dyn Tool>],
    call_id: &str,
    name: &str,
    arguments: &str,
    ctx: &Ctx,
) -> String {
    let started = Instant::now();
    ctx.trace.event(
        &ctx.id,
        "tool_start",
        json!({"call_id": call_id, "name": name, "arguments": arguments}),
    );
    let output = match (
        tools.iter().find(|t| t.spec().name == name),
        parse_arguments(arguments),
    ) {
        (Some(tool), Ok(args)) => {
            // Longer than the maximum shell timeout, so this only fires for a stuck tool.
            match tokio::time::timeout(std::time::Duration::from_secs(1830), tool.call(args, ctx))
                .await
            {
                Ok(result) => result.unwrap_or_else(|e| format!("ERROR: {e:#}")),
                Err(_) => {
                    ctx.stop.cancel();
                    "ERROR: tool watchdog expired; stopping attempt".into()
                }
            }
        }
        (None, _) => format!("ERROR: unknown tool `{name}`"),
        (_, Err(e)) => format!("ERROR: {e:#}"),
    };
    ctx.trace.event(
        &ctx.id,
        "tool",
        json!({"call_id": call_id, "name": name, "error": output.starts_with("ERROR:"), "arguments": arguments, "ms": started.elapsed().as_millis(), "output": output}),
    );
    output
}

fn cache_path(config: &AgentConfig, body: &Value, endpoint: &str) -> Option<PathBuf> {
    let dir = config.cache_dir.as_ref()?;
    let mut hash = Sha256::new();
    // Include the endpoint so two servers with the same model name don't share entries.
    hash.update(endpoint.as_bytes());
    hash.update([0]);
    hash.update(serde_json::to_vec(body).ok()?);
    let digest = hash.finalize();
    Some(dir.join(format!("{:x}.json", digest)))
}

fn cached_completion(config: &AgentConfig, body: &Value, endpoint: &str) -> Option<Completion> {
    // An unreadable entry is a miss.
    let text = std::fs::read(cache_path(config, body, endpoint)?).ok()?;
    serde_json::from_slice(&text).ok()
}

fn store_completion(config: &AgentConfig, body: &Value, completion: &Completion, endpoint: &str) {
    // Best effort: a failed cache write shouldn't fail the turn.
    if let Some(path) = cache_path(config, body, endpoint) {
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(path, serde_json::to_vec(completion).unwrap_or_default());
    }
}
