//! CLI entry point. Run it from the repository root: the `--beam-skill`
//! default is a relative path.
//!
//! Exits 0 when a proof is verified and published, 2 when every lead stopped
//! without one, and 1 on any other error.

use anyhow::{Context, Result};
use clap::Parser;
use harness::agent::AgentConfig;
use harness::responses::{Client, ModelConfig, headers_from_env};
use harness::runtime;
use harness::sandbox::SandboxSpec;
use harness::search::Harness;
use harness::{prompt, proof_files, trace, verify};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use worktrees::{Options, Pool};

/// Prove the theorems of a Lean project template with parallel LLM agents.
///
/// Every attempt runs in its own Docker sandbox with its own Beam runtime.
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// Trusted Lean project in the agent image's layout (Proofs/Challenge.lean,
    /// Proofs/Solution.lean, config.json, the image's lakefile and manifest).
    #[arg(long)]
    template: PathBuf,
    /// Directory for pools, attempts and results (must exist; keep it private).
    #[arg(long)]
    store: PathBuf,
    /// Primary proof file (first manifest entry, or Proofs/Solution.lean for legacy templates).
    #[arg(long)]
    target: Option<String>,
    /// Number of independent top-level agents.
    #[arg(short = 'n', long, default_value_t = 1)]
    agents: usize,
    /// The agent image's prepared `.lake/packages`, extracted to the host.
    #[arg(long)]
    packages: PathBuf,
    /// Image containing the Lean SDK and native Beam runtime.
    #[arg(long, default_value = "eureka-lean:dev")]
    image: String,
    /// Execute attempts in TACC Pods using the supplied trusted scripts/tapis-pod.py.
    #[arg(long)]
    tapis_backend: Option<PathBuf>,
    /// Docker network; use an internal network to enable optional retrieval.
    #[arg(long, default_value = "none")]
    network: String,
    /// Memory limit per sandbox.
    #[arg(long, default_value_t = 12 * 1024)]
    memory_mib: u64,
    /// CPU limit per sandbox.
    #[arg(long, default_value_t = 2.0)]
    cpus: f32,
    /// Model endpoint (defaults to https://api.deepseek.com). Required with custom headers.
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long, default_value = "deepseek-flash")]
    model: String,
    /// Do not send an Authorization: Bearer header to the model provider.
    #[arg(long)]
    no_bearer_auth: bool,
    /// Add a model-provider header whose value is read from an environment variable.
    /// Repeat as needed, e.g. --header-env X-Tapis-Token=TAPIS_TOKEN.
    #[arg(
        long = "header-env",
        value_name = "HEADER=ENV_VAR",
        requires = "base_url"
    )]
    header_env: Vec<String>,
    /// Reasoning effort: low, high, max.
    #[arg(long, default_value = "high")]
    effort: String,
    #[arg(long, default_value_t = 60)]
    max_turns: usize,
    /// Output token cap per model turn (reasoning included).
    #[arg(long, default_value_t = 32_768)]
    max_output_tokens: u32,
    /// Disable the sandboxed shell tool.
    #[arg(long)]
    no_shell: bool,
    /// Memoize model responses here (replays identical requests for free).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Save requests, responses and stream events (no headers). Disable explicitly with =false.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set,
          num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    capture_requests: bool,
    /// Require output-text token logprobs; missing scores are an error. Disable with =false.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set,
          num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    logprobs: bool,
    /// Extra stdio MCP server JSON config (name, command, args); runs in each sandbox.
    #[arg(long)]
    mcp: Vec<PathBuf>,
    /// Seed each lead with a saved target file or multi-file candidate directory, keeping the trusted template unchanged.
    #[arg(long)]
    seed: Option<PathBuf>,
    /// Reverify a saved file or multi-file candidate directory without calling a model or starting an agent sandbox.
    #[arg(long, conflicts_with = "seed")]
    verify_only: Option<PathBuf>,
    /// JSONL trace file.
    #[arg(long, default_value = "harness-trace.jsonl")]
    trace: PathBuf,
    /// Lean Beam skill directory (SKILL.md plus references), used as the workflow.
    #[arg(long, default_value = "lean-beam/skills/lean-beam")]
    beam_skill: PathBuf,
    /// Override the first user message given to each top-level agent.
    #[arg(long)]
    prompt: Option<String>,
    /// Continue text-only replies until submit/finish or the turn limit; encourage persistence.
    #[arg(long)]
    dont_give_up: bool,
}

impl Cli {
    fn model_client(&self) -> Result<Client> {
        let api_key = if self.no_bearer_auth {
            String::new()
        } else {
            std::env::var("DEEPSEEK_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .context("set DEEPSEEK_API_KEY or OPENAI_API_KEY, or pass --no-bearer-auth")?
        };
        Client::new_with_headers(
            ModelConfig {
                base_url: self
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.deepseek.com")
                    .to_owned(),
                api_key,
                model: self.model.clone(),
                effort: self.effort.clone(),
                max_output_tokens: self.max_output_tokens,
                logprobs: self.logprobs,
                idle_timeout: Duration::from_secs(60 * 60),
                max_attempts: 5,
            },
            !self.no_bearer_auth,
            headers_from_env(&self.header_env)?,
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Catch bad paths, unsupported Lean versions and invalid verification configuration
    // before spending anything on the model.
    let template = std::fs::canonicalize(&cli.template)?;
    let packages = std::fs::canonicalize(&cli.packages)?;
    runtime::validate_packages(&template, &packages)?;
    let mut verifier = verify::LeanVerifier::from_template(&template)?;
    verifier.deps = Some(packages.clone());
    let (primary, additional) =
        proof_files::scope(&template, cli.target.as_deref(), &verifier.challenge_path())?;
    let primary_name = primary.as_str().to_owned();
    let multi = !additional.is_empty();
    let instructions_files = std::iter::once(&primary)
        .chain(&additional)
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let challenge = verifier.challenge_path();
    let mut options = Options {
        additional_targets: additional,
        ..Options::default()
    };
    // Beam keeps per-attempt session state in .beam; it isn't part of the project.
    options.excluded_names.insert(".beam".into());
    let verifier = Arc::new(verifier);
    let pool = Pool::create(&template, &cli.store, options, verifier.clone())?;
    verifier
        .prepare(&pool)
        .context("prepare trusted reference before starting attempts")?;
    let task = pool.task(primary)?;
    let run_id = pool
        .store()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "run".into());
    eprintln!("pool: {}", pool.store().display());

    let artifacts = pool.store().join("attempts");
    std::fs::create_dir(&artifacts)?;
    if let Some(candidate) = &cli.verify_only {
        // Re-check a saved file. No model or sandbox is involved, so this
        // happens before the API key is read.
        let files = proof_files::load(&task, candidate)?;
        let dir = artifacts.join("reverification");
        std::fs::create_dir(&dir)?;
        let attempt = task.attempt("reverification")?;
        for (path, source) in files {
            worktrees::replace_beneath(&attempt.root(), &path, source.as_bytes())?;
        }
        let frozen = attempt.freeze()?;
        let candidate = proof_files::save(&frozen, &dir)?;
        let result = harness::search::verify_candidate(frozen, &dir).await;
        trace::artifact(
            &dir.join("result.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "status": match &result { Ok(_) => "verified", Err(worktrees::Error::Rejected(_)) => "rejected", Err(_) => "failed" },
                "detail": result.as_ref().err().map(ToString::to_string), "candidate": candidate
            }))?,
        )?;
        let published = match task.publish(result?)? {
            worktrees::PublishOutcome::Committed(p) | worktrees::PublishOutcome::AlreadyWon(p) => p,
        };
        println!(
            "VERIFIED {}",
            if multi {
                published.root()
            } else {
                published.file()
            }
            .display()
        );
        return Ok(());
    }
    anyhow::ensure!(cli.agents > 0, "--agents must be positive");
    let client = cli.model_client()?;
    let tapis = cli
        .tapis_backend
        .as_ref()
        .map(|backend| -> Result<_> {
            anyhow::ensure!(
                cli.network == "none",
                "custom Docker networks are local to the Docker backend"
            );
            Ok(harness::tapis::TapisSpec {
                backend: backend.canonicalize()?,
                artifacts: artifacts.clone(),
            })
        })
        .transpose()?;
    let image = if tapis.is_some() {
        cli.image.clone()
    } else {
        runtime::validate_image(&cli.image, &template).await?
    };
    // A seed only sets the starting file; the conversation starts fresh.
    let seed = cli
        .seed
        .as_ref()
        .map(|p| proof_files::load(&task, p))
        .transpose()?;
    let harness = Arc::new(Harness {
        task,
        client,
        sandbox: SandboxSpec {
            image,
            tapis,
            network: cli.network,
            packages,
            run_id,
            memory_mib: cli.memory_mib,
            cpus: cli.cpus,
            pids: 512,
            env: runtime::env(),
        },
        trace: Arc::new(trace::Trace::open(&cli.trace)?),
        agent: AgentConfig {
            max_turns: cli.max_turns,
            cache_dir: cli.cache_dir,
            capture_requests: cli.capture_requests,
            dont_give_up: cli.dont_give_up,
        },
        instructions: {
            let mut text = prompt::instructions(
                &template,
                &primary_name,
                &instructions_files,
                &challenge,
                !cli.no_shell,
                &cli.beam_skill,
            )?;
            if cli.dont_give_up {
                // The loop also continues text-only replies; --max-turns still applies.
                text.push_str("\n\nThis statement is definitely provable as stated. Do not give up and do not end the session without a verified submission: if one approach stalls, try another, split the hard claim among subagents, and keep going.\n");
            }
            text
        },
        shell: !cli.no_shell,
        shell_timeout: Duration::from_secs(300),
        stop: CancellationToken::new(),
        artifacts,
        next_id: AtomicU64::new(1),
        extra_servers: cli
            .mcp
            .iter()
            .map(|path| harness::beam::ExtraServer::load(path))
            .collect::<Result<_>>()?,
        usage: Default::default(),
    });

    let prompt = cli.prompt.unwrap_or_else(|| {
        format!(
            "Prove every theorem listed in config.json across these proof files: {}. Start by reading the files and probing the goals.",
            instructions_files
        )
    });
    let stop = harness.stop.clone();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let signals = tokio::spawn(async move {
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
        // Cancel instead of exiting so every attempt can stop its container
        // and save its candidate.
        eprintln!("stopping agents; saving candidates and joining children");
        stop.cancel();
    });
    let result = harness
        .independent(cli.agents, &prompt, seed.as_ref())
        .await;
    signals.abort();
    match result? {
        Some(published) => {
            println!(
                "VERIFIED {}",
                if multi {
                    published.root()
                } else {
                    published.file()
                }
                .display()
            );
            println!("{}", published.report()?);
            Ok(())
        }
        None => {
            eprintln!("no agent produced a verified proof");
            std::process::exit(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn custom_headers_require_an_explicit_destination() {
        let parsed = Cli::try_parse_from([
            "harness",
            "--template",
            ".",
            "--packages",
            ".",
            "--store",
            ".",
            "--header-env",
            "X-Tapis-Token=TAPIS_TOKEN",
        ]);
        assert_eq!(
            parsed.unwrap_err().kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[tokio::test]
    async fn cli_provider_headers_reach_http_without_bearer_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // Use an existing variable so this test never mutates process-wide environment.
        let expected = std::env::var("PATH").unwrap();
        let cli = Cli::parse_from([
            "harness",
            "--template",
            ".",
            "--store",
            ".",
            "--packages",
            ".",
            "--base-url",
            &format!("http://{address}/v1"),
            "--no-bearer-auth",
            "--header-env",
            "X-Tapis-Token=PATH",
            "--header-env",
            "X-Tapis-Tenant=PATH",
            "--logprobs=false",
        ]);
        let client = cli.model_client().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(socket.read_u8().await.unwrap());
            }
            let headers = String::from_utf8(bytes).unwrap();
            assert!(!headers.to_ascii_lowercase().contains("\r\nauthorization:"));
            assert!(headers.contains(&format!("x-tapis-token: {expected}\r\n")));
            assert!(headers.contains(&format!("x-tapis-tenant: {expected}\r\n")));
            let mut body = [0; 2];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"{}");
            let response = "data: {\"type\":\"response.completed\",\"response\":{}}\n\n";
            socket.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            ).as_bytes()).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let trace = trace::Trace::open(&path).unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            client.complete(&json!({}), &trace, "headers", Some(&trace)),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
        let capture = std::fs::read_to_string(&path).unwrap();
        assert!(!capture.contains(&std::env::var("PATH").unwrap()));
    }
}
