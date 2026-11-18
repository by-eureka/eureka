//! Compact a full conversation without restarting the sandbox or its helpers.
//!
//! The same prover writes a short handoff. Original instructions and the initial
//! task are kept verbatim; only conversation history is replaced. The target on
//! disk remains authoritative. Captured history, summary requests and snapshots
//! stay in the attempt's artifacts, so compaction never erases the rollout.

use crate::responses::{Client, ContextLengthExceeded, Item, Usage};
use crate::tools::Ctx;
use crate::trace::{Trace, artifact};
use anyhow::{Context, Result};
use serde_json::{Value, json};

const SUMMARY_INSTRUCTIONS: &str = "Write a concise handoff for the same Lean prover continuing \
this task. Do not solve new mathematics or call tools. Preserve the current plan, proven lemmas \
and their exact names, failed approaches, remaining obligations, recent Lean/verifier feedback, \
and IDs/status of existing helper agents. Distinguish checked facts from conjectures. The proof \
file on disk is authoritative. Include useful details from any previous handoff. Return only \
the handoff, under 1500 words.";

/// No lifetime limit on compactions. The caller only prevents an immediate loop
/// if even the freshly compacted conversation cannot fit the provider's window.
pub(crate) async fn compact(
    client: &Client,
    request: &Value,
    ctx: &Ctx,
    number: usize,
    capture: bool,
    usage: &mut Usage,
) -> Result<Vec<Item>> {
    let dir = ctx.artifacts.join(format!("compaction-{number:04}"));
    std::fs::create_dir(&dir)?;
    let target = ctx
        .sandbox
        .read_file(&ctx.root, &ctx.target, 4 * 1024 * 1024)
        .await?;
    artifact(&dir.join("candidate.lean"), &target)?;
    if capture {
        artifact(
            &dir.join("history.json"),
            &serde_json::to_vec(&request["input"])?,
        )?;
    }
    ctx.trace.event(&ctx.id, "compaction_start", json!({
        "number": number, "items_before": request["input"].as_array().map(Vec::len), "artifacts": dir,
    }));

    let mut body = request.clone();
    body["instructions"] = SUMMARY_INSTRUCTIONS.into();
    body["input"]
        .as_array_mut()
        .context("missing conversation")?
        .push(serde_json::to_value(Item::user(
            "Write the continuation handoff now.",
        ))?);
    // Reserving a short answer usually fits where the normal 300k output reserve
    // does not. Keep tool definitions for historical calls, but forbid new calls.
    body["max_output_tokens"] = client.config.max_output_tokens.min(8192).into();
    body["tool_choice"] = "none".into();
    let mut summary = None;
    for pass in 0..2 {
        let stem = format!("summary-{}", pass + 1);
        if capture {
            artifact(
                &dir.join(format!("{stem}-request.json")),
                &serde_json::to_vec(&body)?,
            )?;
        }
        let stream = capture
            .then(|| Trace::open(&dir.join(format!("{stem}-stream.jsonl"))))
            .transpose()?;
        match client
            .complete(&body, &ctx.trace, &ctx.id, stream.as_ref())
            .await
        {
            Ok(completion) => {
                usage.add(&completion.usage);
                if capture {
                    artifact(
                        &dir.join(format!("{stem}-response.json")),
                        &serde_json::to_vec(&completion)?,
                    )?;
                }
                ctx.trace.event(
                    &ctx.id,
                    "compaction_model",
                    json!({
                        "number": number, "usage": completion.usage,
                        "incomplete": completion.incomplete,
                        "request": capture.then(|| dir.join(format!("{stem}-request.json"))),
                        "response": capture.then(|| dir.join(format!("{stem}-response.json"))),
                    }),
                );
                if client.config.logprobs {
                    completion.require_logprobs()?;
                }
                let text = completion
                    .items
                    .iter()
                    .filter_map(Item::assistant_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                if completion.incomplete.is_none() && !text.trim().is_empty() {
                    summary = Some(text);
                    break;
                }
            }
            Err(error) if error.is::<ContextLengthExceeded>() && pass == 0 => {}
            Err(error) => return Err(error).context("context compaction failed"),
        }
        // One large tool result can fill even the summary request. Use bounded
        // excerpts as plain text, never dangling function-call/output items.
        body["input"] = serde_json::to_value([Item::user(excerpt(&request["input"])?)])?;
    }
    // If reasoning used the summary's output budget, preserve recent raw state
    // instead. This fallback adds no claims and the saved file is still intact.
    let summary = match summary {
        Some(text) => text,
        None => excerpt(&request["input"])?,
    };
    let handoff = format!(
        "Conversation compacted. Continue the original task; do not give up. \
         The sandbox, files and existing helpers are unchanged. Read {} and check current Lean \
         diagnostics before editing; the file is authoritative and this handoff may contain \
         unverified claims. Collect existing helpers rather than duplicating their tasks.\n\n{summary}",
        ctx.target,
    );
    artifact(&dir.join("handoff.txt"), handoff.as_bytes())?;
    ctx.trace.event(
        &ctx.id,
        "compaction",
        json!({
            "number": number, "items_after": 2, "artifacts": dir,
        }),
    );
    ctx.trace.check()?;
    Ok(vec![
        serde_json::from_value(request["input"][0].clone())?,
        Item::user(handoff),
    ])
}

fn excerpt(items: &Value) -> Result<String> {
    let text = serde_json::to_string(items)?;
    let limit = 32 * 1024;
    if text.len() <= 2 * limit {
        return Ok(text);
    }
    let head = text.floor_char_boundary(limit);
    let tail = text.ceil_char_boundary(text.len() - limit);
    Ok(format!(
        "{}\n...[history omitted; reread the current proof file]...\n{}",
        &text[..head],
        &text[tail..]
    ))
}
