//! Runs the leads and their helpers, verifies submissions, publishes the winner.
//!
//! `run_attempt` takes every attempt through the same steps: copy the template,
//! write the seed, start and attach a sandbox, run the agent, cancel and join
//! its helpers, stop the sandbox, freeze and save the declared proof files, and
//! verify it if a lead submitted. The file is frozen only after every writer
//! has stopped, and saved before the verifier runs. Helpers never publish; the
//! lead gets their proof sources back. Attempts save their files under
//! `<pool>/attempts/<id>/`, which no sandbox can see.
use crate::agent::{self, AgentConfig, Outcome};
use crate::beam::{Beam, ExtraServer, ToolSpec};
use crate::proof_files::{self, MAX_FILE, Sources};
use crate::responses::{Client, Usage};
use crate::runtime;
use crate::sandbox::{Sandbox, SandboxSpec, WORK};
use crate::tools::{
    ApplyPatch, BeamTool, Ctx, ExtraTool, Finish, FinishReport, LeanCheck, ReadFile, ReadGuide,
    Role, Shell, Submit, Tool, WriteFile,
};
use crate::trace::{Trace, artifact};
use crate::workflow::{Guides, Workflow};
use anyhow::{Context, Result, anyhow, bail};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use worktrees::{Error, Frozen, PublishOutcome, Published, Task, Verified};

/// Everything the attempts of one run share. Per-attempt state is in tools::Ctx.
pub struct Harness {
    pub task: Task,
    pub client: Client,
    pub sandbox: SandboxSpec,
    pub trace: Arc<Trace>,
    pub agent: AgentConfig,
    pub instructions: String,
    pub workflow: Workflow,
    pub guides: Arc<Guides>,
    pub shell: bool,
    pub shell_timeout: Duration,
    pub stop: CancellationToken,
    /// Total across all leads and helpers.
    pub usage: Mutex<Usage>,
    pub artifacts: PathBuf,
    pub next_id: AtomicU64,
    pub extra_servers: Vec<ExtraServer>,
}

/// How an attempt ended. `termination` is why the conversation stopped;
/// `verdict` is what happened to the saved file afterwards.
pub struct AttemptResult {
    pub workflow: Workflow,
    pub id: String,
    pub label: String,
    pub submitted: bool,
    pub turns: usize,
    pub usage: Usage,
    pub verdict: Verdict,
    pub termination: String,
    pub note: Option<String>,
    pub last_message: Option<String>,
    pub target_text: Option<String>,
    pub files: Sources,
    pub candidate: Option<PathBuf>,
    pub artifacts: PathBuf,
}

/// `Finished` means a helper called `finish`. Only `Verified` can be published.
pub enum Verdict {
    NotSubmitted,
    Reported,
    Finished,
    Verified(Verified),
    Rejected(String),
    Failed(String),
}

impl AttemptResult {
    /// Used for result.json, the attempt_end event and child handoffs.
    /// trace_report.py reads these fields.
    pub fn report(&self) -> Value {
        let (status, detail) = match &self.verdict {
            Verdict::Reported => ("reported", ""),
            Verdict::NotSubmitted => ("not_submitted", ""),
            Verdict::Finished => ("finished", ""),
            Verdict::Verified(v) => ("verified", v.report()),
            Verdict::Rejected(s) => ("rejected", s.as_str()),
            Verdict::Failed(s) => ("failed", s.as_str()),
        };
        json!({"id": self.id, "label": self.label, "workflow": self.workflow.name(), "status": status, "detail": detail,
            "submitted": self.submitted, "turns": self.turns, "usage": self.usage,
            "termination": self.termination, "note": self.note, "last_message": self.last_message,
            "candidate": self.candidate})
    }
}

/// A lead owns one bounded set. Cancel cooperatively, then join; never abort
/// futures that may still own containers. Finished results remain on disk.
struct Children {
    running: JoinSet<Result<AttemptResult>>,
    stop: CancellationToken,
}
impl Children {
    async fn close(&mut self) -> Result<()> {
        self.stop.cancel();
        let mut error = None;
        while let Some(result) = self.running.join_next().await {
            if let Err(e) = result.map_err(anyhow::Error::from).and_then(|r| r) {
                error = Some(e);
            }
        }
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Harness {
    fn id(&self) -> String {
        // Unique within the run; labels come from the model and can repeat.
        format!(
            "attempt-{:06}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn tools(
        self: &Arc<Self>,
        beam: &Beam,
        role: Role,
        children: Arc<tokio::sync::Mutex<Children>>,
    ) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = beam
            .tools()
            .iter()
            .map(|t| Arc::new(BeamTool(t.clone())) as Arc<dyn Tool>)
            .collect();
        tools.extend([
            Arc::new(LeanCheck) as Arc<dyn Tool>,
            Arc::new(ReadFile),
            Arc::new(ReadGuide(self.guides.clone())),
        ]);
        if !self.workflow.inspects_source() {
            tools.push(Arc::new(ApplyPatch));
            tools.push(Arc::new(WriteFile));
        }
        if self.shell && self.workflow.accepts_proof() {
            tools.push(Arc::new(Shell {
                timeout: self.shell_timeout,
            }));
        }
        if role == Role::Lead && self.workflow.accepts_proof() {
            // Only leads can spawn, so helpers never have helpers of their own.
            tools.push(Arc::new(Subagents {
                harness: Arc::downgrade(self),
                children: children.clone(),
                wait: false,
            }));
            tools.push(Arc::new(Subagents {
                harness: Arc::downgrade(self),
                children,
                wait: true,
            }));
        }
        if role == Role::Subagent {
            tools.push(Arc::new(Finish));
        } else {
            if self.workflow.accepts_proof() {
                tools.push(Arc::new(Submit));
            }
            if self.workflow.accepts_report() {
                tools.push(Arc::new(FinishReport(self.workflow)));
            }
        }
        tools
    }

    /// Run one attempt from setup to verification, cleaning up even on failure.
    /// Boxed because Subagents calls it recursively.
    pub fn run_attempt<'a>(
        self: &'a Arc<Self>,
        id: String,
        label: String,
        prompt: String,
        seed: Option<Sources>,
        role: Role,
        stop: CancellationToken,
    ) -> BoxFuture<'a, Result<AttemptResult>> {
        Box::pin(async move {
            let artifacts = self.artifacts.join(&id);
            std::fs::create_dir(&artifacts)?;
            self.trace.event(
                &id,
                "attempt_start",
                json!({"label": label, "role": format!("{role:?}"), "artifacts": artifacts}),
            );
            let task = self.task.clone();
            let attempt_id = id.clone();
            // worktrees is synchronous and copies the whole project.
            let attempt = tokio::task::spawn_blocking(move || -> Result<_> {
                let attempt = task.attempt(attempt_id)?;
                if let Some(seed) = seed {
                    for (path, text) in seed {
                        anyhow::ensure!(
                            task.targets().any(|allowed| allowed == &path),
                            "invalid seed path {path}"
                        );
                        worktrees::replace_beneath(&attempt.root(), &path, text.as_bytes())?;
                    }
                }
                Ok(attempt)
            })
            .await??;
            let root = attempt.root();
            // Lean probes may perform IO. A source review only succeeds if its
            // frozen files still equal the operator's starting candidate.
            let mut baseline = Vec::new();
            if self.workflow.inspects_source() {
                for path in self.task.targets() {
                    baseline.push((
                        path.clone(),
                        worktrees::read_beneath(&root, path, MAX_FILE)?,
                    ));
                }
            }
            let children = Arc::new(tokio::sync::Mutex::new(Children {
                running: JoinSet::new(),
                stop: stop.child_token(),
            }));
            let mut sandbox = None;
            let mut note = None;
            // Errors in here still fall through to the cleanup below.
            let mut outcome = match async {
                let s = Arc::new(Sandbox::create(&self.sandbox, &id, &root).await?);
                sandbox = Some(s.clone());
                if root.join("proof-files.json").is_file() {
                    // Cold workspaces have no local .oleans for Beam to import.
                    // A seed with a proof error can still be repaired by the model.
                    let modules: Vec<_> = self
                        .task
                        .targets()
                        .map(|p| {
                            format!(
                                "+{}",
                                p.as_str()
                                    .strip_suffix(".lean")
                                    .unwrap_or(p.as_str())
                                    .replace('/', ".")
                            )
                        })
                        .collect();
                    let mut args = vec!["lake", "build"];
                    args.extend(modules.iter().map(String::as_str));
                    let build = s.timed_command(&args, self.shell_timeout).await?;
                    self.trace
                        .event(&id, "initial_build", json!({"output": build}));
                }
                let beam = tokio::time::timeout(
                    Duration::from_secs(30),
                    Beam::spawn(runtime::mcp(&s), WORK),
                )
                .await
                .context("Beam startup timeout")??;
                let beam = Arc::new(beam);
                // Resolve imports before the first paid request. Source diagnostics
                // remain available for repair; runtime/setup failures stop startup.
                if let Err(error) = beam
                    .call("lean_sync", &json!({"path": self.task.target().as_str()}))
                    .await
                {
                    if self.workflow != Workflow::Diagnose {
                        return Err(error.context("initial Lean workspace setup"));
                    }
                    self.trace.event(
                        &id,
                        "diagnose_initial_sync",
                        json!({"error":format!("{error:#}")}),
                    );
                }
                let ctx = Ctx {
                    id: id.clone(),
                    role,
                    root: root.clone(),
                    target: self.task.target().clone(),
                    proof_files: self.task.targets().cloned().collect(),
                    beam: beam.clone(),
                    sandbox: s.clone(),
                    trace: self.trace.clone(),
                    ending: Mutex::new(None),
                    artifacts: artifacts.clone(),
                    stop: stop.clone(),
                };
                let mut tools = self.tools(&beam, role, children.clone());
                for config in &self.extra_servers {
                    let args: Vec<&str> = config.args.iter().map(String::as_str).collect();
                    let server = Arc::new(
                        tokio::time::timeout(
                            Duration::from_secs(30),
                            Beam::connect(s.command(&config.command, &args), None),
                        )
                        .await
                        .context("extra MCP startup timeout")??,
                    );
                    for spec in server.tools() {
                        if self.workflow.inspects_source() && !spec.read_only {
                            continue;
                        }
                        let mut named = spec.clone();
                        named.name = format!("mcp_{}_{}", config.name, spec.name);
                        anyhow::ensure!(
                            !tools.iter().any(|t| t.spec().name == named.name),
                            "duplicate MCP tool {}",
                            named.name
                        );
                        tools.push(Arc::new(ExtraTool {
                            spec: named,
                            original_name: spec.name.clone(),
                            server: server.clone(),
                        }));
                    }
                }
                let instructions = format!("{}\n\n{}", self.instructions, beam.instructions());
                let outcome = agent::run(
                    &self.client,
                    &self.agent,
                    &instructions,
                    &prompt,
                    &tools,
                    &ctx,
                )
                .await;
                note = ctx
                    .ending
                    .lock()
                    .unwrap()
                    .take()
                    .and_then(crate::tools::Ending::note);
                Ok::<_, anyhow::Error>(outcome)
            }
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => Outcome {
                    error: Some(format!("startup: {error:#}")),
                    termination: "startup_error".into(),
                    ..Default::default()
                },
            };
            if let Err(error) = children.lock().await.close().await {
                outcome.error = Some(format!("child cleanup: {error:#}"));
            }
            let capture = if let Some(sandbox) = &sandbox {
                async {
                    for path in self.task.targets() {
                        sandbox.collect_target(&root, path).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await
            } else {
                Ok(())
            };
            if let Some(sandbox) = sandbox
                && let Err(error) = sandbox.close().await
            {
                if self.sandbox.tapis.is_some() {
                    // A pod cannot write the private host copy. Verify the collected files and
                    // let the run's final prune retry deletion, even if verification fails.
                    self.trace.event(
                        &id,
                        "pod_cleanup_deferred",
                        json!({"error": format!("{error:#}")}),
                    );
                } else {
                    // A live Docker writer may still hold the host bind mount.
                    std::mem::forget(attempt);
                    artifact(
                        &artifacts.join("result.json"),
                        &serde_json::to_vec_pretty(
                            &json!({"status": "cleanup_failed", "root": root, "error": format!("{error:#}")}),
                        )?,
                    )?;
                    bail!(
                        "{id}: cannot stop sandbox; retained {}: {error:#}",
                        root.display()
                    );
                }
            }
            self.usage.lock().unwrap().add(&outcome.usage);
            // Freeze the declared files once the captured host copy has no sandbox writers.
            let frozen = tokio::task::spawn_blocking(move || {
                capture?;
                let frozen = attempt.freeze().context("freeze target")?;
                for (path, bytes) in baseline {
                    anyhow::ensure!(
                        frozen
                            .files()
                            .get(&path)
                            .is_some_and(|actual| actual.as_ref() == bytes.as_slice()),
                        "source changed during read-only workflow: {path}"
                    );
                }
                Ok::<_, anyhow::Error>(frozen)
            })
            .await?;
            let mut result = AttemptResult {
                id: id.clone(),
                workflow: self.workflow,
                label,
                submitted: outcome.submitted,
                turns: outcome.turns,
                usage: outcome.usage,
                verdict: Verdict::NotSubmitted,
                termination: outcome.termination,
                note,
                last_message: outcome.last_message,
                target_text: None,
                files: Sources::new(),
                candidate: None,
                artifacts,
            };
            let frozen = match frozen {
                Ok(frozen) => {
                    result.candidate = Some(proof_files::save(&frozen, &result.artifacts)?);
                    if role == Role::Subagent && frozen.files().len() > 1 {
                        for (path, bytes) in frozen.files() {
                            result
                                .files
                                .insert(path.clone(), String::from_utf8(bytes.to_vec())?);
                        }
                    }
                    result.target_text = Some(String::from_utf8(frozen.bytes().to_vec())?);
                    Some(frozen)
                }
                Err(error) => {
                    outcome.error = Some(format!("capture: {error}"));
                    None
                }
            };
            // Persist progress before any verifier process can fail.
            let mut pending = result.report();
            pending["status"] = json!(if outcome.submitted && role == Role::Lead {
                "verifying"
            } else {
                "captured"
            });
            artifact(
                &result.artifacts.join("result.json"),
                &serde_json::to_vec_pretty(&pending)?,
            )?;
            result.verdict = match (outcome.error, frozen) {
                // Something broke (model, cleanup, capture). That says nothing
                // about the proof, so it's Failed rather than Rejected.
                (Some(error), _) => Verdict::Failed(error),
                (None, Some(_)) if result.termination == "reported" => Verdict::Reported,
                (None, Some(frozen)) if result.submitted && role == Role::Lead => {
                    self.trace.event(
                        &id,
                        "verification_start",
                        json!({"candidate": result.candidate}),
                    );
                    match verify_candidate(frozen, &result.artifacts).await {
                        Ok(v) => Verdict::Verified(v),
                        Err(Error::Rejected(report)) => Verdict::Rejected(report),
                        Err(error) => Verdict::Failed(format!("verify: {error}")),
                    }
                }
                (None, _) if result.submitted => Verdict::Finished,
                _ => Verdict::NotSubmitted,
            };
            let report = result.report();
            artifact(
                &result.artifacts.join("result.json"),
                &serde_json::to_vec_pretty(&report)?,
            )?;
            self.trace.event(&id, "attempt_end", report);
            if let Verdict::Failed(error) = &result.verdict {
                eprintln!("{id}: {error}");
            }
            self.trace.check()?;
            Ok(result)
        })
    }

    /// Publish a verified proof and stop all other attempts. If another lead
    /// already won, its proof is returned instead.
    pub async fn publish(self: &Arc<Self>, verified: Verified) -> Result<Published> {
        let task = self.task.clone();
        let outcome = tokio::task::spawn_blocking(move || task.publish(verified)).await?;
        let published = match outcome {
            Ok(PublishOutcome::Committed(p)) | Ok(PublishOutcome::AlreadyWon(p)) => p,
            Err(Error::CommitUncertain { published, source }) => {
                // The proof is published but may not have reached disk. It's
                // still the winner; just record the problem.
                self.trace.event(
                    "harness",
                    "commit_uncertain",
                    json!({"error": source.to_string()}),
                );
                published
            }
            Err(e) => return Err(e.into()),
        };
        self.stop.cancel();
        Ok(published)
    }

    /// Run `n` leads in parallel and return the published proof, if any. Waits
    /// for every lead to finish cleaning up before pruning containers.
    pub async fn independent(
        self: &Arc<Self>,
        n: usize,
        prompt: &str,
        seed: Option<&Sources>,
    ) -> Result<(Option<Published>, Vec<PathBuf>)> {
        let mut running = JoinSet::new();
        for i in 1..=n {
            let h = self.clone();
            let id = self.id();
            let prompt = prompt.to_owned();
            let seed = seed.cloned();
            running.spawn(async move {
                h.run_attempt(
                    id,
                    format!("agent-{i}"),
                    prompt,
                    seed,
                    Role::Lead,
                    h.stop.child_token(),
                )
                .await
            });
        }
        let mut winner = None;
        let mut reports = Vec::new();
        let mut error = None;
        while let Some(joined) = running.join_next().await {
            match joined.map_err(anyhow::Error::from).and_then(|r| r) {
                Ok(result) => {
                    eprintln!(
                        "{} ({}): {} after {} turns ({} in / {} cached / {} out tokens)",
                        result.label,
                        result.id,
                        result.report()["status"],
                        result.turns,
                        result.usage.input_tokens,
                        result.usage.cached_input_tokens,
                        result.usage.output_tokens
                    );
                    if matches!(result.verdict, Verdict::Reported) {
                        reports.push(result.artifacts.join("result.json"));
                    }
                    if let Verdict::Verified(v) = result.verdict
                        && winner.is_none()
                    {
                        match self.publish(v).await {
                            Ok(p) => winner = Some(p),
                            Err(e) => {
                                error = Some(e);
                                self.stop.cancel();
                            }
                        }
                    }
                }
                Err(e) => {
                    error = Some(e);
                    self.stop.cancel();
                }
            }
        }
        // Also catches containers left by a panicking task, after every task joins.
        if let Some(tapis) = &self.sandbox.tapis {
            tapis.prune().await?;
        } else {
            crate::sandbox::prune(&self.sandbox.run_id).await?;
        }
        self.trace.event(
            "harness",
            "run_end",
            json!({"usage": self.usage.lock().unwrap().clone()}),
        );
        if let Some(error) = error {
            return Err(error);
        }
        reports.sort();
        Ok((winner, reports))
    }
}

/// Retry only operational verifier failures, once, using the identical saved bytes
/// in another fresh trusted tree. Both reports survive regardless of the verdict.
pub async fn verify_candidate(frozen: Frozen, dir: &Path) -> Result<Verified, Error> {
    let mut reports = String::new();
    for round in 1..=2 {
        // `verify` consumes `frozen`, so keep the bytes for a retry.
        let frozen = frozen.clone();
        let result = tokio::task::spawn_blocking(move || frozen.verify())
            .await
            .map_err(|e| Error::VerifierFailed(std::io::Error::other(e.to_string())))?;
        let report = match &result {
            Ok(v) => v.report().to_owned(),
            Err(e) => e.to_string(),
        };
        reports.push_str(&format!("Verification {round}\n{report}\n"));
        artifact(&dir.join("verification.txt"), reports.as_bytes())
            .map_err(|e| Error::VerifierFailed(std::io::Error::other(e.to_string())))?;
        if round == 2 || !matches!(result, Err(Error::VerifierFailed(_))) {
            return result;
        }
    }
    unreachable!()
}

struct Subagents {
    // Weak, because Harness indirectly owns this tool.
    harness: std::sync::Weak<Harness>,
    children: Arc<tokio::sync::Mutex<Children>>,
    wait: bool,
}
impl Tool for Subagents {
    fn spec(&self) -> ToolSpec {
        if self.wait {
            return ToolSpec { name: "wait_subagents".into(), description: "Wait up to 30 seconds for a child to finish; return newly completed results (including notes and proof file contents), plus the remaining count. You may continue your own work and call again. Each result is delivered once and also saved as an artifact.".into(), input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}), read_only: false };
        }
        ToolSpec { name: "spawn_subagents".into(), description: "Start subagents in private projects seeded with your current proof files. First checks that the seed elaborates (sorries allowed). Returns IDs immediately; use wait_subagents to collect results while continuing your own work. At most 8 uncollected children. Submit/termination cancels and joins remaining children; collect their work first.".into(), input_schema: json!({"type": "object", "properties": {"tasks": {"type": "array", "minItems": 1, "maxItems": 8, "items": {"type": "object", "properties": {"label": {"type": "string"}, "instructions": {"type": "string"}}, "required": ["label", "instructions"], "additionalProperties": false}}}, "required": ["tasks"], "additionalProperties": false}), read_only: false }
    }
    fn call<'a>(&'a self, args: Value, ctx: &'a Ctx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            if self.wait {
                let mut completed = Vec::new();
                if let Ok(Some(joined)) =
                    tokio::time::timeout(Duration::from_secs(30), children.running.join_next())
                        .await
                {
                    completed.push(child_report(joined));
                }
                while let Some(joined) = children.running.try_join_next() {
                    // Also collect any others that are already done.
                    completed.push(child_report(joined));
                }
                return Ok(serde_json::to_string(
                    &json!({"completed": completed, "remaining": children.running.len()}),
                )?);
            }
            let harness = self
                .harness
                .upgrade()
                .ok_or_else(|| anyhow!("harness is gone"))?;
            let tasks = args
                .get("tasks")
                .and_then(Value::as_array)
                .context("missing tasks")?;
            if tasks.is_empty() || tasks.len() + children.running.len() > 8 {
                // Finished children count until wait_subagents collects them.
                bail!("1..8 tasks; collect previous children before exceeding 8");
            }
            let tasks: Vec<(String, String)> = tasks
                .iter()
                .map(|t| {
                    Ok((
                        t["label"].as_str().context("missing label")?.to_owned(),
                        t["instructions"]
                            .as_str()
                            .context("missing instructions")?
                            .to_owned(),
                    ))
                })
                .collect::<Result<_>>()?;
            // Don't hand every helper the same broken file. Sorries are fine.
            let mut seed = Sources::new();
            for path in &ctx.proof_files {
                let sync = ctx.beam.call("lean_sync", &json!({"path": path.as_str(), "diagnostic_scope": "all", "diagnostics_in_result": true})).await?;
                let diagnostics: Value = serde_json::from_str(&sync)
                    .with_context(|| format!("seed sync failed: {sync}"))?;
                if diagnostics
                    .pointer("/diagnostics/counts/error")
                    .and_then(Value::as_u64)
                    != Some(0)
                    || diagnostics.pointer("/readiness/save_ready") != Some(&json!(true))
                {
                    bail!(
                        "repair the seed in {path} before spawning; sorries are allowed, elaboration errors are not: {sync}"
                    );
                }
                let bytes = ctx.sandbox.read_file(&ctx.root, path, MAX_FILE).await?;
                seed.insert(path.clone(), String::from_utf8(bytes)?);
            }
            let mut ids = Vec::new();
            for (label, prompt) in tasks {
                let id = harness.id();
                ids.push(json!({"id": id, "label": label}));
                ctx.trace
                    .event(&ctx.id, "child_spawn", json!({"child": id, "label": label}));
                let h = harness.clone();
                let seed = seed.clone();
                let stop = children.stop.child_token();
                children.running.spawn(async move {
                    h.run_attempt(id, label, prompt, Some(seed), Role::Subagent, stop)
                        .await
                });
            }
            Ok(serde_json::to_string(&json!({"started": ids}))?)
        })
    }
}

// The lead decides which returned proof edits to adopt.
fn child_report(joined: Result<Result<AttemptResult>, tokio::task::JoinError>) -> Value {
    match joined.map_err(anyhow::Error::from).and_then(|r| r) {
        Ok(r) => {
            let mut report = r.report();
            report["target_text"] = json!(r.target_text);
            if !r.files.is_empty() {
                report["files"] = serde_json::to_value(
                    r.files
                        .iter()
                        .map(|(p, text)| (p.as_str(), text))
                        .collect::<std::collections::BTreeMap<_, _>>(),
                )
                .expect("strings serialize");
            }
            report
        }
        Err(e) => json!({"status": "failed", "detail": format!("{e:#}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use worktrees::{Options, Pool, RelPath, Verification, Verifier, VerifyError};

    struct CrashOnce(AtomicU64);
    // Crashes on the first call and passes on the second.
    impl Verifier for CrashOnce {
        fn policy_id(&self) -> &str {
            "crash-once-test"
        }
        fn verify(&self, input: &Verification<'_>) -> Result<String, VerifyError> {
            assert_eq!(
                std::fs::read(input.root().join("Solution.lean")).unwrap(),
                b"saved candidate"
            );
            assert_eq!(
                std::fs::read(input.root().join("Helper.lean")).unwrap(),
                b"saved helper"
            );
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(VerifyError::Operational(std::io::Error::other(
                    "deliberate crash",
                )))
            } else {
                Ok("checked on retry".into())
            }
        }
    }

    #[tokio::test]
    async fn operational_retry_reuses_saved_bytes_and_retains_both_reports() {
        let dir = std::env::temp_dir().join(format!("harness-verify-retry-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("source");
        let store = dir.join("store");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&store).unwrap();
        std::fs::write(source.join("Solution.lean"), "original").unwrap();
        std::fs::write(source.join("Helper.lean"), "original helper").unwrap();
        let pool = Pool::create(
            &source,
            &store,
            Options {
                additional_targets: [RelPath::new("Helper.lean").unwrap()].into(),
                ..Options::default()
            },
            Arc::new(CrashOnce(AtomicU64::new(0))),
        )
        .unwrap();
        let task = pool.task(RelPath::new("Solution.lean").unwrap()).unwrap();
        let mut attempt = task.attempt("test").unwrap();
        attempt.write(b"saved candidate").unwrap();
        std::fs::write(attempt.root().join("Helper.lean"), "saved helper").unwrap();
        let frozen = attempt.freeze().unwrap();
        let saved = proof_files::save(&frozen, &dir).unwrap();
        assert_eq!(proof_files::load(&task, &saved).unwrap().len(), 2);
        let verified = verify_candidate(frozen, &dir).await.unwrap();
        assert_eq!(verified.report(), "checked on retry");
        let report = std::fs::read_to_string(dir.join("verification.txt")).unwrap();
        assert!(report.contains("deliberate crash") && report.contains("checked on retry"));
        assert_eq!(
            std::fs::read(saved.join("Solution.lean")).unwrap(),
            b"saved candidate"
        );
        drop(verified);
        drop(pool);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
