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
