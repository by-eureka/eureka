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
use worktrees::{Pool, Verification, Verifier, VerifyError};

pub struct LeanVerifier {
    lake: PathBuf,
    policy: String,
    solution_module: String,
    challenge_module: String,
    reference_names: Vec<String>,
    separate_reference: bool,
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

/// Derive the negation from Lean's type of the original trusted constant,
/// including its implicit and explicit binders. The model supplies only a proof.
pub fn refutation_template(
    template: &Path,
    requested: Option<&str>,
) -> anyhow::Result<tempfile::TempDir> {
    let scratch = tempfile::tempdir()?;
    let root = scratch.path().join("project");
    let mut options = worktrees::Options::default();
    options.excluded_names.insert(".beam".into());
    worktrees::copy_template(template, &root, &options)?;
    let config_file = root.join("config.json");
    let config_text = std::fs::read_to_string(&config_file)?;
    let base = validate_config(&root, &config_text)?;
    let theorem = match (requested, base.theorem_names.as_slice()) {
        (Some(name), names) if names.iter().any(|n| n == name) => name,
        (Some(name), _) => anyhow::bail!("unknown configured theorem: {name}"),
        (None, [only]) => only,
        _ => anyhow::bail!("select one configured theorem with --refute THEOREM"),
    };
    // Quote name components for Lean source. Never splice raw configuration
    // strings into commands, including import statements.
    fn quoted(name: &str) -> anyhow::Result<String> {
        let parts: Vec<_> = name.split('.').collect();
        anyhow::ensure!(
            parts.iter().all(|s| !s.is_empty()
                && !s.contains(['«', '»'])
                && !s.chars().any(char::is_control)),
            "unsupported quoted Lean name: {name}"
        );
        Ok(parts
            .iter()
            .map(|p| format!("«{p}»"))
            .collect::<Vec<_>>()
            .join("."))
    }
    let prefix = base
        .challenge_module
        .rsplit_once('.')
        .map_or(String::new(), |(p, _)| format!("{p}."));
    let challenge = format!("{prefix}EurekaRefutationChallenge");
    let solution = format!("{prefix}EurekaRefutationSolution");
    let source = format!(
        "import {}\n\ntheorem EurekaRefutation.refutation : ¬ (type_of% @_root_.{}) := by\n  sorry\n",
        quoted(&base.challenge_module)?,
        quoted(theorem)?
    );
    let solution_path = solution.replace('.', "/") + ".lean";
    for module in [&challenge, &solution] {
        use std::io::Write;
        let path = root.join(module.replace('.', "/") + ".lean");
        std::fs::File::create_new(&path)
            .with_context(|| format!("refutation module collision: {}", path.display()))?
            .write_all(source.as_bytes())?;
    }
    let mut config: serde_json::Value = serde_json::from_str(&config_text)?;
    config["challenge_module"] = challenge.into();
    config["solution_module"] = solution.into();
    config["theorem_names"] = serde_json::json!(["EurekaRefutation.refutation"]);
    config["definition_names"] = serde_json::json!([]);
    std::fs::write(config_file, serde_json::to_vec_pretty(&config)?)?;
    std::fs::write(
        root.join("proof-files.json"),
        serde_json::to_vec(&[solution_path])?,
    )?;
    Ok(scratch)
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
    pub fn challenge_path(&self) -> String {
        self.challenge_module.replace('.', "/") + ".lean"
    }

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
        // The comparator compares these kernel primitives even if the requested
        // theorem doesn't mention them (Lake/CLI/Check.lean, primitiveTargets).
        // Restrict exports to these closures; exporting every constant in Mathlib
        // would make a small proof depend on the size of the whole library.
        let mut reference_names = [
            "Nat.add",
            "Nat.sub",
            "Nat.mul",
            "Nat.pow",
            "Nat.gcd",
            "Nat.div",
            "Nat.mod",
            "Nat.beq",
            "Nat.ble",
            "Nat.land",
            "Nat.lor",
            "Nat.xor",
            "Nat.shiftLeft",
            "Nat.shiftRight",
            "String.ofList",
            "Char.ofNat",
            "List",
            "eagerReduce",
            "Nat",
            "String",
            "String.mk",
            "Char",
            "optParam",
            "autoParam",
            "semiOutParam",
            "outParam",
        ]
        .map(str::to_owned)
        .to_vec();
        if parsed.permitted_axioms.iter().any(|s| s == "Quot.sound") {
            reference_names.extend(["Quot", "Quot.mk", "Quot.lift", "Quot.ind"].map(str::to_owned));
        }
        reference_names.extend(parsed.theorem_names);
        reference_names.extend(parsed.definition_names);
        reference_names.extend(parsed.permitted_axioms);
        Ok(Self {
            lake,
            policy,
            solution_module: parsed.solution_module,
            challenge_module: parsed.challenge_module,
            reference_names,
            separate_reference: template.join("proof-files.json").is_file(),
            timeout: Duration::from_secs(1800),
            deps: None,
        })
    }
    /// Build the reference with the *original* imports, even when a challenge
    /// and solution share an editable dependency. The private reference stays
    /// outside the candidate project and Lake reads it via --challenge-from-export.
    pub fn prepare(&self, pool: &Pool) -> std::io::Result<()> {
        if self.separate_reference {
            self.reference_export(pool)?;
        }
        Ok(())
    }

    fn reference_export(&self, pool: &Pool) -> std::io::Result<PathBuf> {
        let cached = pool.store().join("challenge.ndjson");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(pool.store().join("challenge.lock"))?;
        lock.lock()?;
        if cached.is_file() {
            return Ok(cached);
        }
        // Publish only complete exports. All leads and verification retries for
        // this immutable pool reuse the same reference.
        let scratch = tempfile::Builder::new()
            .prefix(".reference-")
            .tempdir_in(pool.store())?;
        let dir = scratch.path();
        let project = dir.join("reference");
        pool.write_baseline(&project)
            .map_err(std::io::Error::other)?;
        let packages = project.join(".lake/packages");
        std::fs::create_dir_all(&packages)?;
        let export = dir.join("challenge.ndjson");
        let bin = self.lake.parent().expect("validated Lean binary directory");
        let sdk = bin.parent().expect("Lean SDK root");
        let mut command = Command::new("systemd-run");
        command
            .args([
                "--user",
                "--wait",
                "--pipe",
                "--quiet",
                "--collect",
                "--expand-environment=no",
            ])
            .arg(format!(
                "--property=RuntimeMaxSec={}",
                self.timeout.as_secs()
            ))
            .args([
                "--",
                "bwrap",
                "--die-with-parent",
                "--new-session",
                "--unshare-all",
                "--clearenv",
                "--setenv",
                "HOME",
                "/tmp",
                "--setenv",
                "PATH",
            ])
            .arg(format!("{}:/usr/bin:/bin", bin.display()))
            .args([
                "--ro-bind",
                "/usr",
                "/usr",
                "--ro-bind-try",
                "/bin",
                "/bin",
                "--ro-bind-try",
                "/lib",
                "/lib",
                "--ro-bind-try",
                "/lib64",
                "/lib64",
                // Nix's elan links the SDK's ELF loader and libraries into this store.
                "--ro-bind-try",
                "/nix/store",
                "/nix/store",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--tmpfs",
                "/tmp",
                "--ro-bind",
            ])
            .arg(sdk)
            .arg(sdk)
            .arg("--bind")
            .arg(dir)
            .arg(dir);
        if let Some(deps) = &self.deps {
            command.arg("--ro-bind").arg(deps).arg(&packages);
        }
        // All positional parameters below are controller-owned. Keep Lake's build
        // chatter out of the NDJSON stream. Only the trusted template is present.
        let output = command
            .arg("--chdir")
            .arg(&project)
            .args([
                "--",
                "sh",
                "-ec",
                r#"lake=$1; module=$2; export_file=$3; shift 3
"$lake" build "+$module" >&2
"$lake" env "${lake%/*}/leanexport" "$module" -- "$@" > "$export_file""#,
                "reference",
            ])
            .arg(&self.lake)
            .arg(&self.challenge_module)
            .arg(&export)
            .args(&self.reference_names)
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "build trusted challenge export: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        std::fs::File::open(&export)?.sync_all()?;
        std::fs::rename(export, &cached)?;
        std::fs::File::open(pool.store())?.sync_all()?;
        Ok(cached)
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
        command
            .arg(&self.lake)
            .args(["comparator", "--config", "config.json"]);
        if self.separate_reference || input.is_multi_file() {
            command
                .arg("--challenge-from-export")
                .arg(self.reference_export(input.pool())?);
        }
        let output = command.output()?;
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
