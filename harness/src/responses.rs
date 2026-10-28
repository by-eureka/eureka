//! Streaming client for the OpenAI Responses API (`POST /responses`).
//!
//! The event handling is derived from openai/codex `codex-rs/codex-api/src/sse/responses.rs`
//! and `codex-client/src/retry.rs` (Apache-2.0, commit 402f5b6), reduced to what a
//! single-model prover needs: complete output items, usage, errors, idle timeout, retries.
//! Items we do not interpret (reasoning) are kept verbatim so they can be sent back.
//!
//! Only fully received items are kept, and a retry throws away everything from
//! the broken stream, so a half-streamed function call is never run. The
//! endpoint must support `/responses`; `/chat/completions` won't work.

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};
use std::time::Duration;

/// One item of the conversation, in the Responses API wire format.
/// Fields we don't model are kept in `rest` and sent back unchanged. Reasoning
/// items carry encrypted state the provider needs on the next turn.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    Message {
        role: String,
        content: Vec<Content>,
        #[serde(flatten)]
        rest: Map<String, Value>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        /// JSON text, as the API sends it.
        arguments: String,
        #[serde(flatten)]
        rest: Map<String, Value>,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
    /// Provider reasoning; passed back untouched on later turns.
    Reasoning {
        #[serde(flatten)]
        rest: Map<String, Value>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    InputText { text: String },
    OutputText { text: String },
}

impl Item {
    pub fn user(text: impl Into<String>) -> Self {
        Item::Message {
            role: "user".into(),
            content: vec![Content::InputText { text: text.into() }],
            rest: Map::new(),
        }
    }

    pub fn output(call_id: impl Into<String>, output: impl Into<String>) -> Self {
        Item::FunctionCallOutput {
            call_id: call_id.into(),
            output: output.into(),
        }
    }

    /// Visible text of an assistant message, if this is one.
    pub fn assistant_text(&self) -> Option<String> {
        match self {
            Item::Message { role, content, .. } if role == "assistant" => Some(
                content
                    .iter()
                    .map(|c| match c {
                        Content::InputText { text } | Content::OutputText { text } => text.as_str(),
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    /// Includes `cached_input_tokens`.
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// Included in `output_tokens`.
    pub reasoning_tokens: u64,
}

impl Usage {
    // Only completed responses report usage, so tokens spent on a stream that
    // failed and was retried are not counted.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Completion {
    pub items: Vec<Item>,
    pub usage: Usage,
    /// Set when the response stopped early (e.g. `max_output_tokens`); the
    /// items received so far are still valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<String>,
    /// Keep the provider's terminal response intact for datasets. Some servers
    /// put logprobs here but omit them from output_item.done. This is not replayed
    /// into the conversation; `items` is the history used by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
}

impl Completion {
    /// Check after saving the response, before executing any tools. The Responses
    /// API exposes output-text logprobs; reasoning/tool-token coverage is not implied.
    pub fn require_logprobs(&self) -> Result<()> {
        let mut found = false;
        for part in self
            .response
            .iter()
            .filter_map(|r| r.get("output").and_then(Value::as_array))
            .flatten()
            .filter_map(|i| i.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("output_text"))
        {
            if part
                .get("text")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                continue;
            }
            let scores = part.get("logprobs").and_then(Value::as_array);
            anyhow::ensure!(
                scores.is_some_and(|s| !s.is_empty()
                    && s.iter()
                        .all(|t| t.get("logprob").and_then(Value::as_f64).is_some())),
                "logprobs requested but missing or invalid for output text; use --logprobs=false only to explicitly allow unscored output"
            );
            found = true;
        }
        anyhow::ensure!(
            found,
            "logprobs requested but no scored output text was returned (reasoning/tool-only responses may not expose logprobs); use --logprobs=false only to explicitly allow unscored output"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Request<'a> {
    pub model: &'a str,
    pub stream: bool,
    pub instructions: &'a str,
    pub input: &'a [Item],
    pub tools: &'a [Value],
    pub tool_choice: &'static str,
    pub parallel_tool_calls: bool,
    pub reasoning: Reasoning<'a>,
    pub store: bool,
    pub include: &'static [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<&'a str>,
    pub max_output_tokens: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Reasoning<'a> {
    pub effort: &'a str,
}

#[derive(Clone, Debug)]
pub struct ModelConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub effort: String,
    pub max_output_tokens: u32,
    pub logprobs: bool,
    pub idle_timeout: Duration,
    pub max_attempts: u32,
}

pub struct Client {
    http: reqwest::Client,
    pub config: ModelConfig,
    bearer_auth: bool,
    extra_headers: HeaderMap,
}

#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    item: Option<Value>,
    response: Option<Value>,
    error: Option<Value>,
}

enum Attempt {
    Done(Completion),
    Retry(String),
}

/// Retrying the same body cannot fix this. The agent must compact its history.
#[derive(Debug)]
pub struct ContextLengthExceeded(String);

impl std::fmt::Display for ContextLengthExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ContextLengthExceeded {}

fn context_length_exceeded(error: &Value) -> bool {
    error["code"].as_str() == Some("context_length_exceeded")
        || (error["param"].as_str() == Some("input_tokens")
            && error["message"]
                .as_str()
                .is_some_and(|s| s.contains("maximum context length")))
}

impl Client {
    pub fn new(config: ModelConfig) -> Result<Self> {
        Self::new_with_headers(config, true, HeaderMap::new())
    }

    pub fn new_with_headers(
        config: ModelConfig,
        bearer_auth: bool,
        extra_headers: HeaderMap,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Also bound silent waits for headers and HTTP error bodies. Each
            // successful read resets this timer; it is not a total deadline.
            .read_timeout(config.idle_timeout)
            .build()?;
        Ok(Self {
            http,
            config,
            bearer_auth,
            extra_headers,
        })
    }

    fn request(&self, url: &str, body: &Value) -> reqwest::RequestBuilder {
        let mut request = self.http.post(url).json(body);
        if self.bearer_auth {
            request = request.bearer_auth(&self.config.api_key);
        }
        for (name, value) in &self.extra_headers {
            request = request.header(name.clone(), value.clone());
        }
        request
    }

    /// Build the request body for `input` with the given tools. Public so the
    /// agent can key its response cache on the exact bytes sent.
    pub fn body(
        &self,
        instructions: &str,
        input: &[Item],
        tools: &[Value],
        cache_key: Option<&str>,
    ) -> Value {
        // With store=false we resend the whole history each turn, so we ask for
        // the encrypted reasoning to include in it. The model may return several
        // tool calls at once, but agent.rs runs them one at a time.
        let mut body = serde_json::to_value(Request {
            model: &self.config.model,
            stream: true,
            instructions,
            input,
            tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: Reasoning {
                effort: &self.config.effort,
            },
            store: false,
            include: &["reasoning.encrypted_content"],
            prompt_cache_key: cache_key,
            max_output_tokens: self.config.max_output_tokens,
        })
        .expect("request is serializable");
        if self.config.logprobs {
            body["include"]
                .as_array_mut()
                .unwrap()
                .push("message.output_text.logprobs".into());
            // Zero alternatives still requests the probability of the emitted token.
            body["top_logprobs"] = 0.into();
        }
        body
    }

    pub fn endpoint(&self) -> &str {
        &self.config.base_url
    }

    /// One completion, with retries on 429/5xx/transport errors.
    pub async fn complete(
        &self,
        body: &Value,
        trace: &crate::trace::Trace,
        id: &str,
        capture: Option<&crate::trace::Trace>,
    ) -> Result<Completion> {
        let mut last = String::new();
        for attempt in 0..self.config.max_attempts {
            if attempt > 0 {
                tokio::time::sleep(backoff(attempt)).await;
            }
            trace.event(id, "request_start", serde_json::json!({"try": attempt + 1}));
            capture_event(
                capture,
                id,
                "request",
                serde_json::json!({"try": attempt + 1, "body": body}),
            )?;
            // Long reasoning can stream for more than 30 minutes. Let the
            // output-token limit bound it; only silence should time out here.
            // agent.rs can still cancel the request when the run is stopped.
            let response = self.once(body, capture, id, attempt + 1).await;
            trace.event(id, "request_end", serde_json::json!({"try": attempt + 1, "error": response.as_ref().err().map(|e| format!("{e:#}"))}));
            match response? {
                Attempt::Done(c) => return Ok(c),
                Attempt::Retry(reason) => {
                    trace.event(
                        id,
                        "request_retry",
                        serde_json::json!({"try": attempt + 1, "reason": reason}),
                    );
                    last = reason;
                }
            }
        }
        bail!(
            "model request failed after {} attempts: {last}",
            self.config.max_attempts
        )
    }

    async fn once(
        &self,
        body: &Value,
        capture: Option<&crate::trace::Trace>,
        id: &str,
        attempt: u32,
    ) -> Result<Attempt> {
        let url = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
        let response = match self.request(&url, body).send().await
        {
            Ok(r) => r,
            Err(e) => return Ok(Attempt::Retry(format!("transport: {e}"))),
        };
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            capture_event(
                capture,
                id,
                "http_error",
                serde_json::json!({"try": attempt, "status": status.as_u16(), "body": text}),
            )?;
            if status.as_u16() == 429 || status.is_server_error() {
                return Ok(Attempt::Retry(format!("HTTP {status}: {text}")));
            }
            if status.as_u16() == 400
                && serde_json::from_str::<Value>(&text)
                    .is_ok_and(|body| context_length_exceeded(&body["error"]))
            {
                return Err(ContextLengthExceeded(format!("HTTP {status}: {text}")).into());
            }
            // Bad credentials/configuration remain fatal; never hide an arbitrary 400.
            bail!("HTTP {status}: {text}");
        }

        let mut events = response.bytes_stream().eventsource();
        let mut items = Vec::new();
        let mut failure: Option<String> = None;
        loop {
            let next = tokio::time::timeout(self.config.idle_timeout, events.next()).await;
            let event = match next {
                Err(_) => return Ok(Attempt::Retry("idle timeout waiting for SSE".into())),
                Ok(None) => {
                    return Ok(Attempt::Retry(failure.unwrap_or_else(|| {
                        "stream closed before response.completed".into()
                    })));
                }
                Ok(Some(Err(e))) => return Ok(Attempt::Retry(format!("stream: {e}"))),
                Ok(Some(Ok(event))) => event,
            };
            // Save before interpreting. This retains reasoning deltas, logprobs,
            // unknown fields and partial output even if the stream is interrupted.
            // No authorization headers are written. Each retry has its own try ID.
            capture_event(
                capture,
                id,
                "sse",
                serde_json::json!({"try": attempt, "data": event.data}),
            )?;
            let event: StreamEvent = match serde_json::from_str(&event.data) {
                Ok(e) => e,
                Err(_) => continue,
            };
            match event.kind.as_str() {
                "response.output_item.done" => {
                    // Unknown item kinds are skipped rather than failing the turn.
                    if let Some(item) = event
                        .item
                        .and_then(|v| serde_json::from_value::<Item>(v).ok())
                    {
                        items.push(item);
                    }
                }
                "response.completed" => {
                    // response.status isn't checked. A server that sends
                    // `completed` for a truncated response won't trigger the
                    // continuation in agent.rs.
                    let usage = event
                        .response
                        .as_ref()
                        .and_then(|r| r.get("usage"))
                        .map(parse_usage)
                        .unwrap_or_default();
                    return Ok(Attempt::Done(Completion {
                        items,
                        usage,
                        incomplete: None,
                        response: event.response,
                    }));
                }
                "response.incomplete" => {
                    // Keep what arrived; agent.rs will ask the model to continue.
                    let reason = event
                        .response
                        .as_ref()
                        .and_then(|r| r.pointer("/incomplete_details/reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    if reason == "content_filter" {
                        bail!("incomplete response: {reason}");
                    }
                    let usage = event
                        .response
                        .as_ref()
                        .and_then(|r| r.get("usage"))
                        .map(parse_usage)
                        .unwrap_or_default();
                    return Ok(Attempt::Done(Completion {
                        items,
                        usage,
                        incomplete: Some(reason),
                        response: event.response,
                    }));
                }
                "response.failed" => {
                    if let Some(error) = event.response.as_ref().and_then(|r| r.get("error"))
                        && context_length_exceeded(error)
                    {
                        return Err(ContextLengthExceeded(error.to_string()).into());
                    }
                    let message = event
                        .response
                        .as_ref()
                        .and_then(|r| r.pointer("/error/message"))
                        .and_then(Value::as_str)
                        .unwrap_or("response.failed");
                    return Ok(Attempt::Retry(message.to_owned()));
                }
                "error" => {
                    if let Some(error) = &event.error
                        && context_length_exceeded(error)
                    {
                        return Err(ContextLengthExceeded(error.to_string()).into());
                    }
                    // Remember the message in case the stream closes next.
                    failure = Some(
                        event
                            .error
                            .as_ref()
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("error event")
                            .to_owned(),
                    );
                }
                _ => {}
            }
        }
    }
}

/// Resolve repeatable `HEADER=ENV_VAR` specifications into sensitive HTTP headers.
/// The environment-variable name may appear in argv/configuration; the secret value does not.
pub fn headers_from_env(specs: &[String]) -> Result<HeaderMap> {
    headers_from_env_with(specs, |name| std::env::var(name).ok())
}

fn headers_from_env_with<F>(specs: &[String], mut lookup: F) -> Result<HeaderMap>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut headers = HeaderMap::new();
    for spec in specs {
        let (raw_name, env_name) = spec
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --header-env `{spec}`; expected HEADER=ENV_VAR"))?;
        anyhow::ensure!(
            !raw_name.trim().is_empty() && !env_name.trim().is_empty(),
            "invalid --header-env `{spec}`; expected non-empty HEADER=ENV_VAR"
        );
        let name = HeaderName::from_bytes(raw_name.trim().as_bytes())
            .with_context(|| format!("invalid HTTP header name in --header-env `{spec}`"))?;
        let secret = lookup(env_name.trim()).ok_or_else(|| {
            anyhow!(
                "environment variable `{}` referenced by --header-env `{spec}` is not set",
                env_name.trim()
            )
        })?;
        let mut value = HeaderValue::from_bytes(secret.as_bytes()).with_context(|| {
            format!(
                "environment variable `{}` contains a value that is invalid for HTTP header `{}`",
                env_name.trim(),
                name
            )
        })?;
        value.set_sensitive(true);
        anyhow::ensure!(
            headers.insert(name.clone(), value).is_none(),
            "duplicate provider header `{name}` in --header-env"
        );
    }
    Ok(headers)
}

fn capture_event(
    capture: Option<&crate::trace::Trace>,
    id: &str,
    kind: &str,
    fields: Value,
) -> Result<()> {
    if let Some(capture) = capture {
        capture.event(id, kind, fields);
        capture.check()?; // Stop on a full disk instead of silently losing the rollout.
    }
    Ok(())
}

// Missing counters read as zero; some compatible servers leave out the breakdowns.
fn parse_usage(usage: &Value) -> Usage {
    let get = |p: &str| usage.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: get("/input_tokens"),
        cached_input_tokens: get("/input_tokens_details/cached_tokens"),
        output_tokens: get("/output_tokens"),
        reasoning_tokens: get("/output_tokens_details/reasoning_tokens"),
    }
}

/// 2s, 4s, 8s, ... capped at 30s, plus up to 200ms of per-process jitter.
fn backoff(attempt: u32) -> Duration {
    let base = 1000u64.saturating_mul(1u64 << attempt.min(5));
    let jitter = (std::process::id() as u64 ^ attempt as u64) % 200;
    Duration::from_millis(base.min(30_000) + jitter)
}

/// Convenience for callers that need a hard error from a tool-argument string.
pub fn parse_arguments(arguments: &str) -> Result<Value> {
    if arguments.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(arguments)
        .with_context(|| format!("tool arguments are not valid JSON: {arguments}"))
        .and_then(|v: Value| {
            if v.is_object() {
                Ok(v)
            } else {
                Err(anyhow!("tool arguments must be a JSON object"))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_context_errors_trigger_compaction() {
        assert!(context_length_exceeded(
            &serde_json::json!({"code":"context_length_exceeded"})
        ));
        assert!(context_length_exceeded(&serde_json::json!({
            "code":400, "param":"input_tokens", "message":"This model's maximum context length is 1048576 tokens."
        })));
        for error in [
            serde_json::json!({"code":400,"param":"model","message":"unknown model"}),
            serde_json::json!({"param":"input_tokens","message":"invalid input"}),
            Value::Null,
        ] {
            assert!(!context_length_exceeded(&error));
        }
    }

    #[test]
    fn items_round_trip_and_keep_unknown_fields() {
        let json = serde_json::json!({
            "type": "reasoning", "id": "r1", "status": "completed",
            "content": [{"type": "reasoning_text", "text": "think"}],
            "encrypted_content": "abc"
        });
        let item: Item = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&item).unwrap(), json);

        let call = serde_json::json!({
            "type": "function_call", "id": "f1", "status": "completed",
            "call_id": "call_1", "name": "lean_goals", "arguments": "{\"line\": 3}"
        });
        let item: Item = serde_json::from_value(call.clone()).unwrap();
        assert_eq!(serde_json::to_value(&item).unwrap(), call);
        assert!(matches!(item, Item::FunctionCall { ref name, .. } if name == "lean_goals"));
    }

    #[test]
    fn usage_parses_cache_fields() {
        let u = parse_usage(&serde_json::json!({
            "input_tokens": 343, "input_tokens_details": {"cached_tokens": 128},
            "output_tokens": 112, "output_tokens_details": {"reasoning_tokens": 22}
        }));
        assert_eq!(u.cached_input_tokens, 128);
        assert_eq!(u.reasoning_tokens, 22);
    }

    #[test]
    fn requested_logprobs_must_be_present_and_numeric() {
        let mut c = Completion {
            items: vec![],
            usage: Usage::default(),
            incomplete: None,
            response: None,
        };
        assert!(c.require_logprobs().is_err());
        for scores in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!([{"logprob":null}]),
        ] {
            c.response = Some(
                serde_json::json!({"output":[{"content":[{"type":"output_text","text":"hello","logprobs":scores}]}]}),
            );
            assert!(c.require_logprobs().is_err());
        }
        c.response = Some(
            serde_json::json!({"output":[{"content":[{"type":"output_text","text":"hello","logprobs":[{"token":"hello","logprob":-0.2}]}]}]}),
        );
        assert!(c.require_logprobs().is_ok());
    }

    #[test]
    fn header_env_resolves_without_exposing_secret_in_spec() {
        let specs = vec!["X-Tapis-Token=TAPIS_TOKEN".to_owned()];
        let headers = headers_from_env_with(&specs, |name| match name {
            "TAPIS_TOKEN" => Some("secret-token".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(headers.get("X-Tapis-Token").unwrap(), "secret-token");
        assert!(headers.get("X-Tapis-Token").unwrap().is_sensitive());
    }

    #[test]
    fn header_env_rejects_bad_specs_missing_env_and_duplicates() {
        assert!(headers_from_env_with(&["bad".to_owned()], |_| Some("x".to_owned())).is_err());
        assert!(
            headers_from_env_with(&["X-Test=MISSING".to_owned()], |_| None).is_err()
        );
        assert!(
            headers_from_env_with(
                &["X-Test=A".to_owned(), "X-Test=B".to_owned()],
                |_| Some("x".to_owned())
            )
            .is_err()
        );
    }

    #[test]
    fn client_can_disable_bearer_auth_and_add_custom_header() {
        let mut headers = HeaderMap::new();
        let mut value = HeaderValue::from_static("secret-token");
        value.set_sensitive(true);
        headers.insert(HeaderName::from_static("x-tapis-token"), value);
        let client = Client::new_with_headers(
            ModelConfig {
                base_url: "http://example.invalid/v1".into(),
                api_key: "unused".into(),
                model: "test".into(),
                effort: "low".into(),
                max_output_tokens: 32,
                logprobs: false,
                idle_timeout: Duration::from_secs(1),
                max_attempts: 1,
            },
            false,
            headers,
        )
        .unwrap();
        let request = client
            .request("http://example.invalid/v1/responses", &serde_json::json!({}))
            .build()
            .unwrap();
        assert!(request.headers().get("authorization").is_none());
        assert_eq!(request.headers().get("x-tapis-token").unwrap(), "secret-token");
    }

    #[test]
    fn arguments_must_be_objects() {
        assert!(parse_arguments("").unwrap().is_object());
        assert!(parse_arguments("[1]").is_err());
        assert!(parse_arguments("{\"a\":1}").is_ok());
    }
}
