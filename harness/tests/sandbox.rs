//! Isolation properties of an attempt sandbox, asserted from inside it.
//! Requires HARNESS_IMAGE naming the built runtime image:
//!
//! HARNESS_IMAGE=eureka-lean:dev cargo test --test sandbox -- --ignored --nocapture

use harness::sandbox::{Sandbox, SandboxSpec, WORK, prune};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Command;

struct Fixture {
    dir: PathBuf,
    network: String,
    image: String,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let image = std::env::var("HARNESS_IMAGE").expect("set HARNESS_IMAGE");
        let have_image = Command::new("docker")
            .args(["image", "inspect", &image])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(have_image, "runtime image {image} unavailable");
        let id = format!("sandbox-test-{tag}-{}", std::process::id());
        let dir = std::env::temp_dir().join(&id);
        std::fs::create_dir_all(dir.join("root")).unwrap();
        std::fs::create_dir_all(dir.join("packages")).unwrap();
        std::fs::write(dir.join("packages/marker"), "ro").unwrap();
        let ok = Command::new("docker")
            .args(["network", "create", "--internal", &id])
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "create test network");
        Self {
            dir,
            network: id,
            image,
        }
    }

    fn spec(&self) -> SandboxSpec {
        SandboxSpec {
            image: self.image.clone(),
            network: self.network.clone(),
            packages: self.dir.join("packages"),
            run_id: self.network.clone(),
            memory_mib: 512,
            cpus: 1.0,
            pids: 64,
            env: vec![("HARNESS_TEST_VAR".into(), "visible".into())],
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Clean up even if the test panicked before close.
        if let Ok(output) = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=harness.run={}", self.network),
            ])
            .output()
        {
            let ids = String::from_utf8_lossy(&output.stdout);
            if !ids.trim().is_empty() {
                let _ = Command::new("docker")
                    .args(["rm", "-f"])
                    .args(ids.split_whitespace())
                    .output();
            }
        }
        let _ = Command::new("docker")
            .args(["network", "rm", &self.network])
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn sh(sandbox: &Sandbox, script: &str) -> (bool, String) {
    let out = sandbox
        .command("bash", &["-c", script])
        .output()
        .await
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}

#[tokio::test]
#[ignore = "requires Docker and HARNESS_IMAGE"]
async fn sandbox_isolates_the_attempt() {
    let f = Fixture::new("isolation");
    let root = f.dir.join("root");
    let sandbox = Sandbox::create(&f.spec(), "agent-1/0-test", &root)
        .await
        .unwrap();

    let (_, caps) = sh(&sandbox, "grep ^CapEff /proc/self/status").await;
    assert!(caps.ends_with("0000000000000000"), "{caps}");
    let (_, privs) = sh(&sandbox, "grep ^NoNewPrivs /proc/self/status").await;
    assert_eq!(privs, "NoNewPrivs:\t1");

    let uid = std::fs::metadata(&root).unwrap().uid();
    // The sandbox runs as our uid, so the harness can read what it writes.
    let (_, id) = sh(&sandbox, "id -u").await;
    assert_eq!(id, uid.to_string());

    let (ok, _) = sh(
        &sandbox,
        &format!("echo hi > {WORK}/note && cat {WORK}/note"),
    )
    .await;
    assert!(ok, "attempt root is writable");
    assert_eq!(std::fs::read_to_string(root.join("note")).unwrap(), "hi\n");
    assert_eq!(std::fs::metadata(root.join("note")).unwrap().uid(), uid);

    let (ok, _) = sh(&sandbox, &format!("touch {WORK}/.lake/packages/x")).await;
    // Packages are shared by every attempt: readable, never writable.
    assert!(!ok, "packages are read-only");
    let (_, marker) = sh(&sandbox, &format!("cat {WORK}/.lake/packages/marker")).await;
    assert_eq!(marker, "ro");

    let (ok, _) = sh(&sandbox, "touch /usr/x").await;
    assert!(!ok, "root filesystem is read-only");
    let home = std::env::var("HOME").unwrap();
    let (ok, _) = sh(&sandbox, &format!("ls {home}")).await;
    assert!(!ok, "host home is invisible");
    let (ok, _) = sh(&sandbox, "timeout 3 bash -c 'echo > /dev/tcp/1.1.1.1/53'").await;
    assert!(!ok, "no internet");
    let (_, var) = sh(&sandbox, "echo $HARNESS_TEST_VAR").await;
    assert_eq!(var, "visible");

    let name = sandbox.name().to_owned();
    sandbox.close().await.unwrap();
    let listed = Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("name={name}")])
        .output()
        .unwrap();
    assert!(listed.stdout.is_empty(), "container removed");
}

#[tokio::test]
#[ignore = "requires Docker and HARNESS_IMAGE"]
async fn prune_removes_every_container_of_a_run() {
    // Two sandboxes dropped without close. Prune finds both, then nothing.
    let f = Fixture::new("prune");
    let root = f.dir.join("root");
    let spec = f.spec();
    let _a = Sandbox::create(&spec, "a", &root).await.unwrap();
    let _b = Sandbox::create(&spec, "b", &root).await.unwrap();
    assert_eq!(prune(&spec.run_id).await.unwrap(), 2);
    assert_eq!(prune(&spec.run_id).await.unwrap(), 0);
}

#[tokio::test]
#[ignore = "requires Docker and HARNESS_IMAGE"]
async fn shell_timeout_kills_descendants_and_bounds_output() {
    let f = Fixture::new("timeout");
    let root = f.dir.join("root");
    let sandbox = Sandbox::create(&f.spec(), "timeout", &root).await.unwrap();
    // A TERM-resistant grandchild must receive the timeout's group KILL.
    let output = sandbox
        .shell(
            "bash -c 'trap \"\" TERM; sleep 300' & echo $! > child.pid; wait",
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(
        output.contains("exit status: 137") || output.contains("exit status: 124"),
        "{output}"
    );
    let (_, alive) = sh(
        &sandbox,
        "ps -eo stat,args | awk '/sleep 300/ && !/awk/ && $1 !~ /^Z/ {print}'",
    )
    .await;
    assert!(alive.is_empty(), "descendant survived timeout: {alive}");
    let output = sandbox
        .shell(
            "python3 -c 'print(\"x\" * 10000000)'",
            std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(output.len() < 70_000, "unbounded shell output");
    assert!(output.contains("bytes omitted"));
    sandbox.close().await.unwrap();
}
