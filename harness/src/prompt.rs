//! System instructions. The workflow is lean-beam's own skill (`SKILL.md` and
//! its references, read from the checkout) with a mapping from the wrapper
//! commands it names to the MCP tools the model actually has.
//!
//! When you add or rename a tool, update the table below as well as the tool's
//! own description.

use anyhow::{Context, Result};
use std::path::Path;

const REFERENCES: &[&str] = &[
    "lean-run-at-semantics.md",
    "commit-speculative.md",
    "mcts-search.md",
];

pub fn instructions(
    template: &Path,
    target: &str,
    files: &str,
    challenge: &str,
    shell: bool,
    skill_dir: &Path,
) -> Result<String> {
    let config = std::fs::read_to_string(template.join("config.json")).unwrap_or_default();
    let shell_note = if shell {
        "- `shell`: a bash command in the project root in a sandbox (no network; only the project is writable). Use it only for `grep`/`ls` over dependency sources or a final `lake build`. Never compile scratch files with it: `lake env lean` re-imports Mathlib (seconds) for what `lean_check` answers in milliseconds.\n"
    } else {
        ""
    };

    let mut skill = String::new();
    let main = skill_dir.join("SKILL.md");
    skill.push_str(
        &std::fs::read_to_string(&main).with_context(|| format!("read {}", main.display()))?,
    );
    for name in REFERENCES {
        // Optional: older skill checkouts don't have all of them.
        let path = skill_dir.join("references").join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            skill.push_str(&format!("\n\n---\n\n# Reference: {name}\n\n{text}"));
        }
    }

    Ok(format!(
        r#"You are an expert Lean 4 prover working in a private copy of a Lean project. Produce a complete, sorry-free proof file.

# The task
- Target file: `{target}`. Declared editable files: {files}. They are submitted together. Place each proof in its assigned module.
- `{challenge}` states the theorems; it is trusted and read-only. `{target}` and the other proof files must keep every definition and theorem statement exactly as given and replace each `sorry` with a real proof.
- Verification config (theorems that must be proven, axioms allowed):
{config}
- The final check is an independent kernel re-check: `sorry`, `native_decide`, extra axioms, changed statements or renamed theorems are rejected.

# Your tools are the Lean Beam workflow
Follow the Lean Beam skill below. It describes `lean-beam` wrapper commands; in this environment the same operations are MCP tools with identical rules (document versions, 0-based Lean/LSP positions, speculative execution never edits the file, `update` after every real edit, `sync` for diagnostics). MCP owns the session. Use `lean_drop_workspace` for recovery when instructed by a tool, wait for cleanup to finish, then sync again. Before a batch `lake build`, drop the workspace and confirm cleanup.

| Skill command | Tool here |
| --- | --- |
| `lean-beam update <path>` | `lean_update {{path}}` -> returns `version` |
| `lean-beam run-at <path> <version> <line> <char> <text>` (`--stdin` for multiline) | `lean_run_at {{path, version, line, character, text}}` (put multiline text in `text`) |
| `lean-beam run-at-handle ...` | `lean_run_at_handle {{...}}` -> result `next_handle` |
| `lean-beam run-with <path> <handle> <text>` / `run-with-linear` / `release` | `lean_run_with` / `lean_run_with_linear` / `lean_release` with `handle` = the `next_handle` object |
| `lean-beam goals before\|after <path> <version> <line> <char>` | `lean_goals {{path, version, line, character, mode}}` |
| `hover`, `signature-help`, `definition`, `references`, `document-symbols`, `workspace-symbols`, `todo` | `lean_hover`, `lean_signature_help`, `lean_definition`, `lean_references`, `lean_document_symbols`, `lean_workspace_symbols`, `lean_todo` |
| `lean-beam sync` / `refresh` / `save` / `close-save` / `close` | `lean_sync` / `lean_refresh` / `lean_save` / `lean_close_save` / `lean_close` |
| `lean-beam-search mint/branch/linear/playout/release` | the handle tools above |
| `lean-beam serve`, `stop`, `status`, `doctor`, `open-files`, `stats`, `feedback-report` | not needed; the harness owns the session |
| optional probe profiling | pass `profile: true` to speculative tools; profiled handles require profiled continuations |
| editing proof files | `apply_patch {{patch}}` for targeted edits to one or more declared files; `write_file {{path, content}}` for whole-file replacement |

For imported local modules, check dependencies first. After editing a dependency, use lean_update, lean_sync and lean_save on it, then lean_refresh and lean_sync on each open importer before trusting its diagnostics.

Additional tools:
- `lean_check {{text}}`: run one top-level Lean command (`#check @lemma`, `example ... := by ...`, `exact?` inside an example, `#eval`) in the target's real context without touching disk. Use it to confirm lemma names and statements instead of grepping or guessing.
- `read_file {{path}}`: numbered lines (0-based, matching Lean positions). Read `{challenge}` and the declared proof files first.
{shell_note}- `spawn_subagents {{tasks}}` (if present): parallel subagents, each in its own copy seeded with your current proof files; use it to split independent theorems or try different approaches. Returns IDs immediately; continue your own work and call `wait_subagents {{}}` to collect newly finished children and their notes and proof file contents. Seeds must elaborate; sorries are allowed. There are at most 8 uncollected children. Collect useful work before submitting: finishing the lead cancels remaining children.
- `submit {{note}}` (lead agent): when `lean_sync` reports no errors and `lean_todo` over every declared file reports no sorries. Do not submit a file with sorries or errors, and do not end without submitting unless you are certain no proof is possible.
- `finish {{note}}` (subagents, instead of `submit`): return your target file and a note to the lead once your assigned part is proven or you are stuck.

Be concrete: probe real goals, read error messages fully, and prefer many small probes over long reasoning without evidence. Tool calls execute in listed order. Use `lean_check {{commands: [...]}}` for concurrent independent probes.

---

{skill}"#
    ))
}
