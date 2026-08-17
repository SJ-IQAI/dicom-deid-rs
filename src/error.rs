use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeidError {
    #[error("Recipe parse error: {0}")]
    RecipeParse(String),

    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),

    #[error("Tag resolution error: {0}")]
    TagResolution(String),

    #[error("Compressed pixel data cannot be masked without decompression: {0}")]
    CompressedPixelData(String),

    #[error("DICOM error: {0}")]
    Dicom(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Variable not found: {0}")]
    VariableNotFound(String),

    #[error("Function not found: {0}")]
    FunctionNotFound(String),

    /// An output path could not be built from the de-identified data set
    /// (r-1-6, r-1-7, r-1-8). Non-fatal: the file is counted as skipped.
    #[error("Output layout error: {0}")]
    Layout(String),

    /// Two input files rendered to the same output path (r-1-9).
    /// Non-fatal: the later file is counted as skipped so the earlier
    /// one is never silently overwritten.
    #[error("Output path collision: {0}")]
    PathCollision(String),

    /// The File Meta Information group cannot be brought into a
    /// de-identified, self-consistent state.
    ///
    /// Unlike the other variants this is *fatal*: it aborts the whole
    /// run rather than skipping the offending file. See
    /// [`DeidError::is_fatal`].
    #[error("File meta information cannot be de-identified: {0}")]
    FatalMeta(String),
}

impl DeidError {
    /// Whether this error must abort the entire run.
    ///
    /// Per-file errors are reported as warnings and counted as skipped
    /// (r-1-5). Fatal errors indicate a malformed recipe or corrupt
    /// input that would make the whole run's output suspect, so they
    /// propagate out of the pipeline instead.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::FatalMeta(_))
    }
}
