//! Trusted submission scope and the on-disk form used by restart and reverification.
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use worktrees::{Frozen, RelPath, Task, read_beneath};

pub const MAX_FILE: u64 = 4 * 1024 * 1024;
pub type Sources = BTreeMap<RelPath, String>;

/// Without a manifest, preserve the existing single-file CLI.
pub fn scope(
    template: &Path,
    requested_primary: Option<&str>,
    challenge: &str,
) -> Result<(RelPath, BTreeSet<RelPath>)> {
    let manifest = template.join("proof-files.json");
    let data = match std::fs::read(manifest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                RelPath::new(requested_primary.unwrap_or("Proofs/Solution.lean"))?,
                BTreeSet::new(),
            ));
        }
        result => result?,
    };
    let names: Vec<String> = serde_json::from_slice(&data).context("invalid proof-files.json")?;
    let first = names
        .first()
        .context("proof-files.json must name at least one file")?;
    let primary = RelPath::new(requested_primary.unwrap_or(first))?;
    let mut files = BTreeSet::new();
    for name in names {
        let path = RelPath::new(&name)?;
        ensure!(
            name.ends_with(".lean")
                && path
                    .as_path()
                    .file_name()
                    .is_some_and(|n| n != "lakefile.lean")
                && name != challenge,
            "protected or invalid proof path: {path}"
        );
        // Pool::create rejects symlinks and oversized inputs when it captures the template.
        ensure!(
            template.join(path.as_path()).is_file(),
            "missing proof file {path}"
        );
        ensure!(files.insert(path), "duplicate proof path: {name}");
    }
    ensure!(
        files.remove(&primary),
        "proof-files.json must include the primary target {primary}"
    );
    Ok((primary, files))
}

pub fn load(task: &Task, input: &Path) -> Result<Sources> {
    let limits = task.limits();
    if input.is_dir() {
        let mut remaining = limits.max_source_bytes;
        return task
            .targets()
            .map(|p| {
                let bytes = read_beneath(input, p, limits.max_file_bytes.min(remaining))
                    .with_context(|| format!("load {p} from {}", input.display()))?;
                remaining -= bytes.len() as u64;
                Ok((p.clone(), String::from_utf8(bytes)?))
            })
            .collect();
    }
    ensure!(
        task.targets().count() == 1,
        "multi-file candidates require a directory containing all declared paths"
    );
    let mut source = String::new();
    std::fs::File::open(input)?
        .take(limits.max_file_bytes + 1)
        .read_to_string(&mut source)?;
    ensure!(
        source.len() as u64 <= limits.max_file_bytes,
        "candidate exceeds size limit"
    );
    Ok([(task.target().clone(), source)].into())
}

/// Single-file runs keep candidate.lean. Multi-file runs save relative paths below candidate/.
pub fn save(frozen: &Frozen, dir: &Path) -> Result<PathBuf> {
    if frozen.files().len() == 1 {
        let path = dir.join("candidate.lean");
        crate::trace::artifact(&path, frozen.bytes())?;
        return Ok(path);
    }
    let stage = tempfile::Builder::new()
        .prefix(".candidate-")
        .tempdir_in(dir)?;
    for (path, bytes) in frozen.files() {
        let dest = stage.path().join(path.as_path());
        std::fs::create_dir_all(dest.parent().unwrap())?;
        crate::trace::artifact(&dest, bytes)?;
    }
    let path = dir.join("candidate");
    std::fs::rename(stage.path(), &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_selects_existing_distinct_sources_and_protects_the_challenge() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in [
            "A.lean",
            "B.lean",
            "Ref.lean",
            "lakefile.lean",
            "config.json",
        ] {
            std::fs::write(root.join(name), "").unwrap();
        }
        let (legacy, others) = scope(root, None, "Ref.lean").unwrap();
        assert_eq!(legacy.as_str(), "Proofs/Solution.lean");
        assert!(others.is_empty());
        std::fs::write(root.join("proof-files.json"), r#"["B.lean","A.lean"]"#).unwrap();
        let (primary, others) = scope(root, None, "Ref.lean").unwrap();
        assert_eq!(primary.as_str(), "B.lean");
        assert_eq!(others, [RelPath::new("A.lean").unwrap()].into());
        assert!(scope(root, Some("Other.lean"), "Ref.lean").is_err());
        for other in [
            "A.lean",
            "Ref.lean",
            "lakefile.lean",
            "../B.lean",
            "Missing.lean",
            "config.json",
        ] {
            let names = [String::from("A.lean"), other.into()];
            std::fs::write(
                root.join("proof-files.json"),
                serde_json::to_vec(&names).unwrap(),
            )
            .unwrap();
            assert!(scope(root, None, "Ref.lean").is_err(), "{other}");
        }
    }
}
