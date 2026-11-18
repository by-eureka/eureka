// Modified from OpenAI Codex (Apache-2.0). Adapts chunk matching to in-memory updates on declared proof files. See ../NOTICE.
//! Adapted from OpenAI Codex apply-patch. See ../NOTICE.
mod parser;
mod seek_sequence;
mod streaming_parser;
mod text_file;
use parser::{Hunk, UpdateFileChunk};
use streaming_parser::StreamingPatchParser;
use text_file::{Replacement, SourceFile};

pub struct Update {
    pub path: String,
    chunks: Vec<UpdateFileChunk>,
}

impl Update {
    pub fn apply(&self, original: &str) -> anyhow::Result<String> {
        let mut source = SourceFile::parse(original);
        let replacements = compute_replacements(&source.line_texts(), &self.path, &self.chunks)?;
        source.apply_replacements(&replacements);
        Ok(source.into_contents())
    }
}

/// Eureka edits existing allowlisted files in one workspace.
pub fn parse(patch: &str) -> anyhow::Result<Vec<Update>> {
    anyhow::ensure!(
        patch.trim().starts_with("*** Begin Patch\n")
            || patch.trim().starts_with("*** Begin Patch\r\n"),
        "expected *** Begin Patch"
    );
    let mut parser = StreamingPatchParser::default();
    parser.push_delta(patch.trim())?;
    let hunks = parser.finish()?;
    anyhow::ensure!(
        parser.environment_id().is_none(),
        "environment IDs are unsupported"
    );
    anyhow::ensure!(!hunks.is_empty(), "empty patch");
    hunks
        .into_iter()
        .map(|h| match h {
            Hunk::UpdateFile {
                path,
                chunks,
                move_path: None,
            } => Ok(Update {
                path: path
                    .into_os_string()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid path"))?,
                chunks,
            }),
            _ => anyhow::bail!(
                "use Update File on declared proof files; add, delete and move are unsupported"
            ),
        })
        .collect()
}

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`. Each replacement is returned as
/// `(start_index, old_len, new_lines)`.
fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> anyhow::Result<Vec<Replacement>> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        // If a chunk has a `change_context`, we use seek_sequence to find it, then
        // adjust our `line_index` to continue from there.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence::seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                /*eof*/ false,
            ) {
                line_index = idx + 1;
            } else {
                anyhow::bail!("Failed to find context '{ctx_line}' in {path}");
            }
        }

        if chunk.old_lines.is_empty() {
            let insertion_idx = original_lines.len();
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        // Otherwise, try to match the existing lines in the file with the old lines
        // from the chunk. If found, schedule that region for replacement.
        // Attempt to locate the `old_lines` verbatim within the file.  In many
        // real‑world diffs the last element of `old_lines` is an *empty* string
        // representing the terminating newline of the region being replaced.
        // This sentinel is not present in `original_lines` because `SourceFile`
        // stores the terminator on the preceding line rather than as an extra
        // trailing element. If a direct search fails and the pattern ends with
        // an empty string, retry without that final element so modifications
        // touching the end‑of‑file can be located reliably.

        let mut pattern: &[String] = &chunk.old_lines;
        let mut found =
            seek_sequence::seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);

        let mut new_slice: &[String] = &chunk.new_lines;

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            // Retry without the trailing empty line which represents the final
            // newline in the file.
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }

            found = seek_sequence::seek_sequence(
                original_lines,
                pattern,
                line_index,
                chunk.is_end_of_file,
            );
        }

        if let Some(start_idx) = found {
            // Context lines occur in both sides of a patch chunk. Keep those
            // original lines in place so their exact contents and terminators
            // survive, especially when the file has mixed line endings.
            let mut old_start = 0;
            let mut new_start = 0;
            for &(old_context, new_context) in &chunk.context_line_indices {
                // A trailing empty context line can be removed from `pattern`
                // and `new_slice` above when it represents the final newline.
                if old_context >= pattern.len() || new_context >= new_slice.len() {
                    break;
                }
                if old_start != old_context || new_start != new_context {
                    replacements.push((
                        start_idx + old_start,
                        old_context - old_start,
                        new_slice[new_start..new_context].to_vec(),
                    ));
                }
                old_start = old_context + 1;
                new_start = new_context + 1;
            }
            if old_start != pattern.len() || new_start != new_slice.len() {
                replacements.push((
                    start_idx + old_start,
                    pattern.len() - old_start,
                    new_slice[new_start..].to_vec(),
                ));
            }
            line_index = start_idx + pattern.len();
        } else {
            anyhow::bail!(
                "Failed to find expected lines in {}:\n{}",
                path,
                chunk.old_lines.join("\n"),
            );
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);

    Ok(replacements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_multiple_files_and_preserves_context_line_endings() {
        let changes = parse("*** Begin Patch\n*** Update File: A.lean\n@@\n context α\n-old\n+new\n*** Update File: B.lean\n@@\n-first\n+second\n*** End Patch").unwrap();
        assert_eq!(
            changes[0].apply("context α\r\nold\r\n").unwrap(),
            "context α\r\nnew\r\n"
        );
        assert_eq!(changes[1].apply("first\n").unwrap(), "second\n");
        assert!(changes[1].apply("other\n").is_err());
    }

    #[test]
    fn rejects_workspace_changes_and_undeclared_file_operations() {
        for header in [
            "*** Add File: A.lean\n+x",
            "*** Delete File: A.lean",
            "*** Update File: A.lean\n*** Move to: B.lean\n@@\n-a\n+b",
            "*** Environment ID: other\n*** Update File: A.lean\n@@\n-a\n+b",
        ] {
            assert!(parse(&format!("*** Begin Patch\n{header}\n*** End Patch")).is_err());
        }
    }
}
