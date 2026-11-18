//! Isolated workspaces and first-winner publication for parallel agents.
//!
//! Linux, Rust >= 1.89. Run `cargo test` (including compile-fail examples) and
//! `cargo fmt --check`. Tests that need internals are at the end of this file;
//! public-API tests are in `tests/worktrees.rs`.
//!
//! # Contract
//!
//! A Pool captures a quiescent, trusted source tree once. Each Attempt gets a
//! real copy. An attempt submits its primary target and any additional files
//! declared by the controller. Frozen owns the submitted bytes in memory. The
//! verifier receives a FRESH reconstruction of the trusted tree with only those
//! bytes substituted. Verified is the only input accepted by publication.
//!
//! Source symlinks and special files are rejected. File lookup beneath working
//! trees uses cap-std directory capabilities and Linux O_NOFOLLOW, not a
//! canonicalize-then-open check. Source bytes are bounded. No hard links or
//! shared writable caches are used for workspaces.
//!
//! A winner is a complete project at Published::root(), atomically installed
//! in the pool's result store. The input source tree is never overwritten.
//! Winner selection is per target for this pool's fixed snapshot and policy.
//! Results persist across Pool drops and can be read after Pool::open.
//!
//! # Integration boundary
//!
//! This library is NOT an OS sandbox or process supervisor. Keep the pool store
//! inaccessible to agent processes; expose only each attempt's project root.
//! Run agents and Lean in the runner's sandbox. Stop/join writers before freeze.
//! Live writers can otherwise produce an inconsistent submission, but cannot
//! alter bytes after capture. Never give untrusted Rust code the Pool/verifier.
//! The verifier is trusted code: returning Ok is an assertion of acceptance.
//!
//! By default .git, .lake and lake-packages are excluded. Dependencies, Lean's
//! toolchain, and the verifier executable must be pinned by the runner and
//! represented in the policy ID. Prepare dependency artifacts in each fresh
//! verification workspace using trusted copies/reflinks or enforced read-only
//! mounts plus private outputs. NEVER reuse an agent's .lake for verification.
//! The verifier can write .lake while checking; all non-excluded source inputs
//! are compared byte-for-byte after the verifier returns.
//!
//! For ../ sibling packages, capture a trusted bundle containing the project
//! and its dependency directories. The verifier chooses the project subdir.
//! This library never discovers or symlinks arbitrary parent siblings.
//!
//! Pool creation requires a quiescent template; capture is not a filesystem
//! snapshot. The result store must be private to cooperating controllers, on a
//! local filesystem supporting flock, directory fsync, and atomic rename.
//! All controllers reopening a pool must supply the same Options and policy.
//! A changed snapshot or verification policy requires a NEW pool.
//!
//! Calls are synchronous; execute expensive work on worker threads. There are
//! no async cancellation points during commit. Explicit errors distinguish
//! rejection, operational failure, and a visible commit whose durability is
//! uncertain. Drop cleanup is best effort; prune_abandoned recovers scratch
//! directories after a crash without deleting locked live attempts.
//!
//! # Example
//!
//! ```no_run
//! use std::{path::Path, sync::Arc};
//! use worktrees::{Options, Pool, RelPath, Verification, Verifier, VerifyError};
//!
//! struct LeanAdapter;
//! impl Verifier for LeanAdapter {
//!     fn policy_id(&self) -> &str {
//!         "my-pinned-lake-and-toolchain-and-axiom-policy-v1"
//!     }
//!     fn verify(&self, input: &Verification<'_>) -> Result<String, VerifyError> {
//!         // Trusted runner implementation: invoke `lake comparator` in input.root(),
//!         // with timeout/process-tree supervision and the sandbox required by
//!         // the pinned Lean version. The template contains Challenge.lean,
//!         // Solution.lean, config.json and trusted Lake files. Only Solution.lean
//!         // is a target. Lake reads its JSON config; it is NOT passed
//!         // a bare target filename as though it were a compiler.
//!         // Lean 4.35+ bundles the verifier and requires Bubblewrap on Linux.
//!         let _ = (input.root(), input.target());
//!         Err(VerifyError::Rejected("wire in your supervised runner".into()))
//!     }
//! }
//!
//! let pool = Pool::create(
//!     Path::new("/trusted/template"), Path::new("/private/runs"),
//!     Options::default(), Arc::new(LeanAdapter),
//! )?;
//! let task = pool.task(RelPath::new("Solution.lean")?)?;
//! let mut attempt = task.attempt("agent-1")?;
//! // Or run your agent inside attempt.root(), then stop/join its processes.
//! attempt.write(b"theorem todo1 : True := by trivial\n")?;
//! let verified = attempt.freeze()?.verify()?;
//! let outcome = task.publish(verified)?;
//! println!("winner: {}", outcome.published().root().display());
//! # Ok::<(), worktrees::Error>(())
//! ```
//!
//! An editable attempt cannot be published:
//! ```compile_fail
//! use worktrees::{Attempt, Task};
//! fn wrong(task: &Task, attempt: Attempt) { task.publish(attempt); }
//! ```
//! Freezing consumes the attempt:
//! ```compile_fail
//! use worktrees::Attempt;
//! fn wrong(mut attempt: Attempt) {
//!     let candidate = attempt.freeze();
//!     attempt.write(b"late edit");
//! }
//! ```
//! Verification cannot be forged from outside this module:
//! ```compile_fail
//! use worktrees::Verified;
//! fn wrong() { let _ = Verified {}; }
//! ```

#![forbid(unsafe_code)]
#![deny(unused_must_use)]

#[cfg(not(target_os = "linux"))]
compile_error!("the worktrees crate currently supports Linux");

use cap_std::fs::{
    Dir, OpenOptions as CapOpenOptions, OpenOptionsExt as CapOpenOptionsExt,
    PermissionsExt as CapPermissionsExt,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// Linux open(2) flags used through safe filesystem APIs. Their numeric values
// differ between architectures (e.g. x86-64 vs aarch64), so never hardcode them.
use libc::{O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
const FORMAT: &str = "agent-worktrees-v1\n";
const MAX_REPORT: usize = 1024 * 1024;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A nonempty, normalized, UTF-8 relative path. No '.', '..', empty components,
/// backslashes, absolute paths, or NULs. Construction validates syntax only.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RelPath(String);

impl RelPath {
    pub fn new(path: impl AsRef<str>) -> Result<Self, Error> {
        let s = path.as_ref();
        if s.is_empty()
            || s.len() > 4096
            || s.contains(['\0', '\\'])
            || s.split('/')
                .any(|c| c.is_empty() || c == "." || c == ".." || c.len() > 255)
            || Path::new(s)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(Error::InvalidPath(s.to_owned()));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Limits apply to each captured source tree; excluded entries are never read.
#[derive(Clone, Debug)]
pub struct Options {
    pub max_file_bytes: u64,
    pub max_source_bytes: u64,
    pub max_entries: usize,
    pub max_depth: usize,
    /// Names excluded at ANY depth, including build outputs created by Lean.
    /// These names may not appear in a target path.
    pub excluded_names: BTreeSet<String>,
    /// Additional template files frozen and published alongside the primary target.
    pub additional_targets: BTreeSet<RelPath>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_file_bytes: 16 * 1024 * 1024,
            max_source_bytes: 256 * 1024 * 1024,
            max_entries: 100_000,
            max_depth: 64,
            additional_targets: BTreeSet::new(),
            excluded_names: [".git", ".lake", "lake-packages", "__pycache__", ".DS_Store"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }
    }
}

impl Options {
    fn validate(&self) -> Result<(), Error> {
        if self.max_file_bytes == 0
            || self.max_source_bytes < self.max_file_bytes
            || self.max_entries == 0
            || self.max_depth == 0
            || self.max_depth > 128
            || self.max_file_bytes > usize::MAX as u64 - 1
        {
            return Err(Error::InvalidConfiguration("invalid capture limits".into()));
        }
        for name in &self.excluded_names {
            if RelPath::new(name)?.as_path().components().count() != 1 {
                return Err(Error::InvalidConfiguration(
                    "exclusions must be single names".into(),
                ));
            }
        }
        Ok(())
    }

    fn metadata(&self, policy: &str) -> String {
        // Hex encoding makes embedded newlines unambiguous. This is an identity
        // record, not a digest. Source comparison also uses exact bytes.
        let mut metadata = format!(
            "{FORMAT}{}\n{}\n{}\n{}\n{}\n{}\n",
            hex(policy.as_bytes()),
            self.max_file_bytes,
            self.max_source_bytes,
            self.max_entries,
            self.max_depth,
            self.excluded_names
                .iter()
                .map(|s| hex(s.as_bytes()))
                .collect::<Vec<_>>()
                .join(",")
        );
        // Keep existing single-file pools readable.
        for path in &self.additional_targets {
            metadata.push_str(&format!("file {}\n", hex(path.as_str().as_bytes())));
        }
        metadata
    }
}

#[derive(Debug)]
pub enum VerifyError {
    Rejected(String),
    Operational(io::Error),
}

impl From<io::Error> for VerifyError {
    fn from(e: io::Error) -> Self {
        Self::Operational(e)
    }
}

/// Trusted adapter, bound to the pool at construction. Ok(summary) means the
/// submitted target passed the pinned policy. This trait is NOT implemented by
/// agents. It must wait for all verifier processes before returning, including
/// on errors. On panic, the runner must still clean up its processes.
pub trait Verifier: Send + Sync {
    /// Include checker version, toolchain/dependency identities, theorem set,
    /// challenge configuration, and axiom policy. Must remain stable.
    fn policy_id(&self) -> &str;
    fn verify(&self, input: &Verification<'_>) -> Result<String, VerifyError>;
}

pub struct Verification<'a> {
    root: &'a Path,
    target: &'a RelPath,
    policy: &'a str,
    pool: &'a Pool,
    multi_file: bool,
}

impl Verification<'_> {
    pub fn is_multi_file(&self) -> bool {
        self.multi_file
    }

    /// The trusted pool that owns this verification.
    pub fn pool(&self) -> &Pool {
        self.pool
    }
    pub fn root(&self) -> &Path {
        self.root
    }
    pub fn target(&self) -> &RelPath {
        self.target
    }
    pub fn policy_id(&self) -> &str {
        self.policy
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidPath(String),
    InvalidConfiguration(String),
    UnsupportedEntry(PathBuf),
    LimitExceeded(&'static str),
    PolicyMismatch,
    WrongTask,
    Rejected(String),
    VerifierFailed(io::Error),
    VerificationInputsChanged,
    CorruptStore(PathBuf),
    /// Rename succeeded, so the result IS visible. Directory fsync failed;
    /// persistence across a power loss is unknown. Do not report a rejection.
    CommitUncertain {
        published: Published,
        source: io::Error,
    },
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "filesystem operation failed: {e}"),
            Self::InvalidPath(p) => write!(f, "invalid relative path: {p:?}"),
            Self::InvalidConfiguration(s) => write!(f, "invalid configuration: {s}"),
            Self::UnsupportedEntry(p) => {
                write!(f, "symlink or special entry rejected: {}", p.display())
            }
            Self::LimitExceeded(s) => write!(f, "capture limit exceeded: {s}"),
            Self::PolicyMismatch => write!(f, "pool options or verifier policy do not match"),
            Self::WrongTask => write!(
                f,
                "verified candidate belongs to a different pool or target"
            ),
            Self::Rejected(s) => write!(f, "verification rejected: {s}"),
            Self::VerifierFailed(e) => write!(f, "verifier failed: {e}"),
            Self::VerificationInputsChanged => {
                write!(f, "verifier changed protected source inputs")
            }
            Self::CorruptStore(p) => write!(f, "invalid result store: {}", p.display()),
            Self::CommitUncertain { published, source } => write!(
                f,
                "result visible at {}, but durability is uncertain: {source}",
                published.root().display()
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) | Self::VerifierFailed(e) | Self::CommitUncertain { source: e, .. } => {
                Some(e)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File { bytes: Arc<[u8]>, executable: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot(BTreeMap<RelPath, Entry>);

impl Snapshot {
    fn capture(root: &Path, options: &Options) -> Result<Self, Error> {
        let mut entries = BTreeMap::new();
        let mut total = 0;
        capture_dir(
            &Dir::from_std_file(open_dir(root)?),
            "",
            0,
            options,
            &mut entries,
            &mut total,
        )?;
        Ok(Self(entries))
    }

    fn with_candidate(&self, files: &BTreeMap<RelPath, Arc<[u8]>>) -> Self {
        let mut result = self.clone();
        for (path, bytes) in files {
            let executable = match self.0.get(path) {
                Some(Entry::File { executable, .. }) => *executable,
                _ => 0,
            };
            result.0.insert(
                path.clone(),
                Entry::File {
                    bytes: bytes.clone(),
                    executable,
                },
            );
        }
        result
    }

    fn materialize(&self, root: &Path, durable: bool) -> Result<(), Error> {
        private_dir(root)?;
        for (rel, entry) in &self.0 {
            let path = root.join(rel.as_path());
            match entry {
                Entry::Directory => private_dir(&path)?,
                Entry::File { bytes, executable } => {
                    write_new(&path, bytes, 0o600 | executable, durable)?
                }
            }
        }
        if durable {
            // Child directory entries must be durable before their parents.
            for (rel, entry) in self.0.iter().rev() {
                if matches!(entry, Entry::Directory) {
                    sync_dir(&root.join(rel.as_path()))?;
                }
            }
            sync_dir(root)?;
        }
        Ok(())
    }
}

struct Inner {
    run: PathBuf,
    options: Options,
    snapshot: Snapshot,
    policy: String,
    verifier: Arc<dyn Verifier>,
}

impl Inner {
    fn scratch(&self, prefix: &str) -> io::Result<OwnedDir> {
        // Serialize allocation with pruning until the new lease is locked.
        let _allocation = lock_file(&self.run.join("allocation-lock"))?;
        OwnedDir::create(&self.run.join("scratch"), prefix)
    }
}

/// Cheap to clone and share across worker threads. Completed results survive
/// all Pool handles. Use a separate pool for each baseline/policy revision.
#[derive(Clone)]
pub struct Pool(Arc<Inner>);

impl Pool {
    /// Capture a quiescent template. `parent` must already exist and be owned
    /// by the trusted controller. Allocation never removes pre-existing paths.
    pub fn create(
        template: &Path,
        parent: &Path,
        options: Options,
        verifier: Arc<dyn Verifier>,
    ) -> Result<Self, Error> {
        options.validate()?;
        let policy = verifier.policy_id().to_owned();
        validate_policy(&policy)?;
        let template = fs::canonicalize(template)?;
        let parent = fs::canonicalize(parent)?;
        if parent.starts_with(&template) {
            return Err(Error::InvalidConfiguration(
                "store must be outside the template".into(),
            ));
        }
        let snapshot = Snapshot::capture(&template, &options)?;
        let mut run = OwnedDir::create(&parent, "pool")?;
        snapshot.materialize(&run.path.join("source"), true)?;
        private_dir(&run.path.join("scratch"))?;
        private_dir(&run.path.join("results"))?;
        write_new(
            &run.path.join("metadata"),
            options.metadata(&policy).as_bytes(),
            0o600,
            true,
        )?;
        sync_dir(&run.path.join("scratch"))?;
        sync_dir(&run.path.join("results"))?;
        sync_dir(&run.path)?;
        sync_dir(&parent)?;
        let path = run.path.clone();
        run.persist();
        Ok(Self(Arc::new(Inner {
            run: path,
            options,
            snapshot,
            policy,
            verifier,
        })))
    }

    /// Reopen an existing trusted pool. Does not restore worker handles. The
    /// stored source is authoritative and must be protected from external edits.
    pub fn open(run: &Path, options: Options, verifier: Arc<dyn Verifier>) -> Result<Self, Error> {
        options.validate()?;
        let policy = verifier.policy_id().to_owned();
        validate_policy(&policy)?;
        let run = fs::canonicalize(run)?;
        let expected = options.metadata(&policy);
        let actual = read_beneath(&run, &RelPath::new("metadata")?, MAX_REPORT as u64)?;
        if actual.as_slice() != expected.as_bytes() {
            return Err(Error::PolicyMismatch);
        }
        let snapshot = Snapshot::capture(&run.join("source"), &options)?;
        let _ = open_dir(&run.join("scratch"))?;
        let _ = open_dir(&run.join("results"))?;
        Ok(Self(Arc::new(Inner {
            run,
            options,
            snapshot,
            policy,
            verifier,
        })))
    }

    /// Materialize the pristine source in a new controller-owned directory.
    pub fn write_baseline(&self, root: &Path) -> Result<(), Error> {
        self.0.snapshot.materialize(root, false)
    }

    /// Controller-only path. Do not expose the result store to agents.
    pub fn store(&self) -> &Path {
        &self.0.run
    }

    pub fn task(&self, target: RelPath) -> Result<Task, Error> {
        for path in &self.0.options.additional_targets {
            if path == &target || !matches!(self.0.snapshot.0.get(path), Some(Entry::File { .. })) {
                return Err(Error::InvalidConfiguration(format!(
                    "expected a distinct template file: {path}"
                )));
            }
        }
        if target
            .0
            .split('/')
            .any(|c| self.0.options.excluded_names.contains(c))
        {
            return Err(Error::InvalidConfiguration(
                "target is excluded from the snapshot".into(),
            ));
        }
        if matches!(self.0.snapshot.0.get(&target), Some(Entry::Directory)) {
            return Err(Error::InvalidConfiguration("target is a directory".into()));
        }
        if let Some(parent) = target
            .as_path()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            let parent = RelPath::new(
                parent
                    .to_str()
                    .ok_or_else(|| Error::InvalidPath(target.0.clone()))?,
            )?;
            if !matches!(self.0.snapshot.0.get(&parent), Some(Entry::Directory)) {
                return Err(Error::InvalidConfiguration(
                    "target parent is absent from the template".into(),
                ));
            }
        }
        // Encoded components avoid collisions with the internal 'winner' and
        // 'lock' names, while retaining hierarchy for long module paths.
        let mut result_dir = self.0.run.join("results");
        for part in target.0.split('/') {
            // Chunking avoids NAME_MAX overflow after hex expansion.
            for chunk in part.as_bytes().chunks(100) {
                result_dir.push(format!("c{}", hex(chunk)));
            }
            result_dir.push("end");
        }
        fs::create_dir_all(&result_dir)?;
        Ok(Task {
            pool: self.clone(),
            target,
            result_dir,
        })
    }

    /// Remove abandoned scratch directories only. Per-directory OS locks keep
    /// live attempts/verifications/commits safe, including across controllers.
    /// Does not kill processes; the runner must reap orphans before calling.
    pub fn prune_abandoned(&self) -> Result<usize, Error> {
        let _allocation = lock_file(&self.0.run.join("allocation-lock"))?;
        let mut count = 0;
        for entry in fs::read_dir(self.0.run.join("scratch"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let lease = match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(O_NOFOLLOW | O_NONBLOCK)
                .open(entry.path().join("lease"))
            {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // No allocator can be between mkdir and lease creation
                    // while we hold allocation-lock. This is a crash remnant.
                    match fs::remove_dir_all(entry.path()) {
                        Ok(()) => count += 1,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            match lease.try_lock() {
                Ok(()) => {
                    fs::remove_dir_all(entry.path())?;
                    count += 1;
                }
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        Ok(count)
    }
}

/// A target within one fixed pool. Clone this to dispatch parallel attempts.
#[derive(Clone)]
pub struct Task {
    pool: Pool,
    target: RelPath,
    result_dir: PathBuf,
}

impl Task {
    pub fn target(&self) -> &RelPath {
        &self.target
    }

    pub fn limits(&self) -> &Options {
        &self.pool.0.options
    }

    pub fn targets(&self) -> impl Iterator<Item = &RelPath> {
        std::iter::once(&self.target).chain(&self.pool.0.options.additional_targets)
    }

    pub fn attempt(&self, agent: impl Into<String>) -> Result<Attempt, Error> {
        let agent = agent.into();
        if agent.len() > MAX_REPORT {
            return Err(Error::LimitExceeded("agent label"));
        }
        let dir = self.pool.0.scratch("attempt")?;
        self.pool
            .0
            .snapshot
            .materialize(&dir.path.join("project"), false)?;
        Ok(Attempt {
            task: self.clone(),
            dir,
            agent,
        })
    }

    /// Lookup is safe during publication: the directory appears only after all
    /// files and metadata are ready. Existing winners are never overwritten.
    pub fn winner(&self) -> Result<Option<Published>, Error> {
        let dir = self.result_dir.join("winner");
        match fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
            Ok(m) if m.is_dir() => {
                let policy = read_beneath(&dir, &RelPath::new("policy")?, MAX_REPORT as u64)?;
                let target = read_beneath(&dir, &RelPath::new("target")?, 4096)?;
                if policy != self.pool.0.policy.as_bytes()
                    || target != self.target.0.as_bytes()
                    || open_dir(&dir.join("project")).is_err()
                {
                    return Err(Error::CorruptStore(dir));
                }
                Ok(Some(Published {
                    dir,
                    target: self.target.clone(),
                }))
            }
            Ok(_) => Err(Error::CorruptStore(dir)),
        }
    }

    /// Consume verified bytes and publish one durable result. The OS lock spans
    /// winner lookup, rename and fsync. Verification runs outside this lock.
    pub fn publish(&self, verified: Verified) -> Result<PublishOutcome, Error> {
        if !Arc::ptr_eq(&self.pool.0, &verified.frozen.task.pool.0)
            || self.target != verified.frozen.task.target
        {
            return Err(Error::WrongTask);
        }
        let mut stage = self.pool.0.scratch("publish")?;
        self.pool
            .0
            .snapshot
            .with_candidate(&verified.frozen.files)
            .materialize(&stage.path.join("project"), true)?;
        for (name, bytes) in [
            ("policy", self.pool.0.policy.as_bytes()),
            ("target", self.target.0.as_bytes()),
            ("agent", verified.frozen.agent.as_bytes()),
            ("report", verified.report.as_bytes()),
        ] {
            write_new(&stage.path.join(name), bytes, 0o600, true)?;
        }
        sync_dir(&stage.path)?;

        let _lock = lock_file(&self.result_dir.join("lock"))?;
        if let Some(winner) = self.winner()? {
            self.ensure_durable(&winner)?;
            return Ok(PublishOutcome::AlreadyWon(winner));
        }
        let dest = self.result_dir.join("winner");
        #[cfg(test)]
        commit_fault(1)?;
        fs::rename(&stage.path, &dest)?; // same store/filesystem; linearization point
        stage.persist();
        let published = Published {
            dir: dest,
            target: self.target.clone(),
        };
        self.ensure_durable(&published)?;
        Ok(PublishOutcome::Committed(published))
    }

    fn ensure_durable(&self, published: &Published) -> Result<(), Error> {
        // Sync the entire new result path to the pool root, plus the old parent
        // of the renamed directory. Ancestors may have just been created.
        let durable = (|| -> io::Result<()> {
            #[cfg(test)]
            commit_fault(2)?;
            sync_dir(&self.pool.0.run.join("scratch"))?;
            let mut current = Some(self.result_dir.as_path());
            while let Some(path) = current {
                sync_dir(path)?;
                if path == self.pool.0.run {
                    break;
                }
                current = path.parent();
            }
            Ok(())
        })();
        if let Err(source) = durable {
            return Err(Error::CommitUncertain {
                published: published.clone(),
                source,
            });
        }
        Ok(())
    }
}

/// Unique editable attempt. No Clone. Dropping deletes only its private scratch
/// directory. Stop all subprocesses before dropping, discarding, or freezing.
#[must_use]
pub struct Attempt {
    task: Task,
    dir: OwnedDir,
    agent: String,
}

impl Attempt {
    pub fn root(&self) -> PathBuf {
        self.dir.path.join("project")
    }
    pub fn target(&self) -> &RelPath {
        &self.task.target
    }

    /// Convenient controlled write. External editors can use root() instead.
    /// Replaces a regular target atomically; never follows a target symlink.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() as u64 > self.task.pool.0.options.max_file_bytes {
            return Err(Error::LimitExceeded("target bytes"));
        }
        replace_beneath(&self.root(), &self.task.target, bytes)?;
        Ok(())
    }

    /// Copies the target into owned memory and consumes the workspace handle.
    /// The caller must have stopped/joined external writers. Even if a stale
    /// process later writes, Frozen's already captured bytes cannot change.
    pub fn freeze(self) -> Result<Frozen, Error> {
        let root = self.root();
        let limits = &self.task.pool.0.options;
        let mut files = BTreeMap::new();
        let mut remaining = limits.max_source_bytes;
        for path in self.task.targets() {
            let bytes = read_beneath(&root, path, remaining.min(limits.max_file_bytes))?;
            remaining -= bytes.len() as u64;
            files.insert(path.clone(), Arc::from(bytes));
        }
        let result = Frozen {
            task: self.task.clone(),
            agent: self.agent.clone(),
            files,
        };
        self.dir.cleanup()?;
        Ok(result)
    }

    /// Explicit cleanup reports errors that Drop cannot report.
    pub fn discard(self) -> Result<(), Error> {
        self.dir.cleanup().map_err(Into::into)
    }
}

/// Immutable in-memory submission, not a wrapper around a mutable pathname.
#[must_use]
#[derive(Clone)]
pub struct Frozen {
    task: Task,
    agent: String,
    files: BTreeMap<RelPath, Arc<[u8]>>,
}

impl Frozen {
    pub fn target(&self) -> &RelPath {
        &self.task.target
    }
    pub fn bytes(&self) -> &[u8] {
        &self.files[self.target()]
    }
    pub fn files(&self) -> &BTreeMap<RelPath, Arc<[u8]>> {
        &self.files
    }

    pub fn verify(self) -> Result<Verified, Error> {
        let inner = &self.task.pool.0;
        if inner.verifier.policy_id() != inner.policy {
            return Err(Error::PolicyMismatch);
        }
        let expected = inner.snapshot.with_candidate(&self.files);
        let dir = inner.scratch("verify")?;
        let root = dir.path.join("project");
        expected.materialize(&root, false)?;
        let input = Verification {
            root: &root,
            target: &self.task.target,
            policy: &inner.policy,
            pool: &self.task.pool,
            multi_file: self.files.len() > 1,
        };
        let report = inner.verifier.verify(&input).map_err(|e| match e {
            VerifyError::Rejected(reason) => Error::Rejected(reason),
            VerifyError::Operational(e) => Error::VerifierFailed(e),
        })?;
        if report.len() > MAX_REPORT {
            return Err(Error::LimitExceeded("verification report"));
        }
        if inner.verifier.policy_id() != inner.policy {
            return Err(Error::PolicyMismatch);
        }
        if Snapshot::capture(&root, &inner.options)? != expected {
            return Err(Error::VerificationInputsChanged);
        }
        dir.cleanup()?;
        Ok(Verified {
            frozen: self,
            report,
        })
    }
}

/// Evidence-bearing submission. Only Frozen::verify can construct it. No Clone,
/// public constructor, or deserialization implementation.
#[must_use]
pub struct Verified {
    frozen: Frozen,
    report: String,
}

impl Verified {
    pub fn report(&self) -> &str {
        &self.report
    }
    pub fn target(&self) -> &RelPath {
        self.frozen.target()
    }
}

#[derive(Clone, Debug)]
pub struct Published {
    dir: PathBuf,
    target: RelPath,
}

impl Published {
    /// Treat as immutable. To continue editing, create a new Pool from this
    /// root; never mutate a published winner in place.
    pub fn root(&self) -> PathBuf {
        self.dir.join("project")
    }
    pub fn target(&self) -> &RelPath {
        &self.target
    }
    pub fn file(&self) -> PathBuf {
        self.root().join(self.target.as_path())
    }
    pub fn agent(&self) -> Result<String, Error> {
        self.text("agent")
    }
    pub fn report(&self) -> Result<String, Error> {
        self.text("report")
    }
    fn text(&self, name: &str) -> Result<String, Error> {
        String::from_utf8(read_beneath(
            &self.dir,
            &RelPath::new(name)?,
            MAX_REPORT as u64,
        )?)
        .map_err(|_| Error::CorruptStore(self.dir.clone()))
    }
}

#[derive(Debug)]
pub enum PublishOutcome {
    Committed(Published),
    AlreadyWon(Published),
}

impl PublishOutcome {
    pub fn published(&self) -> &Published {
        match self {
            Self::Committed(p) | Self::AlreadyWon(p) => p,
        }
    }
}

// Implementation: private filesystem primitives and ownership guards.

fn validate_policy(policy: &str) -> Result<(), Error> {
    if policy.is_empty() || policy.len() > MAX_REPORT / 4 {
        Err(Error::InvalidConfiguration(
            "policy ID is empty or too long".into(),
        ))
    } else {
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        result.push(DIGITS[(b >> 4) as usize] as char);
        result.push(DIGITS[(b & 15) as usize] as char);
    }
    result
}

fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}
fn sync_dir(path: &Path) -> io::Result<()> {
    open_dir(path)?.sync_all()
}
fn open_dir(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(path)
}

fn lock_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(path)?;
    file.lock()?;
    Ok(file)
}
fn child_dir(dir: &Dir, name: &str) -> io::Result<Dir> {
    let file = dir.open_with(
        name,
        CapOpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW),
    )?;
    Ok(Dir::from_std_file(file.into_std()))
}

fn parent_beneath(root: &Path, rel: &RelPath) -> Result<(Dir, String), Error> {
    let mut dir = Dir::from_std_file(open_dir(root)?);
    let mut parts = rel.0.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return Ok((dir, part.to_owned()));
        }
        dir = child_dir(&dir, part)?;
    }
    Err(Error::InvalidPath(rel.0.clone()))
}

fn read_regular(dir: &Dir, name: &str, limit: u64) -> Result<Vec<u8>, Error> {
    // NONBLOCK prevents a malicious FIFO substitution from blocking open.
    let file = dir.open_with(
        name,
        CapOpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK),
    )?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::UnsupportedEntry(name.into()));
    }
    if metadata.len() > limit {
        return Err(Error::LimitExceeded("file bytes"));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::LimitExceeded("file bytes"));
    }
    Ok(bytes)
}

/// Read a bounded regular file through directory capabilities, rejecting symlinks.
pub fn read_beneath(root: &Path, rel: &RelPath, limit: u64) -> Result<Vec<u8>, Error> {
    let (parent, name) = parent_beneath(root, rel)?;
    read_regular(&parent, &name, limit)
}

/// Atomically replace a regular file through directory capabilities, rejecting symlinks.
/// The parent directory must already exist.
pub fn replace_beneath(root: &Path, rel: &RelPath, bytes: &[u8]) -> Result<(), Error> {
    let (parent, name) = parent_beneath(root, rel)?;
    let mode = match parent.symlink_metadata(&name) {
        Ok(m) if m.is_file() => 0o600 | (m.permissions().mode() & 0o111),
        Ok(_) => return Err(Error::UnsupportedEntry(rel.as_path().to_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0o600,
        Err(e) => return Err(e.into()),
    };
    let temporary = format!(".write-{}", next_id());
    let mut file = parent
        .open_with(
            &temporary,
            CapOpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode),
        )?
        .into_std();
    let result = (|| -> io::Result<()> {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        parent.rename(&temporary, &parent, &name)
    })();
    if result.is_err() {
        let _ = parent.remove_file(&temporary);
    }
    result.map_err(Into::into)
}

fn write_new(path: &Path, bytes: &[u8], mode: u32, durable: bool) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    if durable {
        file.sync_all()?;
    }
    Ok(())
}

fn capture_dir(
    dir: &Dir,
    prefix: &str,
    depth: usize,
    options: &Options,
    entries: &mut BTreeMap<RelPath, Entry>,
    total: &mut u64,
) -> Result<(), Error> {
    if depth > options.max_depth {
        return Err(Error::LimitExceeded("directory depth"));
    }
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::InvalidPath("non-UTF-8 filename".into()))?;
        if options.excluded_names.contains(&name) {
            continue;
        }
        if entries.len() >= options.max_entries {
            return Err(Error::LimitExceeded("entry count"));
        }
        let rel = RelPath::new(if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        })?;
        let path = PathBuf::from(rel.as_str());
        let metadata = dir.symlink_metadata(&name)?;
        if metadata.is_dir() {
            let child = child_dir(dir, &name)?;
            entries.insert(rel.clone(), Entry::Directory);
            capture_dir(&child, &rel.0, depth + 1, options, entries, total)?;
        } else if metadata.is_file() {
            let bytes = read_regular(dir, &name, options.max_file_bytes)?;
            *total = total
                .checked_add(bytes.len() as u64)
                .ok_or(Error::LimitExceeded("source bytes"))?;
            if *total > options.max_source_bytes {
                return Err(Error::LimitExceeded("source bytes"));
            }
            entries.insert(
                rel,
                Entry::File {
                    bytes: bytes.into(),
                    executable: metadata.permissions().mode() & 0o111,
                },
            );
        } else {
            return Err(Error::UnsupportedEntry(path));
        }
    }
    Ok(())
}

fn next_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{nanos:x}-{:x}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

struct OwnedDir {
    path: PathBuf,
    _lease: File,
    cleanup_on_drop: bool,
}

impl OwnedDir {
    fn create(parent: &Path, prefix: &str) -> io::Result<Self> {
        for _ in 0..100 {
            let path = parent.join(format!("{prefix}-{}", next_id()));
            match private_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
            let lease = (|| -> io::Result<File> {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path.join("lease"))?;
                file.lock()?;
                Ok(file)
            })();
            match lease {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        _lease: file,
                        cleanup_on_drop: true,
                    });
                }
                Err(e) => {
                    let _ = fs::remove_dir_all(&path);
                    return Err(e);
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique directory",
        ))
    }
    fn persist(&mut self) {
        self.cleanup_on_drop = false;
    }
    fn cleanup(mut self) -> io::Result<()> {
        fs::remove_dir_all(&self.path)?;
        self.cleanup_on_drop = false;
        Ok(())
    }
}

impl Drop for OwnedDir {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
thread_local! {
    static COMMIT_FAULT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn commit_fault(point: u8) -> io::Result<()> {
    COMMIT_FAULT.with(|fault| {
        if fault.get() == point {
            fault.set(0);
            Err(io::Error::other("injected commit failure"))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Accept;
    impl Verifier for Accept {
        fn policy_id(&self) -> &str {
            "tests/accept-v1"
        }
        fn verify(&self, _: &Verification<'_>) -> Result<String, VerifyError> {
            Ok("accepted".into())
        }
    }

    struct Fixture {
        _dir: OwnedDir,
        source: PathBuf,
        store: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = OwnedDir::create(&std::env::temp_dir(), "worktrees-test").unwrap();
            let source = dir.path.join("template");
            private_dir(&source).unwrap();
            fs::write(source.join("Solution.lean"), "original").unwrap();
            private_dir(&dir.path.join("runs")).unwrap();
            Self {
                store: dir.path.join("runs"),
                source,
                _dir: dir,
            }
        }
        fn pool(&self) -> Pool {
            Pool::create(
                &self.source,
                &self.store,
                Options::default(),
                Arc::new(Accept),
            )
            .unwrap()
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
    fn pruning_preserves_live_directories_and_removes_abandoned_ones() {
        let f = Fixture::new();
        let pool = f.pool();
        let a = task(&pool).attempt("live").unwrap();
        let mut abandoned = OwnedDir::create(&pool.store().join("scratch"), "abandoned").unwrap();
        let abandoned_path = abandoned.path.clone();
        abandoned.persist();
        drop(abandoned);
        assert_eq!(pool.prune_abandoned().unwrap(), 1);
        assert!(a.root().is_dir());
        assert!(!abandoned_path.exists());
    }

    #[test]
    fn error_before_rename_leaves_no_winner() {
        let f = Fixture::new();
        let pool = f.pool();
        let t = task(&pool);
        let v = verified(&t, b"proof");
        COMMIT_FAULT.with(|fault| fault.set(1));
        assert!(matches!(t.publish(v), Err(Error::Io(_))));
        assert!(t.winner().unwrap().is_none());
        assert_eq!(
            fs::read_dir(pool.store().join("scratch")).unwrap().count(),
            0
        );
    }

    #[test]
    fn error_after_rename_reports_visible_commit_and_retry_keeps_winner() {
        let f = Fixture::new();
        let pool = f.pool();
        let t = task(&pool);
        let v = verified(&t, b"first proof");
        COMMIT_FAULT.with(|fault| fault.set(2));
        match t.publish(v) {
            Err(Error::CommitUncertain { published, .. }) => {
                assert_eq!(fs::read(published.file()).unwrap(), b"first proof");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let second = t.publish(verified(&t, b"second proof")).unwrap();
        assert!(matches!(second, PublishOutcome::AlreadyWon(_)));
        assert_eq!(fs::read(second.published().file()).unwrap(), b"first proof");
    }
}
