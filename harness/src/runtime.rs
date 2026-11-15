//! Beam's native MCP runtime, isolated in one container per attempt.
//! The image contains a complete Lean SDK with a qualified Beam installation.

use crate::sandbox::Sandbox;
use anyhow::{Context, Result, ensure};
use std::{path::Path, process::Command};

pub const MCP: &str = "/opt/lean/bin/lean-beam-mcp";

pub fn env() -> Vec<(String, String)> {
    vec![
        (
            "PATH".into(),
            "/opt/lean/bin:/usr/local/bin:/usr/bin:/bin".into(),
        ),
        ("BEAM_SESSION_ROOT".into(), "/work/.beam".into()),
        ("ELAN_HOME".into(), "/opt/elan".into()),
    ]
}

pub fn mcp(sandbox: &Sandbox) -> tokio::process::Command {
    sandbox.command(MCP, &[])
}

/// Check the trusted dependency snapshot before creating attempts or spending tokens.
pub fn validate_packages(template: &Path, packages: &Path) -> Result<()> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(template.join("lake-manifest.json"))?)?;
    let entries = manifest["packages"]
        .as_array()
        .context("manifest packages must be an array")?;
    for entry in entries {
        let name = entry["name"].as_str().context("package without a name")?;
        ensure!(
            !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != "..",
            "invalid package name: {name}"
        );
        let path = packages.join(name);
        ensure!(
            path.is_dir(),
            "prepared package {name} is missing; run scripts/setup-runtime.sh"
        );
        if entry["type"] == "git" {
            let expected = entry["rev"]
                .as_str()
                .context("git package without a revision")?;
            let output = Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(["rev-parse", "HEAD"])
                .output()?;
            ensure!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).trim() == expected,
                "prepared package {name} does not match manifest revision {expected}; extract matching packages"
            );
        }
    }
    Ok(())
}

/// Resolve the image to an immutable ID and compare its compiler to the verifier's.
/// Called once per run. Verification-only runs never start Docker.
pub async fn validate_image(image: &str, template: &Path) -> Result<String> {
    let output = tokio::process::Command::new("docker")
        .args(["image", "inspect", "--format={{.Id}}", image])
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "cannot inspect runtime image {image}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let image_id = String::from_utf8(output.stdout)?.trim().to_owned();
    let toolchain = std::fs::read_to_string(template.join("lean-toolchain"))?;
    let expected = Command::new("elan")
        .args(["run", toolchain.trim(), "lean", "--githash"])
        .output()?;
    ensure!(
        expected.status.success(),
        "cannot read verifier's Lean compiler identity"
    );
    let actual = tokio::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--entrypoint",
            "/opt/lean/bin/lean",
            &image_id,
            "--githash",
        ])
        .output()
        .await?;
    ensure!(
        actual.status.success() && actual.stdout == expected.stdout,
        "runtime image compiler differs from template {}; rebuild the image for this toolchain",
        toolchain.trim()
    );
    Ok(image_id)
}
