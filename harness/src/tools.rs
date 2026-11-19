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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use worktrees::RelPath;

// Replies to the model are cut to MAX_OUTPUT. Files up to MAX_READ can still be
// read and written.
const MAX_OUTPUT: usize = 64 * 1024;
use crate::proof_files::MAX_FILE as MAX_READ;

/// Who runs an attempt: the lead verifies and publishes; a subagent only
/// returns its text to the lead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Lead,
    Subagent,
}

/// Terminal actions carry the note and outcome together so a report can
/// never be mistaken for a verification request.
pub enum Ending {
    Submit(Option<String>),
    Handoff(String),
    Report(String),
}

impl Ending {
    pub fn note(self) -> Option<String> {
        match self {
            Self::Submit(note) => note,
            Self::Handoff(note) | Self::Report(note) => Some(note),
        }
    }
}

/// Per-attempt context shared by all tools.
/// `root` is the controller-side attempt. File tools use the active sandbox workspace.
pub struct Ctx {
    pub id: String,
    pub role: Role,
    pub root: PathBuf,
    pub target: RelPath,
    pub proof_files: Vec<RelPath>,
    pub beam: Arc<Beam>,
    pub sandbox: Arc<Sandbox>,
    pub trace: Arc<Trace>,
    pub ending: Mutex<Option<Ending>>,
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

pub struct ReadGuide(pub Arc<crate::workflow::Guides>);

impl Tool for ReadGuide {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_guide",
            "Read a Lean workflow or a reference from this run's Beam skill.",
            json!({"type":"object","properties":{"name":{"type":"string","enum":self.0.names().collect::<Vec<_>>()}},"required":["name"],"additionalProperties":false}),
            true,
        )
    }
    fn call<'a>(&'a self, args: Value, _ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move { Ok(self.0.read(str_arg(&args, "name")?)?.to_owned()) })
    }
}

pub struct ReadFile;

fn file_page(text: &str, offset: &Value) -> Result<String> {
    let start = offset
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .context("start_byte must be a nonnegative integer")?;
    anyhow::ensure!(
        text.is_char_boundary(start),
        "start_byte must be a UTF-8 boundary within the file"
    );
    // Leave room for the continuation marker, including when a line spans multiple pages.
    let end = text.floor_char_boundary(start.saturating_add(MAX_OUTPUT - 128).min(text.len()));
    let page = &text[start..end];
    if end == text.len() {
        Ok(format!("[end of file]\n{page}"))
    } else {
        Ok(format!("[next start_byte={end}]\n{page}"))
    }
}

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_file",
            "Read a project file with 0-based line numbers. For a long source, set start_byte to 0, then follow each next start_byte to read consecutive pages of raw text.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Path relative to the project root."},
                "start_byte": {"type": "integer", "minimum": 0, "description": "Byte offset for a plain-text page, starting at 0."}
            }, "required": ["path"], "additionalProperties": false}),
            true,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let rel = RelPath::new(str_arg(&args, "path")?)?;
            // read_beneath won't follow symlinks out of the root.
            let text = String::from_utf8(ctx.sandbox.read_file(&ctx.root, &rel, MAX_READ).await?)?;
            if let Some(offset) = args.get("start_byte") {
                return file_page(&text, offset);
            }
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

fn editable<'a>(ctx: &'a Ctx, name: &str) -> Result<&'a RelPath> {
    ctx.proof_files
        .iter()
        .find(|p| p.as_str() == name)
        .ok_or_else(|| anyhow!("path is outside the declared proof files: {name}"))
}

pub struct ApplyPatch;
impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        spec(
            "apply_patch",
            "Edit one or several declared proof files using Codex patch syntax. Use *** Begin Patch, one or more *** Update File: relative/path.lean sections, @@ then lines prefixed by space (context), - (remove), + (add), and *** End Patch. Call lean_update on changed paths.",
            json!({"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"],"additionalProperties":false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let patch = str_arg(&args, "patch")?;
            anyhow::ensure!(patch.len() <= MAX_READ as usize, "patch exceeds size limit");
            let mut edits = std::collections::BTreeMap::new();
            // Resolve and match every hunk before the first write; repeated sections
            // for one file see the result of the preceding section.
            for update in crate::codex_patch::parse(patch)? {
                let path = editable(ctx, &update.path)?;
                let (original, current) = match edits.remove(path) {
                    Some(pair) => pair,
                    None => {
                        let text = String::from_utf8(
                            ctx.sandbox.read_file(&ctx.root, path, MAX_READ).await?,
                        )?;
                        (text.clone(), text)
                    }
                };
                let changed = update.apply(&current)?;
                anyhow::ensure!(
                    changed.len() <= MAX_READ as usize,
                    "{path} exceeds size limit"
                );
                edits.insert(path, (original, changed));
            }
            for (path, (_, content)) in &edits {
                if let Err(error) = ctx
                    .sandbox
                    .write_file(&ctx.root, path, content.as_bytes())
                    .await
                {
                    // A remote write may succeed before its acknowledgement is lost.
                    // Include the failing path when restoring the pre-patch contents.
                    for (written, (original, _)) in edits.range::<&RelPath, _>(..=*path).rev() {
                        if let Err(rollback) = ctx
                            .sandbox
                            .write_file(&ctx.root, written, original.as_bytes())
                            .await
                        {
                            ctx.stop.cancel();
                            bail!(
                                "write failed at {path}: {error}; cannot restore {written}: {rollback}; attempt stopped"
                            );
                        }
                    }
                    return Err(
                        error.context(format!("patch reverted after write failed at {path}"))
                    );
                }
            }
            Ok(format!(
                "Updated {}",
                edits
                    .keys()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }
}

pub struct WriteFile;

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "write_file",
            "Replace the full contents of a declared proof file. After writing, call lean_update before probing it again; use lean_check for experiments.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"], "additionalProperties": false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let rel = str_arg(&args, "path")?;
            let content = str_arg(&args, "content")?;
            let path = editable(ctx, rel)?;
            if content.len() > MAX_READ as usize {
                bail!("target exceeds {MAX_READ} bytes");
            }
            ctx.sandbox
                .write_file(&ctx.root, path, content.as_bytes())
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
                "path": {"type": "string", "description": "Declared proof file; defaults to the primary target."},
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
            let file = match args.get("path") {
                Some(_) => editable(ctx, str_arg(&args, "path")?)?,
                None => &ctx.target,
            };
            let path = file.as_str();
            let source =
                String::from_utf8(ctx.sandbox.read_file(&ctx.root, file, MAX_READ).await?)?;
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
            "Submit all declared proof files for verification and end the session. Call only after lean_sync reports no errors and lean_todo reports no remaining proof obligations.",
            json!({"type": "object", "properties": {"note": {"type": "string", "description": "Short summary of the approach."}}, "additionalProperties": false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            *ctx.ending.lock().unwrap() = Some(Ending::Submit(
                args.get("note").and_then(Value::as_str).map(str::to_owned),
            ));
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
            "End your session and return the declared proof files to the lead agent. Call when your assigned part is proven (lean_sync shows no errors in it) or when you cannot make further progress; say which in the note. Nothing is verified or published at this level.",
            json!({"type": "object", "properties": {"note": {"type": "string", "description": "What was proven, what remains, and anything the lead should know."}}, "required": ["note"], "additionalProperties": false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            *ctx.ending.lock().unwrap() = Some(Ending::Handoff(str_arg(&args, "note")?.to_owned()));
            ctx.trace
                .event(&ctx.id, "finish", json!({"note": args.get("note")}));
            Ok("finished".into())
        })
    }
}

/// A report ends the lead's workflow without submitting a proof.
pub struct FinishReport(pub crate::workflow::Workflow);
impl Tool for FinishReport {
    fn spec(&self) -> ToolSpec {
        spec(
            "finish_report",
            "Finish with a written report and the saved candidate. Include the finding, checked evidence and any remaining obligation or decision.",
            json!({"type":"object","properties":{"note":{"type":"string","minLength":1}},"required":["note"],"additionalProperties":false}),
            false,
        )
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let note = str_arg(&args, "note")?;
            anyhow::ensure!(!note.trim().is_empty(), "the report must contain a finding");
            if self.0.drafts_source() {
                for file in &ctx.proof_files {
                    let sync = ctx
                        .beam
                        .call("lean_sync", &json!({"path":file.as_str()}))
                        .await?;
                    let state: Value = serde_json::from_str(&sync)?;
                    anyhow::ensure!(
                        state.pointer("/readiness/save_ready") == Some(&json!(true)),
                        "repair the draft in {file} before reporting: {sync}"
                    );
                }
            }
            *ctx.ending.lock().unwrap() = Some(Ending::Report(note.to_owned()));
            ctx.trace
                .event(&ctx.id, "finish_report", json!({"note":note}));
            Ok("reported".into())
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
