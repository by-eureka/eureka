//! Exercise the public MCP surface with a qualified native runtime.
//! Set HARNESS_BEAM_MCP; HARNESS_LEAN_TOOLCHAIN can select another compiler.
//! HARNESS_TEST_TEMPLATE defaults to examples/core; use examples/simple and
//! HARNESS_PACKAGES to additionally exercise prepared Mathlib integration.
//! cargo test --test beam_surface -- --ignored --nocapture

use harness::beam::{Beam, host_server};
use harness::lean_text::command_line;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("beam-surface-{}", std::process::id()));
        let template = std::env::var_os("HARNESS_TEST_TEMPLATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/core"));
        copy_dir(&template, &dir);
        if let Ok(toolchain) = std::env::var("HARNESS_LEAN_TOOLCHAIN") {
            std::fs::write(dir.join("lean-toolchain"), toolchain + "\n").unwrap();
        }
        if let Some(packages) = std::env::var_os("HARNESS_PACKAGES") {
            std::fs::create_dir_all(dir.join(".lake")).unwrap();
            std::os::unix::fs::symlink(packages, dir.join(".lake/packages")).unwrap();
        }
        Self(dir)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    // Source only: skip build outputs and any old Beam session.
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == ".lake" || name == ".beam" {
            continue;
        }
        let dest = to.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn mcp_binary() -> PathBuf {
    std::env::var_os("HARNESS_BEAM_MCP")
        .map(PathBuf::from)
        .expect("set HARNESS_BEAM_MCP to the pinned native runtime's lean-beam-mcp")
}

async fn call(beam: &Beam, _root: &Path, name: &str, args: Value) -> Value {
    let text = beam.call(name, &args).await.unwrap();
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("{name} returned non-JSON: {text}"))
}

#[tokio::test]
#[ignore = "requires HARNESS_BEAM_MCP pointing at a qualified native runtime"]
async fn every_lean_operation_works() {
    let mcp = mcp_binary();
    let project = Project::new();
    let root = project.0.as_path();
    let beam = Beam::spawn(host_server(&mcp), root).await.unwrap();

    assert!(
        !beam.instructions().is_empty(),
        "missing shared MCP instructions"
    );
    let names: Vec<&str> = beam.tools().iter().map(|t| t.name.as_str()).collect();
    for expected in [
        "lean_update",
        "lean_sync",
        "lean_refresh",
        "lean_save",
        "lean_close",
        "lean_close_save",
        "lean_run_at",
        "lean_run_at_handle",
        "lean_run_with",
        "lean_run_with_linear",
        "lean_release",
        "lean_goals",
        "lean_hover",
        "lean_signature_help",
        "lean_definition",
        "lean_references",
        "lean_document_symbols",
        "lean_workspace_symbols",
        "lean_todo",
        "lean_code_action_resolve",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    // The model never sees the workspace descriptor.
    for tool in beam.tools() {
        assert!(
            tool.input_schema["properties"].get("workspace").is_none(),
            "{}",
            tool.name
        );
    }

    // Document lifecycle. The sorries are only warnings, so the file is save_ready.
    let version = call(
        &beam,
        root,
        "lean_update",
        json!({"path": "Proofs/Solution.lean"}),
    )
    .await["version"]
        .as_u64()
        .unwrap();
    let sync = call(
        &beam,
        root,
        "lean_sync",
        json!({"path": "Proofs/Solution.lean", "diagnostic_scope": "all", "diagnostics_in_result": true}),
    )
    .await;
    assert_eq!(sync["readiness"]["save_ready"], true);
    assert_eq!(
        sync["diagnostics"]["counts"]["warning"], 2,
        "two sorries expected"
    );

    // Proof state and speculative execution at the first tactic (line 1, char 2).
    let goals = call(&beam, root, "lean_goals", json!({"path": "Proofs/Solution.lean", "version": version, "line": 1, "character": 2, "mode": "before"})).await;
    assert_eq!(goals["goals"][0]["target"], "a + b = b + a");
    let ok = call(&beam, root, "lean_run_at", json!({"path": "Proofs/Solution.lean", "version": version, "line": 1, "character": 2, "text": "omega"})).await;
    assert_eq!(ok["success"], true);
    assert_eq!(ok["proof_state"]["goals"], json!([]));
    let bad = call(&beam, root, "lean_run_at", json!({"path": "Proofs/Solution.lean", "version": version, "line": 1, "character": 2, "text": "exact rfl"})).await;
    assert_eq!(bad["success"], false);
    assert!(bad["messages"][0]["text"].as_str().unwrap().contains("rfl"));

    // Handles: mint a root, branch twice (non-linear), continue linearly, release.
    let root_handle = call(&beam, root, "lean_run_at_handle", json!({"path": "Proofs/Solution.lean", "version": version, "line": 1, "character": 2, "text": "induction a"})).await;
    let handle = root_handle["next_handle"].clone();
    // Handles are opaque. Reusing one branches; a linear call consumes it.
    assert!(handle.is_object(), "no handle in {root_handle}");
    assert_eq!(
        root_handle["proof_state"]["goals"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let left = call(
        &beam,
        root,
        "lean_run_with",
        json!({"path": "Proofs/Solution.lean", "handle": handle, "text": "simp"}),
    )
    .await;
    assert_eq!(left["success"], true, "{left}");
    let right = call(
        &beam,
        root,
        "lean_run_with",
        json!({"path": "Proofs/Solution.lean", "handle": handle, "text": "omega"}),
    )
    .await;
    assert_eq!(right["success"], true, "{right}");
    let step = call(
        &beam,
        root,
        "lean_run_with_linear",
        json!({"path": "Proofs/Solution.lean", "handle": left["next_handle"], "text": "omega"}),
    )
    .await;
    assert_eq!(step["success"], true, "{step}");
    assert_eq!(
        step["proof_state"]["goals"],
        json!([]),
        "all goals closed: {step}"
    );
    for h in [
        handle,
        right["next_handle"].clone(),
        step["next_handle"].clone(),
    ] {
        if h.is_object() {
            call(
                &beam,
                root,
                "lean_release",
                json!({"path": "Proofs/Solution.lean", "handle": h}),
            )
            .await;
        }
    }

    // A command probe at the position lean_check uses sees the file's own declarations.
    let source = std::fs::read_to_string(root.join("Proofs/Solution.lean")).unwrap();
    let at = command_line(&source);
    let seen = call(&beam, root, "lean_run_at", json!({"path": "Proofs/Solution.lean", "version": version, "line": at, "character": 0, "text": "#check @add_comm_nat"})).await;
    assert_eq!(seen["success"], true, "{seen}");
    assert!(
        seen["messages"][0]["text"]
            .as_str()
            .unwrap()
            .contains("a + b = b + a"),
        "{seen}"
    );
    let ex = call(&beam, root, "lean_run_at", json!({"path": "Proofs/Solution.lean", "version": version, "line": at, "character": 0, "text": "example (a b : Nat) : a + b = b + a := by\n  omega"})).await;
    assert_eq!(ex["success"], true, "{ex}");

    // Navigation and symbols.
    let hover = call(
        &beam,
        root,
        "lean_hover",
        json!({"path": "Proofs/Solution.lean", "version": version, "line": 0, "character": 8}),
    )
    .await;
    assert!(hover.to_string().contains("a + b = b + a"), "{hover}");
    call(
        &beam,
        root,
        "lean_signature_help",
        json!({"path": "Proofs/Solution.lean", "version": version, "line": 0, "character": 40}),
    )
    .await;
    let def = call(
        &beam,
        root,
        "lean_definition",
        json!({"path": "Proofs/Solution.lean", "version": version, "line": 0, "character": 8}),
    )
    .await;
    assert!(def.to_string().contains("Proofs/Solution.lean"), "{def}");
    call(&beam, root, "lean_references", json!({"path": "Proofs/Solution.lean", "version": version, "line": 0, "character": 8, "include_declaration": true})).await;
    let symbols = call(
        &beam,
        root,
        "lean_document_symbols",
        json!({"path": "Proofs/Solution.lean", "version": version}),
    )
    .await;
    assert!(symbols.to_string().contains("two_mul_eq"), "{symbols}");
    let ws = call(
        &beam,
        root,
        "lean_workspace_symbols",
        json!({"query": "Nat.add_comm"}),
    )
    .await;
    assert!(ws.to_string().contains("add_comm"), "{ws}");

    // Whole-file todo no longer requires a range or a suggestion mode.
    let todo = call(
        &beam,
        root,
        "lean_todo",
        json!({"path": "Proofs/Solution.lean", "version": version}),
    )
    .await;
    let items = todo["items"].as_array().unwrap();
    assert!(items.len() >= 2, "{todo}");
    let sorry = items
        .iter()
        .find(|i| i["kind"] == "sorry")
        .expect("syntactic sorry item");
    assert_eq!(sorry["run_at_position"], json!({"line": 1, "character": 2}));

    // An identical probe with an invalid version must reach Beam and fail.
    let stale = beam.call("lean_goals", &json!({"path": "Proofs/Solution.lean", "version": version + 1000, "line": 1, "character": 2, "mode": "before"})).await.unwrap_err();
    let stale = format!("{stale:#}");
    assert!(
        stale.contains("contentModified"),
        "stale version accepted: {stale}"
    );

    // Real edit, then checkpoint and close.
    std::fs::write(root.join("Proofs/Solution.lean"), "theorem add_comm_nat (a b : Nat) : a + b = b + a := by\n  omega\n\ntheorem two_mul_eq (n : Nat) : 2 * n = n + n := by\n  omega\n").unwrap();
    let v2 = call(
        &beam,
        root,
        "lean_update",
        json!({"path": "Proofs/Solution.lean"}),
    )
    .await["version"]
        .as_u64()
        .unwrap();
    assert!(v2 > version);
    let refreshed = call(
        &beam,
        root,
        "lean_refresh",
        json!({"path": "Proofs/Solution.lean"}),
    )
    .await;
    assert_eq!(refreshed["readiness"]["save_ready"], true, "{refreshed}");
    let saved = call(
        &beam,
        root,
        "lean_save",
        json!({"path": "Proofs/Solution.lean"}),
    )
    .await;
    assert!(
        saved.to_string().contains("olean") || saved["sync"]["readiness"]["save_ready"] == true,
        "{saved}"
    );
    call(
        &beam,
        root,
        "lean_close_save",
        json!({"path": "Proofs/Solution.lean"}),
    )
    .await;
    call(
        &beam,
        root,
        "lean_close",
        json!({"path": "Proofs/Solution.lean"}),
    )
    .await;

    // New upstream capabilities: term holes, profiling, and explicit recovery.
    let holes = root.join("Proofs/Holes.lean");
    let source = "theorem term_hole : True := sorry\n";
    std::fs::write(&holes, source).unwrap();
    let version = call(
        &beam,
        root,
        "lean_update",
        json!({"path":"Proofs/Holes.lean"}),
    )
    .await["version"]
        .as_u64()
        .unwrap();
    let todos = call(
        &beam,
        root,
        "lean_todo",
        json!({"path":"Proofs/Holes.lean", "version":version}),
    )
    .await;
    let item = todos["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "sorry")
        .unwrap();
    let pos = &item["run_at_position"];
    let mut args = json!({"path":"Proofs/Holes.lean", "version":version,
        "line":pos["line"], "character":pos["character"], "text":"trivial", "profile":true});
    let profiled = call(&beam, root, "lean_run_at", args.clone()).await;
    assert_eq!(profiled["success"], true, "{profiled}");
    assert!(
        profiled["profile"]["elapsed_ms"].as_f64().is_some(),
        "{profiled}"
    );
    assert!(profiled["profile"]["spans"].is_array(), "{profiled}");
    assert_eq!(std::fs::read_to_string(&holes).unwrap(), source);

    args["text"] = json!("skip");
    let minted = call(&beam, root, "lean_run_at_handle", args).await;
    let handle = &minted["next_handle"];
    assert!(handle.is_object(), "{minted}");
    let continuation = json!({"path":"Proofs/Holes.lean", "handle":handle, "text":"trivial"});
    assert!(
        beam.call("lean_run_with", &continuation).await.is_err(),
        "profile mode must match"
    );
    let mut continuation = continuation;
    continuation["profile"] = json!(true);
    let done = call(&beam, root, "lean_run_with", continuation.clone()).await;
    assert_eq!(done["success"], true, "{done}");
    assert!(done["profile"].is_object(), "{done}");

    // A real edit invalidates retained state, including a profiled parent.
    std::fs::write(&holes, "theorem term_hole : True := trivial\n").unwrap();
    call(
        &beam,
        root,
        "lean_update",
        json!({"path":"Proofs/Holes.lean"}),
    )
    .await;
    assert!(beam.call("lean_run_with", &continuation).await.is_err());
    call(&beam, root, "lean_drop_workspace", json!({})).await;
    let fresh = call(
        &beam,
        root,
        "lean_sync",
        json!({"path":"Proofs/Holes.lean"}),
    )
    .await;
    assert_eq!(fresh["readiness"]["save_ready"], true, "{fresh}");
}
