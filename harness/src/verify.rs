//! Independent verification through Lean 4.35+'s bundled `lake comparator`.
//!
//! The trusted template selects the exact compiler, exporter and kernel checker.
//! Lake owns the Bubblewrap sandbox; systemd bounds the entire process tree.
//! Only the candidate source is copied into the fresh verification workspace.

use anyhow::Context;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use worktrees::{Verification, Verifier, VerifyError};

pub struct LeanVerifier {
    lake: PathBuf,
    policy: String,
    solution_module: String,
    pub timeout: Duration,
    /// Trusted prebuilt dependencies, mounted read-only inside the fresh root.
    pub deps: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    challenge_module: String,
    solution_module: String,
    theorem_names: Vec<String>,
    #[serde(default)]
    definition_names: Vec<String>,
    permitted_axioms: Vec<String>,
    #[serde(default)]
    external_kernels: std::collections::BTreeMap<String, Vec<String>>,
}

fn validate_config(template: &Path, text: &str) -> anyhow::Result<Config> {
    let config: Config = serde_json::from_str(text).context("invalid verification config.json")?;
    anyhow::ensure!(
        !config.theorem_names.is_empty() || !config.definition_names.is_empty(),
        "config.json must name at least one theorem or definition"
    );
    for name in config
        .theorem_names
        .iter()
        .chain(&config.definition_names)
        .chain(&config.permitted_axioms)
    {
        anyhow::ensure!(
            !name.trim().is_empty(),
            "empty declaration name in config.json"
        );
    }
    anyhow::ensure!(
        config.challenge_module != config.solution_module,
        "challenge_module and solution_module must differ"
    );
    for module in [&config.challenge_module, &config.solution_module] {
        anyhow::ensure!(
            !module.contains(['/', '\\']) && module.split('.').all(|p| !p.is_empty()),
            "invalid module path: {module}"
        );
        let path = template.join(module.replace('.', "/") + ".lean");
        anyhow::ensure!(path.is_file(), "missing module {}", path.display());
    }
    for command in config.external_kernels.values() {
        anyhow::ensure!(
            !command.is_empty() && !command[0].is_empty(),
            "external kernel command must not be empty"
        );
    }
    Ok(config)
}

fn supported_version(version: &str) -> bool {
    let Some(number) = version.split("version ").nth(1) else {
        return false;
    };
    let mut parts = number.split('.');
    parts.next() == Some("4")
        && parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .is_some_and(|minor| minor >= 35)
}

impl LeanVerifier {
    pub fn from_template(template: &Path) -> anyhow::Result<Self> {
        let toolchain = std::fs::read_to_string(template.join("lean-toolchain"))?;
        let toolchain = toolchain.trim();
        anyhow::ensure!(!toolchain.is_empty(), "empty lean-toolchain");
        let config = std::fs::read_to_string(template.join("config.json"))?;
        let parsed = validate_config(template, &config)?;
        anyhow::ensure!(
            template.join("lake-manifest.json").is_file(),
            "missing lake-manifest.json; prepare the trusted template first"
        );
        // Resolve once: verification must not follow a changed default toolchain.
        let resolved = Command::new("elan")
            .args(["which", "lake"])
            .env("ELAN_TOOLCHAIN", toolchain)
            .current_dir(template)
            .output()
            .context("resolve template's Lake with elan")?;
        anyhow::ensure!(
            resolved.status.success(),
            "cannot resolve {toolchain}: {}",
            String::from_utf8_lossy(&resolved.stderr)
        );
        let lake = PathBuf::from(String::from_utf8(resolved.stdout)?.trim());
        let bin = lake.parent().context("Lake has no parent directory")?;
        let version = Command::new(bin.join("lean")).arg("--version").output()?;
        let version_text = String::from_utf8(version.stdout)?;
        anyhow::ensure!(
            version.status.success() && supported_version(&version_text),
            "verification requires Lean 4.35 or newer; {toolchain}: {version_text}"
        );
        for name in ["leanexport", "leanchecker"] {
            anyhow::ensure!(bin.join(name).is_file(), "{toolchain} lacks bundled {name}");
        }
        let canonical_config: serde_json::Value = serde_json::from_str(&config)?;
        let policy = format!(
            "lake-comparator-v1;toolchain:{toolchain};compiler:{};config:{canonical_config}",
            version_text.trim()
        );
        Ok(Self {
            lake,
            policy,
            solution_module: parsed.solution_module,
            timeout: Duration::from_secs(1800),
            deps: None,
        })
    }
}

impl Verifier for LeanVerifier {
    fn policy_id(&self) -> &str {
        &self.policy
    }

    fn verify(&self, input: &Verification<'_>) -> Result<String, VerifyError> {
        let path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::new("systemd-run");
        command
            .args(["--user", "--wait", "--pipe", "--quiet", "--collect"])
            .arg(format!(
                "--property=RuntimeMaxSec={}",
                self.timeout.as_secs()
            ))
            .arg(format!("-EPATH={path}"))
            .arg("--working-directory")
            .arg(input.root())
            .arg("--");
        if let Some(deps) = &self.deps {
            // A symlink into /home or /tmp becomes inaccessible when Lake covers
            // those directories. A read-only mount survives the nested sandbox
            // and prevents the candidate from changing shared build artifacts.
            let packages = input.root().join(".lake/packages");
            std::fs::create_dir_all(&packages)?;
            command
                .args([
                    "bwrap",
                    "--die-with-parent",
                    "--bind",
                    "/",
                    "/",
                    "--dev-bind",
                    "/dev",
                    "/dev",
                    "--ro-bind",
                ])
                .arg(deps)
                .arg(packages)
                .arg("--");
        }
        let output = command
            .arg(&self.lake)
            .args(["comparator", "--config", "config.json"])
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let report = tail(&format!("{stdout}\n{stderr}"), 1024 * 1024 - 1024);
        classify(
            output.status.success(),
            &stdout,
            &stderr,
            report,
            &self.solution_module,
        )
    }
}

// A nonzero exit alone doesn't mean the proof is wrong. Only output we recognize
// as a rejection counts as one. Anything else (a crash, a PANIC, setup trouble)
// is an operational failure, which search.rs retries once.
fn classify(
    success: bool,
    stdout: &str,
    stderr: &str,
    report: String,
    solution_module: &str,
) -> Result<String, VerifyError> {
    if success && stdout.contains("Your solution is okay!") {
        return Ok(report);
    }
    let text = format!("{stdout}\n{stderr}");
    let rejection = [
        "Illegal axiom detected",
        "does not match between challenge and target",
        "theorem statement do not match",
        "constant kind don't match",
        "Const not found in solution",
        "Constant not found in solution",
        "Solution constant is not a theorem",
        "Solution constant is not a definition",
        "Lean default kernel rejects the solution",
        "Nanoda kernel rejected the solution",
    ]
    .iter()
    .any(|marker| text.contains(marker));
    let solution_build_error = stdout
        .lines()
        .rfind(|line| line.starts_with("Building ") || line.starts_with("Exporting "))
        .is_some_and(|line| line == format!("Building {solution_module}"))
        && text.contains(".lean:")
        && text.contains("error:");
    if !text.contains("PANIC") && (rejection || solution_build_error) {
        Err(VerifyError::Rejected(report))
    } else {
        Err(VerifyError::Operational(std::io::Error::other(report)))
    }
}

// worktrees caps reports at 1 MiB. Keep the end, which is where the verdict is.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.trim().to_owned();
    }
    let start = text.ceil_char_boundary(text.len() - max);
    text[start..].trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(
        success: bool,
        stdout: &str,
        stderr: &str,
        report: String,
    ) -> Result<String, VerifyError> {
        super::classify(success, stdout, stderr, report, "Proofs.Solution")
    }

    #[test]
    fn minimum_toolchain() {
        for text in ["Lean (version 4.35.0, x)", "Lean (version 4.36.0-rc2, x)"] {
            assert!(supported_version(text));
        }
        for text in [
            "Lean (version 4.34.1, x)",
            "Lean (version 3.51.0, x)",
            "garbage",
        ] {
            assert!(!supported_version(text));
        }
    }

    #[test]
    fn failures_require_proof_rejection_evidence() {
        for text in [
            "Child exited with 139",
            "Failed to connect to bus",
            "error: bwrap: setting up uid map: Permission denied",
            "error: malformed configuration",
        ] {
            assert!(matches!(
                classify(false, "", text, text.into()),
                Err(VerifyError::Operational(_))
            ));
        }
        for text in [
            "Illegal axiom detected: sorryAx",
            "theorem statement do not match",
            "Const not found in solution",
            "Lean default kernel rejects the solution",
        ] {
            assert!(matches!(
                classify(false, "", text, text.into()),
                Err(VerifyError::Rejected(_))
            ));
        }
        assert!(classify(true, "Your solution is okay!", "", "ok".into()).is_ok());
        assert!(classify(true, "", "", "missing verdict".into()).is_err());
        assert!(matches!(
            classify(false, "", "PANIC Illegal axiom detected", "panic".into()),
            Err(VerifyError::Operational(_))
        ));
    }
}
