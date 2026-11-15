//! One hardened Docker container per attempt.
//!
//! Each container has a native Beam/Lean runtime. Networking is disabled by
//! default; an internal Docker network can connect optional retrieval services.
//! Only the attempt root is writable. Prepared dependencies are mounted read-only.
//!
//! Nothing is cleaned up on drop. Callers must `close` the sandbox before
//! freezing the workspace, and `prune` removes whatever a run left behind.

use anyhow::{Context, Result, bail};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Path of the attempt root as seen from inside the container.
pub const WORK: &str = "/work";
const HOME: &str = "/tmp/home";
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct SandboxSpec {
    /// Image containing the pinned Lean SDK and native Beam runtime.
    pub image: String,
    /// Docker network name, or `none` for disconnected execution.
    pub network: String,
    /// Host directory holding the image's prepared `.lake/packages`.
    pub packages: PathBuf,
    /// Labels every container with this run so crashed runs can be pruned.
    pub run_id: String,
    pub memory_mib: u64,
    pub cpus: f32,
    pub pids: u32,
    /// Explicit runtime environment passed to every command; no host credentials.
    pub env: Vec<(String, String)>,
}

pub struct Sandbox {
    name: String,
    env: Vec<(String, String)>,
    closed: AtomicBool,
}

impl Sandbox {
    /// Start a container for the attempt at `root`. Returns once it is running.
    pub async fn create(spec: &SandboxSpec, attempt: &str, root: &Path) -> Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let packages = std::fs::canonicalize(&spec.packages)?;
        // Docker would create a missing mount point as root; keep it ours.
        std::fs::create_dir_all(root.join(".lake/packages"))?;
        // Files the agent creates must belong to the harness user, like the root.
        let meta = std::fs::metadata(&root)?;
        let name = format!(
            "harness-{}-{}",
            spec.run_id,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );

        let mut cmd = Command::new("docker");
        // The container only sleeps; all work goes through `docker exec`.
        cmd.args(["run", "-d", "--name", &name])
            .args(["--label", &format!("harness.run={}", spec.run_id)])
            .args(["--label", &format!("harness.attempt={attempt}")])
            .args(["--network", &spec.network])
            .args(["--read-only", "--tmpfs", "/tmp:rw,size=2g"])
            .args(["--cap-drop", "ALL", "--security-opt", "no-new-privileges"])
            .args(["--pids-limit", &spec.pids.to_string()])
            .args(["--memory", &format!("{}m", spec.memory_mib)])
            .args(["--cpus", &spec.cpus.to_string()])
            .args(["--user", &format!("{}:{}", meta.uid(), meta.gid())])
            .args(["-e", &format!("HOME={HOME}")])
            .args(["-v", &format!("{}:{WORK}:rw", root.display())])
            .args([
                "-v",
                &format!("{}:{WORK}/.lake/packages:ro", packages.display()),
            ])
            .args(["-w", WORK, "--entrypoint", "sleep", &spec.image, "infinity"]);
        let started = run(cmd).await.context("docker run");
        let sandbox = Self {
            name,
            env: spec.env.clone(),
            closed: AtomicBool::new(false),
        };
        if let Err(error) = started {
            let _ = sandbox.close().await;
            return Err(error);
        }
        // HOME must exist for Lean and Beam; /tmp is the only scratch space.
        if let Err(error) = run(sandbox.command("mkdir", &["-p", HOME])).await {
            sandbox.close().await?;
            return Err(error);
        }
        Ok(sandbox)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// A command executed inside the container, in `/work`, with stdio piped.
    pub fn command(&self, program: &str, args: &[&str]) -> Command {
        // Only `env` is passed in, never the host environment.
        let mut cmd = Command::new("docker");
        cmd.args(["exec", "-i", "-w", WORK]);
        for (key, value) in &self.env {
            cmd.args(["-e", &format!("{key}={value}")]);
        }
        cmd.arg(&self.name).arg(program).args(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// The in-container timer kills the process group, including descendants.
    /// The host watchdog also covers a wedged Docker exec and inherited pipes.
    pub async fn shell(&self, script: &str, timeout: Duration) -> Result<String> {
        let seconds = timeout.as_secs_f64().to_string();
        let mut child = self
            .command(
                "timeout",
                &[
                    "--signal=KILL",
                    "--kill-after=2s",
                    &seconds,
                    "bash",
                    "-c",
                    script,
                ],
            )
            .stdin(Stdio::null())
            .spawn()
            .context("spawn sandbox shell")?;
        let stdout = child.stdout.take().context("shell stdout")?;
        let stderr = child.stderr.take().context("shell stderr")?;
        let wait = async {
            // Read both pipes while waiting, or a noisy command blocks on a full pipe.
            let (status, stdout, stderr) =
                tokio::try_join!(child.wait(), bounded_read(stdout), bounded_read(stderr))?;
            Ok::<_, std::io::Error>((status, stdout, stderr))
        };
        match tokio::time::timeout(timeout + Duration::from_secs(10), wait).await {
            // A failing command is a normal result; the model sees its exit code
            // (137 after a timeout).
            Ok(Ok((status, stdout, stderr))) => Ok(format!(
                "{stdout}\n--- stderr ---\n{stderr}\n--- exit status: {} ---",
                status.code().unwrap_or(-1)
            )),
            failure => {
                // We can't tell what is still running inside, so stop the
                // container. tools::Shell then cancels the attempt.
                self.close()
                    .await
                    .context("stop sandbox after shell watchdog failure")?;
                anyhow::bail!("shell watchdog/transport failed; sandbox stopped: {failure:?}")
            }
        }
    }

    /// Stop and delete the container. Files in the attempt root remain.
    pub async fn close(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut cmd = Command::new("docker");
        cmd.args(["rm", "-f", &self.name]);
        run(cmd).await.context("docker rm")?;
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Remove every container of `run_id`, e.g. after a crash. Returns the count.
pub async fn prune(run_id: &str) -> Result<usize> {
    let mut list = Command::new("docker");
    list.args([
        "ps",
        "-aq",
        "--filter",
        &format!("label=harness.run={run_id}"),
    ]);
    let ids = run(list).await?;
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if !ids.is_empty() {
        let mut rm = Command::new("docker");
        rm.args(["rm", "-f"]).args(&ids);
        run(rm).await?;
    }
    Ok(ids.len())
}

/// Run a docker CLI command to completion; stdout on success, stderr in the error.
async fn run(mut cmd: Command) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("docker command timed out")?
    .context("spawn docker")?;
    if !output.status.success() {
        bail!(
            "docker exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// Drain the stream to EOF, keeping a bounded head and tail. Reading past the
// cap keeps the writer from blocking on a full pipe.
async fn bounded_read(mut reader: impl AsyncRead + Unpin) -> std::io::Result<String> {
    const HALF: usize = 16 * 1024;
    let mut head = Vec::new();
    let mut tail = std::collections::VecDeque::new();
    let mut total = 0;
    let mut buffer = [0; 8192];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        total += n;
        let take = n.min(HALF - head.len());
        head.extend_from_slice(&buffer[..take]);
        for byte in &buffer[take..n] {
            if tail.len() == HALF {
                tail.pop_front();
            }
            tail.push_back(*byte);
        }
    }
    let mut out = String::from_utf8_lossy(&head).into_owned();
    let omitted = total - head.len() - tail.len();
    if omitted > 0 {
        out.push_str(&format!("\n...[{omitted} bytes omitted]...\n"));
    }
    out.push_str(&String::from_utf8_lossy(tail.make_contiguous()));
    Ok(out)
}
