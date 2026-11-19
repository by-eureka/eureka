//! Lean development workflows adapted from cameronfreer/lean4-skills.
//! The controller serves these guides to both local and remote attempts.
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Workflow {
    Autoprove,
    Prove,
    Disprove,
    Draft,
    Formalize,
    Autoformalize,
    Checkpoint,
    Review,
    Refactor,
    Golf,
    Learn,
    Diagnose,
}

impl Workflow {
    pub fn name(self) -> String {
        self.to_possible_value().unwrap().get_name().to_owned()
    }

    pub fn guide(self) -> &'static str {
        match self {
            Self::Autoprove => include_str!("../workflows/autoprove.md"),
            Self::Prove => include_str!("../workflows/prove.md"),
            Self::Disprove => include_str!("../workflows/disprove.md"),
            Self::Draft => include_str!("../workflows/draft.md"),
            Self::Formalize => include_str!("../workflows/formalize.md"),
            Self::Autoformalize => include_str!("../workflows/autoformalize.md"),
            Self::Checkpoint => include_str!("../workflows/checkpoint.md"),
            Self::Review => include_str!("../workflows/review.md"),
            Self::Refactor => include_str!("../workflows/refactor.md"),
            Self::Golf => include_str!("../workflows/golf.md"),
            Self::Learn => include_str!("../workflows/learn.md"),
            Self::Diagnose => include_str!("../workflows/diagnose.md"),
        }
    }

    pub fn inspects_source(self) -> bool {
        matches!(self, Self::Review | Self::Learn | Self::Diagnose)
    }

    pub fn drafts_source(self) -> bool {
        matches!(self, Self::Draft | Self::Formalize | Self::Autoformalize)
    }

    pub fn accepts_proof(self) -> bool {
        !self.inspects_source() && !self.drafts_source()
    }

    pub fn accepts_report(self) -> bool {
        !matches!(self, Self::Autoprove | Self::Refactor | Self::Golf)
    }
}

/// Read only the operator-selected skill at startup. Model-provided names are
/// map keys, never controller filesystem paths.
pub struct Guides {
    pub skill: String,
    entries: BTreeMap<String, String>,
}

impl Guides {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("SKILL.md");
        let skill =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut entries: BTreeMap<_, _> = Workflow::value_variants()
            .iter()
            .map(|w| (w.name(), w.guide().to_owned()))
            .collect();
        for (name, guide) in [
            ("proof-repair", include_str!("../workflows/proof-repair.md")),
            (
                "axiom-eliminator",
                include_str!("../workflows/axiom-eliminator.md"),
            ),
            (
                "sorry-filler-deep",
                include_str!("../workflows/sorry-filler-deep.md"),
            ),
            ("proof-golfer", Workflow::Golf.guide()),
        ] {
            entries.insert(name.into(), guide.into());
        }
        match std::fs::read_dir(dir.join("references")) {
            Ok(files) => {
                for file in files {
                    let file = file?;
                    let path = file.path();
                    if file.file_type()?.is_file()
                        && path.extension().is_some_and(|ext| ext == "md")
                    {
                        let name = file
                            .file_name()
                            .into_string()
                            .map_err(|_| anyhow::anyhow!("invalid reference filename"))?;
                        entries.insert(name, std::fs::read_to_string(path)?);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(Self { skill, entries })
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn read(&self, name: &str) -> Result<&str> {
        let Some(guide) = self.entries.get(name) else {
            bail!("unknown guide: {name}");
        };
        Ok(guide)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serves_only_the_selected_skills_references_and_registered_guides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "custom skill").unwrap();
        let refs = dir.path().join("references");
        std::fs::create_dir(&refs).unwrap();
        std::fs::write(refs.join("custom.md"), "custom reference").unwrap();
        std::fs::write(refs.join("unrelated"), "ignore").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", refs.join("outside.md")).unwrap();
        let guides = Guides::load(dir.path()).unwrap();
        assert_eq!(guides.skill, "custom skill");
        assert_eq!(guides.read("custom.md").unwrap(), "custom reference");
        for workflow in Workflow::value_variants() {
            assert_eq!(guides.read(&workflow.name()).unwrap(), workflow.guide());
        }
        for name in ["../SKILL.md", "outside.md", "unrelated", "/etc/passwd"] {
            assert!(guides.read(name).is_err());
        }
    }
}
