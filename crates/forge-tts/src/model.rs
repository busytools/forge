//! The two GGUF files a speaking model is made of.

use std::path::{Path, PathBuf};

/// The model file and, where the pipeline keeps its codec in one, the
/// projector beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFiles {
    pub model: PathBuf,
    pub projector: Option<PathBuf>,
}

impl ModelFiles {
    /// Resolve `model` and `projector` file names against `dir`.
    pub fn from_dir(dir: &Path, model: &str, projector: Option<&str>) -> Self {
        Self { model: dir.join(model), projector: projector.map(|name| dir.join(name)) }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::ModelFiles;

    #[test]
    fn a_model_with_no_projector_is_still_a_model() {
        let files =
            ModelFiles::from_dir(Path::new("/tmp/tts"), "qwen3-tts-0.6b-base-q8.gguf", None);
        assert_eq!(files.model, std::path::PathBuf::from("/tmp/tts/qwen3-tts-0.6b-base-q8.gguf"));
        assert!(files.projector.is_none());
    }
}
