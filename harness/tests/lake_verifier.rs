//! Real verification boundary tests. Requires Lean 4.35+, bwrap and user systemd.
//! cargo test --test lake_verifier -- --ignored --nocapture

use harness::verify::LeanVerifier;
use std::{fs, path::PathBuf, sync::Arc};
use worktrees::{Error, Options, Pool, RelPath};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("lake-verifier-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("template")).unwrap();
        let template = root.join("template");
        fs::write(
            template.join("lean-toolchain"),
            std::env::var("HARNESS_LEAN_TOOLCHAIN")
                .unwrap_or_else(|_| "leanprover/lean4:v4.35.0".into())
                + "\n",
        )
        .unwrap();
        fs::write(
            template.join("lakefile.toml"),
            "name = \"verifier_test\"\n[[lean_lib]]\nname = \"Proofs\"\n",
        )
        .unwrap();
        fs::write(template.join("lake-manifest.json"), r#"{"version":"1.2.0","name":"verifier_test","lakeDir":".lake","packagesDir":".lake/packages","packages":[]}"#).unwrap();
        fs::create_dir(template.join("Proofs")).unwrap();
        fs::write(
            template.join("Proofs/Challenge.lean"),
            "def value : Nat := 7\ntheorem target : value = 7 := by sorry\n",
        )
        .unwrap();
        fs::write(template.join("Proofs/Candidate.lean"), "").unwrap();
        fs::write(template.join("config.json"), r#"{"challenge_module":"Proofs.Challenge","solution_module":"Proofs.Candidate","theorem_names":["target"],"permitted_axioms":["propext","Quot.sound","Classical.choice"]}"#).unwrap();
        Self(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires installed Lean 4.35, Bubblewrap and a systemd user session"]
fn bundled_verifier_enforces_the_trust_boundary() {
    let fixture = Fixture::new();
    let template = fixture.0.join("template");
    let deps = fixture.0.join("packages");
    fs::create_dir(&deps).unwrap();
    fs::write(deps.join("marker"), "trusted").unwrap();
    let mut verifier = LeanVerifier::from_template(&template).unwrap();
    verifier.deps = Some(deps.clone());
    let store = fixture.0.join("store");
    fs::create_dir(&store).unwrap();
    let pool = Pool::create(&template, &store, Options::default(), Arc::new(verifier)).unwrap();
    let task = pool
        .task(RelPath::new("Proofs/Candidate.lean").unwrap())
        .unwrap();
    let cases = [
        (
            "valid",
            "def value : Nat := 7\ntheorem target : value = 7 := rfl",
            true,
        ),
        (
            "sorry",
            "def value : Nat := 7\ntheorem target : value = 7 := by sorry",
            false,
        ),
        (
            "statement",
            "def value : Nat := 7\ntheorem target : True := trivial",
            false,
        ),
        (
            "definition",
            "def value : Nat := 3 + 4\ntheorem target : value = 7 := rfl",
            false,
        ),
        (
            "axiom",
            "def value : Nat := 7\naxiom cheat : value = 7\ntheorem target : value = 7 := cheat",
            false,
        ),
        (
            "native",
            "def value : Nat := 7\ntheorem target : value = 7 := by native_decide",
            false,
        ),
        ("missing", "def value : Nat := 7", false),
        (
            "syntax",
            "def value : Nat := 7\ntheorem target : value = 7 := by unknown_tactic",
            false,
        ),
        (
            "shared_write",
            "#eval IO.FS.writeFile \".lake/packages/marker\" \"tampered\"\ndef value : Nat := 7\ntheorem target : value = 7 := rfl",
            false,
        ),
        (
            "fake_success",
            "#eval IO.println \"Your solution is okay!\"\ndef value : Nat := 7\ntheorem target : value = 7 := by sorry",
            false,
        ),
    ];
    for (name, source, accepted) in cases {
        let mut attempt = task.attempt(name).unwrap();
        attempt.write(source.as_bytes()).unwrap();
        // The agent's dependency output and challenge edits must not survive freeze.
        fs::write(attempt.root().join("Proofs/Challenge.lean"), source).unwrap();
        let result = attempt.freeze().unwrap().verify().map(|_| ());
        if accepted {
            assert!(result.is_ok(), "{name}: {result:?}");
        } else if name == "missing" {
            // Lean 4.35's exporter panics for a missing declaration. Preserve
            // that operational failure rather than treating crashes as judgments.
            assert!(
                matches!(result, Err(Error::VerifierFailed(_) | Error::Rejected(_))),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(Error::Rejected(_))),
                "{name}: {result:?}"
            );
        }
        assert_eq!(fs::read_to_string(deps.join("marker")).unwrap(), "trusted");
        eprintln!(
            "{name}: {}",
            if accepted {
                "accepted"
            } else if name == "missing" {
                "exporter failure (not accepted)"
            } else {
                "rejected"
            }
        );
    }
    fs::write(
        template.join("lean-toolchain"),
        "leanprover/lean4:v4.34.1\n",
    )
    .unwrap();
    assert!(
        LeanVerifier::from_template(&template)
            .err()
            .unwrap()
            .to_string()
            .contains("4.35")
    );
    fs::write(template.join("config.json"), "{}").unwrap();
    assert!(LeanVerifier::from_template(&template).is_err());
}
