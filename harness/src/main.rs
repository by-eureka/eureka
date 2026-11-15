//! CLI entry point. Run it from the repository root: the `--beam-skill`
//! default is a relative path.
//!
//! Exits 0 when a proof is verified and published, 2 when every lead stopped
//! without one, and 1 on any other error.

use anyhow::{Context, Result};
use clap::Parser;
use harness::agent::AgentConfig;
use harness::pool::PoolConfig;
use harness::responses::{Client, ModelConfig};
use harness::sandbox::SandboxSpec;
use harness::search::Harness;
use harness::{prompt, trace, verify};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use worktrees::{Options, Pool, RelPath};

/// Prove the theorems of a Lean project template with parallel LLM agents.
///
/// Every attempt runs in its own Docker sandbox attached to a Beam pool.
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// Trusted Lean project in the pool image's layout (Proofs/Challenge.lean,
    /// Proofs/Solution.lean, config.json, the image's lakefile and manifest).
    #[arg(long)]
    template: PathBuf,
    /// Directory for pools, attempts and results (must exist; keep it private).
    #[arg(long)]
    store: PathBuf,
    /// File the agents may change, relative to the template root.
    #[arg(long, default_value = "Proofs/Solution.lean")]
    target: String,
    /// Number of independent top-level agents.
    #[arg(short = 'n', long, default_value_t = 1)]
    agents: usize,
    /// The pool image's prepared `.lake/packages`, extracted to the host.
    #[arg(long)]
    packages: PathBuf,
    /// Beam pool image, used for sandboxes.
    #[arg(long, default_value = "beam-pool:local")]
    image: String,
    /// Internal Docker network shared by sandboxes and the pool gateway.
    #[arg(long, default_value = "harness-agents")]
    network: String,
    /// Pool gateway as seen from that network.
    #[arg(long, default_value = "gateway:9000")]
    pool_endpoint: String,
    /// File containing the pool's shared token.
    #[arg(long)]
    pool_token_file: PathBuf,
    /// Seconds allowed for attaching a sandbox to the pool. Attaching hashes
    /// every prepared file, Mathlib included, so it is CPU-bound.
    #[arg(long, default_value_t = 300)]
    pool_attach_timeout_secs: u64,
    /// Memory limit per sandbox.
    #[arg(long, default_value_t = 12 * 1024)]
    memory_mib: u64,
    /// CPU limit per sandbox.
    #[arg(long, default_value_t = 2.0)]
    cpus: f32,
    #[arg(long, default_value = "https://api.deepseek.com")]
    base_url: String,
    #[arg(long, default_value = "deepseek-flash")]
    model: String,
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
    /// Seed each lead with this saved target, keeping the trusted template unchanged.
    #[arg(long)]
    seed: Option<PathBuf>,
    /// Reverify a saved target without calling a model or starting an agent sandbox.
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Catch bad paths, unsupported Lean versions and invalid verification configuration
    // before spending anything on the model.
    let template = std::fs::canonicalize(&cli.template)?;
    let packages = std::fs::canonicalize(&cli.packages)?;
    let mut verifier = verify::LeanVerifier::from_template(&template)?;
    verifier.deps = Some(packages.clone());
    let mut options = Options::default();
    // Beam keeps per-attempt session state in .beam; it isn't part of the project.
    options.excluded_names.insert(".beam".into());
    let pool = Pool::create(&template, &cli.store, options, Arc::new(verifier))?;
    let task = pool.task(RelPath::new(&cli.target)?)?;
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
        let bytes = std::fs::read(candidate)?;
        let dir = artifacts.join("reverification");
        std::fs::create_dir(&dir)?;
        trace::artifact(&dir.join("candidate.lean"), &bytes)?;
        let mut attempt = task.attempt("reverification")?;
        attempt.write(&bytes)?;
        let frozen = attempt.freeze()?;
        let result = harness::search::verify_candidate(task.clone(), frozen, &dir).await;
        trace::artifact(
            &dir.join("result.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "status": match &result { Ok(_) => "verified", Err(worktrees::Error::Rejected(_)) => "rejected", Err(_) => "failed" },
                "detail": result.as_ref().err().map(ToString::to_string), "candidate": dir.join("candidate.lean")
            }))?,
        )?;
        let published = match task.publish(result?)? {
            worktrees::PublishOutcome::Committed(p) | worktrees::PublishOutcome::AlreadyWon(p) => p,
        };
        println!("VERIFIED {}", published.file().display());
        return Ok(());
    }
    anyhow::ensure!(cli.agents > 0, "--agents must be positive");
    // A seed only sets the starting file; the conversation starts fresh.
    let seed = cli.seed.as_ref().map(std::fs::read_to_string).transpose()?;
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .context("set DEEPSEEK_API_KEY or OPENAI_API_KEY")?;
    let token = std::fs::read_to_string(&cli.pool_token_file)
        .with_context(|| format!("read {}", cli.pool_token_file.display()))?
        .trim()
        .to_owned();

    // The API key stays on the host. Sandboxes only get PoolConfig::env.
    let client = Client::new(ModelConfig {
        base_url: cli.base_url,
        api_key,
        model: cli.model,
        effort: cli.effort,
        max_output_tokens: cli.max_output_tokens,
        logprobs: cli.logprobs,
        idle_timeout: Duration::from_secs(60 * 60),
        max_attempts: 5,
    })?;
    let beam_pool = PoolConfig::new(
        cli.pool_endpoint,
        token,
        Duration::from_secs(cli.pool_attach_timeout_secs),
    );
    let harness = Arc::new(Harness {
        task,
        client,
        sandbox: SandboxSpec {
            image: cli.image,
            network: cli.network,
            packages,
            run_id,
            memory_mib: cli.memory_mib,
            cpus: cli.cpus,
            pids: 512,
            env: beam_pool.env(),
        },
        pool: beam_pool,
        trace: Arc::new(trace::Trace::open(&cli.trace)?),
        agent: AgentConfig {
            max_turns: cli.max_turns,
            cache_dir: cli.cache_dir,
            capture_requests: cli.capture_requests,
            dont_give_up: cli.dont_give_up,
        },
        instructions: {
            let mut text =
                prompt::instructions(&template, &cli.target, !cli.no_shell, &cli.beam_skill)?;
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
            "Prove every theorem listed in config.json in `{}`. Start by reading the files and probing the goals.",
            cli.target
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
        .independent(cli.agents, &prompt, seed.as_deref())
        .await;
    signals.abort();
    match result? {
        Some(published) => {
            println!("VERIFIED {}", published.file().display());
            println!("{}", published.report()?);
            Ok(())
        }
        None => {
            eprintln!("no agent produced a verified proof");
            std::process::exit(2)
        }
    }
}
