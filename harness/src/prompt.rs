//! Instructions shared by leads and helpers. The running MCP server contributes
//! its tool contract and the selected Beam checkout supplies its matching skill.
use crate::workflow::{Guides, Workflow};
use anyhow::Result;
use std::path::Path;

pub fn instructions(
    template: &Path,
    files: &str,
    challenge: &str,
    w: Workflow,
    guides: &Guides,
) -> Result<String> {
    let config = std::fs::read_to_string(template.join("config.json"))?;
    let task = if w.inspects_source() {
        "Inspect the given source and return the requested findings with finish_report. Preserve the source."
    } else if w.drafts_source() {
        "Develop the proposed declarations and proofs in the assigned files. Use finish_report to hand back the candidate and your interpretation of the informal source. Identify intended holes and unresolved claims."
    } else {
        "Keep the supplied definitions and theorem statements fixed. Fill the requested proofs in their assigned modules. Leads submit complete candidates to the independent kernel checker. Extra axioms, sorry and changed claims fail verification. Helpers use finish to hand their part back to the lead."
    };
    Ok(format!(
        r#"You work in a private Lean project with the {name} workflow.
{task}
Declared files: {files}. Trusted challenge: {challenge}. Verification configuration:
{config}

# Selected workflow: {name}
{guide}

# Leanbeam in this harness
Read the challenge and assigned sources with read_file (0-based lines). Use read_guide for other workflows or any Beam reference named in the skill (use its filename, e.g. commit-speculative.md). For a blocked goal consult sorry-filler-deep, for elaboration errors proof-repair, for unwanted assumptions axiom-eliminator, and for simplifying a working seed proof-golfer. A lead or a helper can use the same guide. Reading another guide does not change the assigned task or its available completion tools.

MCP owns the workspace. The CLI operations in the Beam skill map to lean_* tools, for example run-at-handle to lean_run_at_handle. Use the tool schemas for parameters. After a real edit call lean_update and pass the returned version to position-bound tools. Positions count UTF-16 code units. The harness unwraps MCP structured content. A tool failure starts with ERROR. For a completed probe read success and remaining goals. Use lean_check with text or an array of independent commands to query the real imports of a declared path. It probes before the last declaration; use a position-bound probe when scope matters. Use lean_todo on a point for code actions and lean_code_action_resolve when necessary.

When editing, use apply_patch or write_file on the declared sources only. Update and sync a changed dependency, checkpoint it with lean_save, then refresh and sync each open importer. If shell is available, use it for source lookup or an explicitly needed batch build. Drop affected Beam workspaces and confirm cleanup before a batch build. The controller verifies in a fresh project. Additional retrieval tools and helpers are optional; consult the tools actually offered. A helper works on its assigned portion and always returns with finish. The lead collects useful helper work before completing the run.

# Beam skill
{skill}"#,
        name = w.name(),
        guide = w.guide(),
        skill = guides.skill
    ))
}
