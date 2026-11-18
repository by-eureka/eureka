//! Tapis lifecycle stays on the trusted host. Only SSH session streams enter a proof pod.
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct TapisSpec {
    pub backend: PathBuf,
    pub artifacts: PathBuf,
}

pub struct TapisPod {
    backend: PathBuf,
    state: PathBuf,
}

impl TapisSpec {
    pub async fn create(
        &self,
        attempt: &str,
        root: &Path,
        image: &str,
        memory: u64,
        cpus: f32,
    ) -> Result<(String, TapisPod)> {
        let pod = TapisPod {
            backend: self.backend.clone(),
            state: self.artifacts.join(attempt).join("tapis"),
        };
        let output = pod
            .action("create")
            .arg(root)
            .arg(image)
            .arg(memory.to_string())
            .arg(((cpus * 1000.0).round() as u64).to_string())
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "Tapis pod startup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok((String::from_utf8(output.stdout)?.trim().to_owned(), pod))
    }

    pub async fn prune(&self) -> Result<()> {
        let mut failures = Vec::new();
        for entry in std::fs::read_dir(&self.artifacts)? {
            let state = entry?.path().join("tapis");
            if state.join("pod.json").is_file() && !state.join("deleted").exists() {
                let pod = TapisPod {
                    backend: self.backend.clone(),
                    state,
                };
                if let Err(error) = pod.close().await {
                    failures.push(format!("{}: {error:#}", pod.state.display()));
                }
            }
        }
        ensure!(
            failures.is_empty(),
            "pod cleanup failed: {}",
            failures.join("\n")
        );
        Ok(())
    }
}

impl TapisPod {
    fn action(&self, op: &str) -> Command {
        let mut command = Command::new(&self.backend);
        command
            .arg(op)
            .arg(&self.state)
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    pub fn command(&self, program: &str, args: &[&str]) -> Command {
        let mut command = self.action("exec");
        command.arg(program).args(args).stdin(Stdio::piped());
        command
    }

    pub async fn close(&self) -> Result<()> {
        let out = self.action("delete").output().await?;
        ensure!(
            out.status.success(),
            "Tapis cleanup: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }

    pub async fn file(&self, path: &str, limit: u64, input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut command = self.action(if input.is_some() { "write" } else { "read" });
        let mut child = command.arg(path).stdin(Stdio::piped()).spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap().take(limit + 1);
        let mut stderr = child.stderr.take().unwrap().take(16 * 1024);
        tokio::time::timeout(Duration::from_secs(60), async {
            let send = async {
                if let Some(bytes) = input {
                    stdin.write_all(bytes).await?;
                }
                drop(stdin);
                Ok::<(), std::io::Error>(())
            };
            let mut bytes = Vec::new();
            let mut detail = Vec::new();
            tokio::try_join!(
                send,
                async { stdout.read_to_end(&mut bytes).await.map(|_| ()) },
                async { stderr.read_to_end(&mut detail).await.map(|_| ()) }
            )?;
            ensure!(
                bytes.len() as u64 <= limit,
                "remote file exceeds size limit"
            );
            ensure!(
                detail.len() < 16 * 1024,
                "remote error output exceeds limit"
            );
            ensure!(
                child.wait().await?.success(),
                "Tapis file operation failed: {}",
                String::from_utf8_lossy(&detail)
            );
            Ok(bytes)
        })
        .await
        .context("Tapis file operation timed out")?
    }
}
