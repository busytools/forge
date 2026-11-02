//! The two GGUF files a speaking model is made of.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::Error;

/// The model file and, where the pipeline keeps its codec in one, the
/// projector beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFiles {
    pub model: PathBuf,
    pub projector: Option<PathBuf>,
    /// The digest the model's entry declares, checked before a load.
    pub model_sha256: Option<String>,
    /// The digest the projector's entry declares, checked before a load.
    pub projector_sha256: Option<String>,
}

impl ModelFiles {
    /// Resolve `model` and `projector` file names against `dir`.
    pub fn from_dir(dir: &Path, model: &str, projector: Option<&str>) -> Self {
        Self {
            model: dir.join(model),
            projector: projector.map(|name| dir.join(name)),
            model_sha256: None,
            projector_sha256: None,
        }
    }

    /// Carry the digests the entries declare, so a load can verify them.
    pub fn with_digests(
        mut self,
        model: Option<String>,
        projector: Option<String>,
    ) -> Self {
        self.model_sha256 = model;
        self.projector_sha256 = projector;
        self
    }

    /// Check every file against its declared digest. A file with no declared
    /// digest is left to the loader's own existence check.
    pub fn verify(&self) -> Result<(), Error> {
        verify_digest(&self.model, self.model_sha256.as_deref())?;
        if let Some(projector) = &self.projector {
            verify_digest(projector, self.projector_sha256.as_deref())?;
        }
        Ok(())
    }
}

/// Hash `path` and compare it with `expected`, when one is declared.
pub fn verify_digest(path: &Path, expected: Option<&str>) -> Result<(), Error> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let mut file = std::fs::File::open(path)
        .map_err(|error| Error::ModelMissing(format!("{}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| Error::Load(format!("{}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::Load(format!(
            "{}: the entry declares sha256 {expected}, and the file is {actual}",
            path.display(),
        )))
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

    #[test]
    fn a_file_whose_digest_does_not_match_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("model.gguf");
        std::fs::write(&path, b"weights").expect("the file is written");
        let files = ModelFiles::from_dir(dir.path(), "model.gguf", None)
            .with_digests(Some("00".repeat(32)), None);
        let error = files.verify().expect_err("a wrong digest is refused");
        assert!(error.to_string().contains("model.gguf"), "got: {error}");
    }

    #[test]
    fn a_file_whose_digest_matches_is_accepted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("model.gguf");
        std::fs::write(&path, b"weights").expect("the file is written");
        let digest = {
            use sha2::{Digest as _, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(b"weights");
            hex::encode(hasher.finalize())
        };
        let files = ModelFiles::from_dir(dir.path(), "model.gguf", None)
            .with_digests(Some(digest), None);
        files.verify().expect("the declared digest matches");
    }
}
