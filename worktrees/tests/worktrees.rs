//! Public-API tests. This binary also serves as its own subprocess fixture
//! (see `subprocess_worker`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use worktrees::{
    Error, Options, Pool, PublishOutcome, RelPath, Task, Verification, Verified, Verifier,
    VerifyError,
};

struct Accept;
impl Verifier for Accept {
    fn policy_id(&self) -> &str {
        "tests/accept-v1"
    }
    fn verify(&self, _: &Verification<'_>) -> Result<String, VerifyError> {
        Ok("accepted".into())
    }
}
struct Check(fn(&Verification<'_>) -> Result<String, VerifyError>);
impl Verifier for Check {
    fn policy_id(&self) -> &str {
        "tests/check-v1"
    }
    fn verify(&self, input: &Verification<'_>) -> Result<String, VerifyError> {
        self.0(input)
    }
}

struct Fixture {
    dir: PathBuf,
    source: PathBuf,
    store: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "worktrees-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let source = dir.join("template");
        let store = dir.join("runs");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("Solution.lean"), "original").unwrap();
        fs::write(source.join("Challenge.lean"), "trusted challenge").unwrap();
        fs::write(source.join("lakefile.toml"), "trusted config").unwrap();
        fs::create_dir(&store).unwrap();
        Self { dir, source, store }
    }
    fn pool(&self, verifier: Arc<dyn Verifier>) -> Pool {
        Pool::create(&self.source, &self.store, Options::default(), verifier).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn task(pool: &Pool) -> Task {
    pool.task(RelPath::new("Solution.lean").unwrap()).unwrap()
}
fn verified(task: &Task, bytes: &[u8]) -> Verified {
    let mut a = task.attempt("worker").unwrap();
    a.write(bytes).unwrap();
    a.freeze().unwrap().verify().unwrap()
}

#[test]
fn rejects_bad_paths() {
    for bad in [
        "", "/x", "../x", "x/../y", "./x", "x//y", "x/", "x\\y", "x\0y",
    ] {
        assert!(RelPath::new(bad).is_err(), "accepted {bad:?}");
    }
    assert!(RelPath::new("A/B.lean").is_ok());
}

#[test]
fn attempts_and_template_are_independent() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let t = task(&pool);
    let mut a = t.attempt("a").unwrap();
    let b = t.attempt("b").unwrap();
    a.write(b"changed").unwrap();
    assert_eq!(
        fs::read(b.root().join("Solution.lean")).unwrap(),
        b"original"
    );
    assert_eq!(
        fs::read(f.source.join("Solution.lean")).unwrap(),
        b"original"
    );
    let root = a.root();
    drop(a);
    assert!(!root.exists());
}

#[test]
fn verifier_sees_fresh_template_and_only_submitted_target() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Check(|input| {
        assert_eq!(
            fs::read(input.root().join("Challenge.lean"))?,
            b"trusted challenge"
        );
        assert_eq!(fs::read(input.root().join("Solution.lean"))?, b"proof");
        assert!(!input.root().join(".lake").exists());
        Ok("fresh".into())
    })));
    let t = task(&pool);
    let mut a = t.attempt("worker").unwrap();
    fs::write(a.root().join("Challenge.lean"), "tampered").unwrap();
    fs::create_dir(a.root().join(".lake")).unwrap();
    fs::write(a.root().join(".lake/poison.olean"), "bad").unwrap();
    a.write(b"proof").unwrap();
    t.publish(a.freeze().unwrap().verify().unwrap()).unwrap();
    assert_eq!(
        fs::read(t.winner().unwrap().unwrap().root().join("Challenge.lean")).unwrap(),
        b"trusted challenge"
    );
}

#[test]
fn frozen_bytes_do_not_follow_a_saved_file_handle() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let t = task(&pool);
    let mut a = t.attempt("a").unwrap();
    a.write(b"proof").unwrap();
    let mut old = OpenOptions::new()
        .write(true)
        .open(a.root().join("Solution.lean"))
        .unwrap();
    let frozen = a.freeze().unwrap();
    old.write_all(b"other").unwrap();
    assert_eq!(frozen.bytes(), b"proof");
    let result = t.publish(frozen.verify().unwrap()).unwrap();
    assert_eq!(fs::read(result.published().file()).unwrap(), b"proof");
}

#[test]
fn concurrent_publication_has_exactly_one_winner() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let t = task(&pool);
    let barrier = Arc::new(Barrier::new(12));
    let workers: Vec<_> = (0..12)
        .map(|i| {
            let t = t.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let v = verified(&t, format!("proof-{i}").as_bytes());
                barrier.wait();
                t.publish(v).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, PublishOutcome::Committed(_)))
            .count(),
        1
    );
    let bytes = fs::read(results[0].published().file()).unwrap();
    for r in results {
        assert_eq!(fs::read(r.published().file()).unwrap(), bytes);
    }
}

#[test]
fn reopened_controllers_share_winner_selection() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let reopened = Pool::open(pool.store(), Options::default(), Arc::new(Accept)).unwrap();
    let a = task(&pool);
    let b = task(&reopened);
    let va = verified(&a, b"first");
    let vb = verified(&b, b"second");
    assert!(matches!(
        a.publish(va).unwrap(),
        PublishOutcome::Committed(_)
    ));
    assert!(matches!(
        b.publish(vb).unwrap(),
        PublishOutcome::AlreadyWon(_)
    ));
    assert_eq!(
        fs::read(b.winner().unwrap().unwrap().file()).unwrap(),
        b"first"
    );
}

#[test]
fn rejects_cross_task_and_cross_pool_tokens() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let a = task(&pool);
    let other = pool.task(RelPath::new("Other.lean").unwrap()).unwrap();
    assert!(matches!(
        other.publish(verified(&a, b"proof")),
        Err(Error::WrongTask)
    ));
    let pool2 = f.pool(Arc::new(Accept));
    assert!(matches!(
        task(&pool2).publish(verified(&a, b"proof")),
        Err(Error::WrongTask)
    ));
}

#[test]
fn rejection_never_creates_a_winner() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Check(|_| {
        Err(VerifyError::Rejected("no proof".into()))
    })));
    let t = task(&pool);
    let a = t.attempt("worker").unwrap();
    assert!(matches!(
        a.freeze().unwrap().verify(),
        Err(Error::Rejected(_))
    ));
    assert!(t.winner().unwrap().is_none());
    assert_eq!(
        fs::read_dir(pool.store().join("scratch")).unwrap().count(),
        0
    );
}

#[test]
fn verifier_source_mutation_is_rejected() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Check(|input| {
        fs::write(input.root().join("Challenge.lean"), "changed")?;
        Ok("claim success".into())
    })));
    let a = task(&pool).attempt("worker").unwrap();
    assert!(matches!(
        a.freeze().unwrap().verify(),
        Err(Error::VerificationInputsChanged)
    ));
}

#[test]
fn verifier_may_write_private_build_outputs() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Check(|input| {
        fs::create_dir(input.root().join(".lake"))?;
        fs::write(input.root().join(".lake/output.olean"), "compiled")?;
        Ok("accepted".into())
    })));
    let t = task(&pool);
    t.publish(verified(&t, b"proof")).unwrap();
    assert!(!t.winner().unwrap().unwrap().root().join(".lake").exists());
}

#[test]
fn final_symlink_cannot_escape_for_read_or_write() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let t = task(&pool);
    let mut a = t.attempt("worker").unwrap();
    fs::remove_file(a.root().join("Solution.lean")).unwrap();
    symlink(
        f.source.join("Solution.lean"),
        a.root().join("Solution.lean"),
    )
    .unwrap();
    assert!(a.write(b"damage").is_err());
    assert!(a.freeze().is_err());
    assert_eq!(
        fs::read(f.source.join("Solution.lean")).unwrap(),
        b"original"
    );
}

#[test]
fn final_symlink_inside_the_attempt_is_not_followed() {
    // Unlike an escaping link, this one would be permitted by a sandboxed
    // resolver, so only O_NOFOLLOW rejects it.
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let t = task(&pool);
    let mut a = t.attempt("worker").unwrap();
    fs::remove_file(a.root().join("Solution.lean")).unwrap();
    symlink("Challenge.lean", a.root().join("Solution.lean")).unwrap();
    assert!(a.write(b"damage").is_err());
    assert_eq!(
        fs::read(a.root().join("Challenge.lean")).unwrap(),
        b"trusted challenge"
    );
    assert!(matches!(a.freeze(), Err(Error::Io(_))));
}

#[test]
fn parent_symlink_cannot_escape() {
    let f = Fixture::new();
    fs::create_dir(f.source.join("Nested")).unwrap();
    fs::write(f.source.join("Nested/P.lean"), "original").unwrap();
    let pool = f.pool(Arc::new(Accept));
    let t = pool.task(RelPath::new("Nested/P.lean").unwrap()).unwrap();
    let mut a = t.attempt("worker").unwrap();
    fs::remove_dir_all(a.root().join("Nested")).unwrap();
    symlink(f.source.join("Nested"), a.root().join("Nested")).unwrap();
    assert!(a.write(b"damage").is_err());
    assert!(a.freeze().is_err());
    assert_eq!(
        fs::read(f.source.join("Nested/P.lean")).unwrap(),
        b"original"
    );
}

#[test]
fn template_symlinks_are_rejected_but_excluded_caches_are_not_traversed() {
    let f = Fixture::new();
    symlink("/does/not/exist", f.source.join(".lake")).unwrap();
    let _ = f.pool(Arc::new(Accept));
    symlink("Challenge.lean", f.source.join("linked.lean")).unwrap();
    assert!(matches!(
        Pool::create(&f.source, &f.store, Options::default(), Arc::new(Accept)),
        Err(Error::UnsupportedEntry(_))
    ));
}

#[test]
fn capture_limits_are_enforced() {
    let f = Fixture::new();
    let options = Options {
        max_file_bytes: 4,
        ..Options::default()
    };
    assert!(matches!(
        Pool::create(&f.source, &f.store, options, Arc::new(Accept)),
        Err(Error::LimitExceeded(_))
    ));
    let pool = f.pool(Arc::new(Accept));
    let mut a = task(&pool).attempt("a").unwrap();
    assert!(matches!(
        a.write(&vec![0; 16 * 1024 * 1024 + 1]),
        Err(Error::LimitExceeded(_))
    ));
}

#[test]
fn snapshot_is_fixed_even_if_original_changes() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    fs::write(f.source.join("Challenge.lean"), "new revision").unwrap();
    let a = task(&pool).attempt("a").unwrap();
    assert_eq!(
        fs::read(a.root().join("Challenge.lean")).unwrap(),
        b"trusted challenge"
    );
}

#[test]
fn reopen_rejects_changed_policy_or_options() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    assert!(matches!(
        Pool::open(
            pool.store(),
            Options::default(),
            Arc::new(Check(|_| Ok("ok".into())))
        ),
        Err(Error::PolicyMismatch)
    ));
    let options = Options {
        max_depth: 32,
        ..Options::default()
    };
    assert!(matches!(
        Pool::open(pool.store(), options, Arc::new(Accept)),
        Err(Error::PolicyMismatch)
    ));
}

#[test]
fn empty_directories_and_executable_bits_survive() {
    let f = Fixture::new();
    fs::create_dir(f.source.join("Empty")).unwrap();
    fs::write(f.source.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(f.source.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let pool = f.pool(Arc::new(Accept));
    let t = task(&pool);
    let out = t.publish(verified(&t, b"proof")).unwrap();
    assert!(out.published().root().join("Empty").is_dir());
    assert_eq!(
        fs::metadata(out.published().root().join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
}

#[test]
fn new_target_can_be_published_without_modifying_template() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let t = pool.task(RelPath::new("New.lean").unwrap()).unwrap();
    let out = t.publish(verified(&t, b"new proof")).unwrap();
    assert_eq!(fs::read(out.published().file()).unwrap(), b"new proof");
    assert!(!f.source.join("New.lean").exists());
}

#[test]
fn invalid_and_excluded_targets_are_rejected() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    for target in [".lake/X.lean", "Missing/X.lean"] {
        assert!(pool.task(RelPath::new(target).unwrap()).is_err());
    }
}

#[test]
fn panic_in_verifier_cleans_up_without_publishing() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Check(|_| panic!("verifier panic"))));
    let t = task(&pool);
    let frozen = t.attempt("a").unwrap().freeze().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = frozen.verify();
    }));
    assert!(result.is_err());
    assert!(t.winner().unwrap().is_none());
    assert_eq!(
        fs::read_dir(pool.store().join("scratch")).unwrap().count(),
        0
    );
}

// Ordinary test runs skip this body; only `child` sets its environment keys.
#[test]
fn subprocess_worker() {
    let Some(run) = std::env::var_os("WORKTREES_TEST_CHILD_RUN") else {
        return;
    };
    let pool = Pool::open(Path::new(&run), Options::default(), Arc::new(Accept)).unwrap();
    let t = task(&pool);
    let id = std::env::var("WORKTREES_TEST_CHILD_ID").unwrap();
    let mut attempt = t.attempt(&id).unwrap();
    attempt.write(id.as_bytes()).unwrap();
    if id == "crash" {
        std::process::exit(23);
    }
    let out = t
        .publish(attempt.freeze().unwrap().verify().unwrap())
        .unwrap();
    if matches!(out, PublishOutcome::Committed(_)) {
        println!("CHILD_COMMITTED");
    }
}

fn child(pool: &Pool, id: &str) -> std::process::Child {
    // current_exe requires procfs on Linux; argv[0] is also available to
    // the Cargo test harness in minimal containers without procfs.
    let executable = std::env::current_exe()
        .or_else(|_| fs::canonicalize(std::env::args_os().next().expect("test executable")))
        .unwrap();
    std::process::Command::new(executable)
        .args(["--exact", "subprocess_worker", "--nocapture"])
        .env("WORKTREES_TEST_CHILD_RUN", pool.store())
        .env("WORKTREES_TEST_CHILD_ID", id)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn separate_processes_select_exactly_one_winner() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let workers: Vec<_> = (0..4)
        .map(|i| child(&pool, &format!("child-{i}")))
        .collect();
    let mut committed = 0;
    for worker in workers {
        let output = worker.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        committed += String::from_utf8_lossy(&output.stdout)
            .matches("CHILD_COMMITTED")
            .count();
    }
    assert_eq!(committed, 1);
    assert!(task(&pool).winner().unwrap().is_some());
}

#[test]
fn exited_controller_releases_lease_for_recovery() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    assert_eq!(
        child(&pool, "crash")
            .wait_with_output()
            .unwrap()
            .status
            .code(),
        Some(23)
    );
    assert_eq!(pool.prune_abandoned().unwrap(), 1);
    assert!(task(&pool).winner().unwrap().is_none());
}

#[test]
fn allocation_and_pruning_can_run_concurrently() {
    let f = Fixture::new();
    let pool = f.pool(Arc::new(Accept));
    let pruner_pool = pool.clone();
    let pruner = thread::spawn(move || {
        for _ in 0..40 {
            pruner_pool.prune_abandoned().unwrap();
        }
    });
    for _ in 0..40 {
        let mut a = task(&pool).attempt("live").unwrap();
        a.write(b"proof").unwrap();
        assert_eq!(a.freeze().unwrap().bytes(), b"proof");
    }
    pruner.join().unwrap();
}

#[test]
fn store_inside_template_is_rejected_before_mutation() {
    let f = Fixture::new();
    assert!(matches!(
        Pool::create(&f.source, &f.source, Options::default(), Arc::new(Accept)),
        Err(Error::InvalidConfiguration(_))
    ));
    assert_eq!(fs::read_dir(&f.source).unwrap().count(), 3);
}

#[test]
fn additional_files_are_frozen_together_and_bound_to_pool_metadata() {
    let f = Fixture::new();
    let extra = RelPath::new("Aux.lean").unwrap();
    fs::write(f.source.join(extra.as_path()), b"original").unwrap();
    let options = Options {
        additional_targets: [extra.clone()].into(),
        ..Options::default()
    };
    let pool = Pool::create(&f.source, &f.store, options.clone(), Arc::new(Accept)).unwrap();
    let t = task(&pool);
    let mut attempt = t.attempt("pair").unwrap();
    fs::write(attempt.root().join("Challenge.lean"), b"discard this").unwrap();
    fs::write(attempt.root().join(extra.as_path()), b"second").unwrap();
    attempt.write(b"first").unwrap();
    let frozen = attempt.freeze().unwrap();
    assert_eq!(frozen.files()[&extra].as_ref(), b"second");
    t.publish(frozen.verify().unwrap()).unwrap();
    let reopened = Pool::open(pool.store(), options, Arc::new(Accept)).unwrap();
    let published = task(&reopened).winner().unwrap().unwrap();
    assert_eq!(fs::read(published.file()).unwrap(), b"first");
    assert_eq!(
        fs::read(published.root().join(extra.as_path())).unwrap(),
        b"second"
    );
    assert_eq!(
        fs::read(published.root().join("Challenge.lean")).unwrap(),
        b"trusted challenge"
    );
    assert!(matches!(
        Pool::open(pool.store(), Options::default(), Arc::new(Accept)),
        Err(Error::PolicyMismatch)
    ));
    let incomplete = t.attempt("missing file").unwrap();
    fs::remove_file(incomplete.root().join(extra.as_path())).unwrap();
    assert!(incomplete.freeze().is_err());
}
