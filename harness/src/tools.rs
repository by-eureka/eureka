//! Tools the model can call inside one attempt. Beam tools come from the MCP
//! server; the rest are the minimum needed to edit the target, look around, run
//! commands in a sandbox, and declare completion.
//!
//! To add a tool, implement Tool and register it in `Harness::tools`.

use crate::beam::{Beam, ToolSpec};
use crate::lean_text::{command_line, tactic_block};
use crate::sandbox::Sandbox;
use crate::trace::Trace;
use anyhow::{Context, Result, anyhow, bail};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use worktrees::RelPath;

// Replies to the model are cut to MAX_OUTPUT. Files up to MAX_READ can still be
// read and written.
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_READ: u64 = 4 * 1024 * 1024;

/// Who runs an attempt: the lead verifies and publishes; a subagent only
/// returns its text to the lead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Lead,
    Subagent,
}

/// Per-attempt context shared by all tools.
/// `root` is the controller-side attempt. File tools use the active sandbox workspace.
pub struct Ctx {
    pub id: String,
    pub role: Role,
    pub root: PathBuf,
    pub target: RelPath,
    pub beam: Arc<Beam>,
    pub sandbox: Arc<Sandbox>,
    pub trace: Arc<Trace>,
    /// Set by `submit` or `finish`; the loop ends after the current call.
    pub submitted: AtomicBool,
    pub note: Mutex<Option<String>>,
    pub artifacts: PathBuf,
    pub stop: tokio_util::sync::CancellationToken,
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>>;
}

/// Function-tool definition in the Responses API shape.
pub fn definition(spec: &ToolSpec) -> Value {
    // Not strict: MCP schemas often use features strict mode rejects.
    json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.input_schema,
        "strict": false,
    })
}

fn spec(name: &str, description: &str, params: Value, read_only: bool) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: params,
        read_only,
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string argument `{key}`"))
}

fn truncate(mut text: String) -> String {
    // Keep the head and tail, cut on char boundaries (Lean output is full of Unicode).
    if text.len() > MAX_OUTPUT {
        let keep = MAX_OUTPUT / 2;
        let head = text.floor_char_boundary(keep);
        let tail = text.ceil_char_boundary(text.len() - keep);
        text = format!(
            "{}\n...[{} bytes omitted]...\n{}",
            &text[..head],
            tail - head,
            &text[tail..]
        );
    }
    text
}

// --- Beam -------------------------------------------------------------------

pub struct BeamTool(pub ToolSpec);

impl Tool for BeamTool {
    fn spec(&self) -> ToolSpec {
        self.0.clone()
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let args = normalize_probe(&self.0.name, args, ctx);
            ctx.beam.call(&self.0.name, &args).await.map(truncate)
        })
    }
}

/// Speculative-execution tools take one tactic node; group sequences so a
/// model's natural multi-line text runs instead of failing to parse.
fn normalize_probe(name: &str, mut args: Value, ctx: &Ctx) -> Value {
    const PROBES: &[&str] = &[
        "lean_run_at",
        "lean_run_at_handle",
        "lean_run_with",
        "lean_run_with_linear",
    ];
    if PROBES.contains(&name)
        && let Some(text) = args.get("text").and_then(Value::as_str)
    {
        let grouped = tactic_block(text);
        if grouped != text {
            ctx.trace.event(
                &ctx.id,
                "normalized",
                json!({"tool": name, "from": text, "to": grouped}),
            );
            args["text"] = Value::String(grouped.into_owned());
        }
    }
    args
}

// --- Files ------------------------------------------------------------------

pub struct ReadFile;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_file",
            "Read a file in the project. Lines are numbered from 0 to match Lean/LSP positions.",
            json!({"type": "object", "properties": {"path": {"type": "string", "description": "Path relative to the project root."}}, "required": ["path"], "additionalProperties": false}),
            true,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let rel = RelPath::new(str_arg(&args, "path")?)?;
            // read_beneath won't follow symlinks out of the root.
            let text = String::from_utf8(ctx.sandbox.read_file(&ctx.root, &rel, MAX_READ).await?)?;
            Ok(truncate(
                text.lines()
                    .enumerate()
                    .map(|(i, l)| format!("{i:4}| {l}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        })
    }
}

/// Writes the target file only. The shell can still change other files in
/// /work, but those never reach the verifier.
pub struct WriteFile;

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "write_file",
            "Replace the full contents of the target file. After writing, call lean_update before probing it again. Only the target file may be written; use lean_check for experiments.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"], "additionalProperties": false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let rel = str_arg(&args, "path")?;
            let content = str_arg(&args, "content")?;
            if rel != ctx.target.as_str() {
                bail!(
                    "only the target file `{}` may be written; use lean_check instead of scratch files",
                    ctx.target
                );
            }
            if content.len() > MAX_READ as usize {
                bail!("target exceeds {MAX_READ} bytes");
            }
            ctx.sandbox
                .write_file(&ctx.root, &ctx.target, content.as_bytes())
                .await?;
            Ok(format!("wrote {} bytes to {rel}", content.len()))
        })
    }
}

// --- Shell (inside the attempt's sandbox) ----------------------------------

pub struct Shell {
    pub timeout: Duration,
}

impl Tool for Shell {
    fn spec(&self) -> ToolSpec {
        spec(
            "shell",
            "Run a bash command in the project root inside the attempt's sandbox: only the project is writable, there is no network, and output is truncated. Useful for `lake build`, grep over dependency sources, or small scripts.",
            json!({"type": "object", "properties": {"command": {"type": "string"}, "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 1800}}, "required": ["command"], "additionalProperties": false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let command = str_arg(&args, "command")?;
            let timeout = args
                .get("timeout_seconds")
                .and_then(Value::as_u64)
                .map(Duration::from_secs)
                .unwrap_or(self.timeout);
            if !(1..=1800).contains(&timeout.as_secs()) {
                bail!("timeout_seconds must be 1..1800");
            }
            match ctx.sandbox.shell(command, timeout).await {
                // A failing command is Ok, with the exit code in the text. Err
                // means the sandbox itself broke, so the attempt is over.
                Ok(output) => Ok(output),
                Err(error) => {
                    ctx.stop.cancel();
                    Err(error)
                }
            }
        })
    }
}

// --- lean_check: a command probe without a scratch file ---------------------

pub struct LeanCheck;

impl Tool for LeanCheck {
    fn spec(&self) -> ToolSpec {
        spec(
            "lean_check",
            "Run Lean commands speculatively in the real context of the target file (same imports and definitions) and return Lean's messages, in milliseconds. Use it to check lemma statements (`#check @Nat.gcd_comm`), search (`exact?` inside an `example`), evaluate (`#eval`), or test a whole `example : ... := by ...`. Each command must be a single top-level command (one `#check` checks one term); pass several in `commands` and they run concurrently. The command runs just before the last declaration of the file, so every earlier definition and lemma is in scope. Nothing is written to disk. Never create scratch files or run `lake env lean` for this.",
            json!({"type": "object", "properties": {
                "text": {"type": "string", "description": "One top-level Lean command."},
                "commands": {"type": "array", "items": {"type": "string"}, "maxItems": 32, "description": "Several independent top-level commands, run concurrently."}
            }, "additionalProperties": false}),
            true,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let mut commands: Vec<String> = args
                .get("commands")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(text) = args.get("text").and_then(Value::as_str) {
                commands.insert(0, text.to_owned());
            }
            if commands.is_empty() {
                bail!("pass `text` or `commands`");
            }
            if commands.len() > 32 {
                bail!("at most 32 commands per lean_check");
            }
            let path = ctx.target.as_str();
            let source = String::from_utf8(
                ctx.sandbox
                    .read_file(&ctx.root, &ctx.target, MAX_READ)
                    .await?,
            )?;
            // Command-mode probes need a command start. The start of the last
            // top-level declaration sees everything declared before it.
            let line = command_line(&source);
            let update = ctx.beam.call("lean_update", &json!({"path": path})).await?;
            let version = serde_json::from_str::<Value>(&update)
                .ok()
                .and_then(|v| v.get("version")?.as_u64())
                .ok_or_else(|| anyhow!("lean_update failed: {update}"))?;
            let probes = commands.iter().map(|text| async move {
                let out = ctx
                    .beam
                    .call("lean_run_at",
                        &json!({"path": path, "version": version, "line": line, "character": 0, "text": text}),
                    )
                    .await
                    .with_context(|| format!("lean_check command {text:?}"))?;
                // Keep only what the model needs: the command, success flag and messages.
                Ok::<_, anyhow::Error>(match serde_json::from_str::<Value>(&out) {
                    Ok(v) if v.get("messages").is_some() => {
                        json!({"command": text, "success": v["success"], "messages": v["messages"]})
                    }
                    _ => json!({"command": text, "raw": truncate(out)}),
                })
            });
            // Drain every probe before returning an infrastructure error. Ordinary
            // Lean failures remain `success: false` results for the model to inspect.
            let results: Vec<Value> = futures::future::join_all(probes)
                .await
                .into_iter()
                .collect::<Result<_>>()?;
            Ok(truncate(serde_json::to_string(&results)?))
        })
    }
}

// --- Submit -----------------------------------------------------------------

/// Only sets a flag. The check happens in search.rs after the loop ends, and
/// nothing here stops the model from submitting a file that still has sorries.
pub struct Submit;

impl Tool for Submit {
    fn spec(&self) -> ToolSpec {
        spec(
            "submit",
            "Submit the current target file for verification and end the session. Call only after lean_sync reports no errors and lean_todo reports no sorries in the target.",
            json!({"type": "object", "properties": {"note": {"type": "string", "description": "Short summary of the approach."}}, "additionalProperties": false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            *ctx.note.lock().unwrap() = args.get("note").and_then(Value::as_str).map(str::to_owned);
            ctx.submitted.store(true, Ordering::SeqCst);
            ctx.trace
                .event(&ctx.id, "submit", json!({"note": args.get("note")}));
            Ok("submitted".into())
        })
    }
}

// --- Finish (subagents) -----------------------------------------------------

/// The helper's `submit`. A note is required, and the file goes back to the
/// lead without being checked.
pub struct Finish;

impl Tool for Finish {
    fn spec(&self) -> ToolSpec {
        spec(
            "finish",
            "End your session and return the current target file to the lead agent. Call when your assigned part is proven (lean_sync shows no errors in it) or when you cannot make further progress; say which in the note. Nothing is verified or published at this level.",
            json!({"type": "object", "properties": {"note": {"type": "string", "description": "What was proven, what remains, and anything the lead should know."}}, "required": ["note"], "additionalProperties": false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            *ctx.note.lock().unwrap() = Some(str_arg(&args, "note")?.to_owned());
            ctx.submitted.store(true, Ordering::SeqCst);
            ctx.trace
                .event(&ctx.id, "finish", json!({"note": args.get("note")}));
            Ok("finished".into())
        })
    }
}

/// Namespacing permits additional MCP tools without changing Beam or the loop.
pub struct ExtraTool {
    pub spec: ToolSpec,
    pub original_name: String,
    /// Shared by every tool from the same server.
    pub server: Arc<Beam>,
}
impl Tool for ExtraTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn call<'a>(&'a self, args: Value, _ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            self.server
                .call(&self.original_name, &args)
                .await
                .map(truncate)
        })
    }
}
