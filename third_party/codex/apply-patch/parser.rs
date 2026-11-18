// Modified from OpenAI Codex (Apache-2.0). Extracted patch grammar types and markers. See ../NOTICE.
use std::path::PathBuf;
use thiserror::Error;

pub(crate) const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
pub(crate) const END_PATCH_MARKER: &str = "*** End Patch";
pub(crate) const ADD_FILE_MARKER: &str = "*** Add File: ";
pub(crate) const DELETE_FILE_MARKER: &str = "*** Delete File: ";
pub(crate) const UPDATE_FILE_MARKER: &str = "*** Update File: ";
pub(crate) const MOVE_TO_MARKER: &str = "*** Move to: ";
pub(crate) const EOF_MARKER: &str = "*** End of File";
pub(crate) const CHANGE_CONTEXT_MARKER: &str = "@@ ";
pub(crate) const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

#[derive(Debug, PartialEq, Error, Clone)]
pub enum ParseError {
    #[error("invalid patch: {0}")]
    InvalidPatchError(String),
    #[error("invalid hunk at line {line_number}, {message}")]
    InvalidHunkError { message: String, line_number: usize },
}
#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::enum_variant_names)]
pub enum Hunk {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,

        /// Chunks should be in order, i.e. the `change_context` of one chunk
        /// should occur later in the file than the previous chunk.
        chunks: Vec<UpdateFileChunk>,
    },
}

#[derive(Debug, Default, PartialEq, Clone)]
pub struct UpdateFileChunk {
    /// A single line of context used to narrow down the position of the chunk
    /// (this is usually a class, method, or function definition.)
    pub change_context: Option<String>,

    /// A contiguous block of lines that should be replaced with `new_lines`.
    /// `old_lines` must occur strictly after `change_context`.
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,

    /// Pairs of indices into `old_lines` and `new_lines` that identify lines
    /// parsed as context rather than inferred to be equal by their contents.
    pub context_line_indices: Vec<(usize, usize)>,

    /// If set to true, `old_lines` must occur at the end of the source file.
    /// (Tolerance around trailing newlines should be encouraged.)
    pub is_end_of_file: bool,
}

impl UpdateFileChunk {
    /// Adds a context line to both sides while recording its corresponding
    /// indices so it remains distinguishable from identical changed lines.
    pub(crate) fn push_context_line(&mut self, line: String) {
        self.context_line_indices
            .push((self.old_lines.len(), self.new_lines.len()));
        self.old_lines.push(line.clone());
        self.new_lines.push(line);
    }
}
