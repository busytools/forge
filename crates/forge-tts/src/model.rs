//! The files a speaking model is made of: one ONNX model and a directory of
//! `<voice>.bin` styles.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::Error;

/// A model's files, and the digests the install declared for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFiles {
    /// The ONNX model file.
    pub model: PathBuf,
    /// The directory of `<voice>.bin` style files.
    pub voices: PathBuf,
    /// Digests to check before a load, keyed by path.
    pub digests: Vec<(PathBuf, String)>,
}

impl ModelFiles {
    /// The model file and the voice directory it renders with.
    pub fn new(model: PathBuf, voices: PathBuf) -> Self {
        Self { model, voices, digests: Vec::new() }
    }

    /// Carry the digests the install declared, so a load can verify them.
    pub fn with_digests(mut self, digests: Vec<(PathBuf, String)>) -> Self {
        self.digests = digests;
        self
    }

    /// Check every file against its declared digest. A file with no declared
    /// digest is left to the loader's own existence check.
    pub fn verify(&self) -> Result<(), Error> {
        if !self.model.is_file() {
            return Err(Error::ModelMissing(self.model.display().to_string()));
        }
        if !self.voices.is_dir() {
            return Err(Error::ModelMissing(self.voices.display().to_string()));
        }
        for (path, digest) in &self.digests {
            verify_digest(path, Some(digest))?;
        }
        Ok(())
    }

    /// Whether the files a load needs are on disk, by presence only: the
    /// model file, the voice directory, and every declared file's path.
    /// Cheap enough for a boot trigger, where [`Self::verify`] (which hashes
    /// each declared digest) is not. A file evicted after the record was
    /// written reads as absent.
    pub fn present(&self) -> bool {
        self.model.is_file()
            && self.voices.is_dir()
            && self.digests.iter().all(|(path, _)| path.is_file())
    }

    /// The installed voice ids: every `.bin` file stem in `voices`, sorted.
    pub fn installed_voices(&self) -> Result<Vec<String>, Error> {
        let entries = std::fs::read_dir(&self.voices)
            .map_err(|error| Error::ModelMissing(format!("{}: {error}", self.voices.display())))?;
        let mut voices = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| Error::Load(format!("{}: {error}", self.voices.display())))?;
            let path = entry.path();
            let is_style = path.extension().is_some_and(|extension| extension == "bin");
            if !is_style {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            voices.push(stem.to_owned());
        }
        voices.sort();
        Ok(voices)
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
    use super::ModelFiles;

    fn model(dir: &std::path::Path) -> ModelFiles {
        ModelFiles::new(dir.join("model.onnx"), dir.join("voices"))
    }

    #[test]
    fn the_installed_voices_are_the_style_file_stems_sorted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let voices = dir.path().join("voices");
        std::fs::create_dir(&voices).expect("the voice dir is made");
        for name in ["bf_emma.bin", "af_heart.bin", "am_adam.bin"] {
            std::fs::write(voices.join(name), b"styles").expect("a style is written");
        }
        std::fs::write(voices.join("readme.txt"), b"not a voice").expect("a stray file");
        let files = model(dir.path());
        assert_eq!(
            files.installed_voices().expect("the directory reads"),
            vec!["af_heart".to_owned(), "am_adam".to_owned(), "bf_emma".to_owned()],
        );
    }

    #[test]
    fn a_file_whose_digest_does_not_match_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir(dir.path().join("voices")).expect("the voice dir is made");
        let path = dir.path().join("model.onnx");
        std::fs::write(&path, b"weights").expect("the file is written");
        let files = model(dir.path()).with_digests(vec![(path.clone(), "00".repeat(32))]);
        let error = files.verify().expect_err("a wrong digest is refused");
        assert!(error.to_string().contains("model.onnx"), "got: {error}");
    }

    #[test]
    fn a_file_whose_digest_matches_is_accepted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir(dir.path().join("voices")).expect("the voice dir is made");
        let path = dir.path().join("model.onnx");
        std::fs::write(&path, b"weights").expect("the file is written");
        let digest = {
            use sha2::{Digest as _, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(b"weights");
            hex::encode(hasher.finalize())
        };
        let files = model(dir.path()).with_digests(vec![(path, digest)]);
        files.verify().expect("the declared digest matches");
    }

    #[test]
    fn present_requires_the_model_the_voice_directory_and_each_declared_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let voices = dir.path().join("voices");
        std::fs::create_dir(&voices).expect("the voice dir is made");
        let model = dir.path().join("model.onnx");
        std::fs::write(&model, b"weights").expect("the model is written");
        let voice = voices.join("af_heart.bin");
        std::fs::write(&voice, b"styles").expect("a style is written");
        // The declared digests are wrong on purpose: presence does not hash.
        let files =
            ModelFiles::new(model, voices.clone()).with_digests(vec![(voice, "00".repeat(32))]);
        assert!(files.present(), "every file the load needs is on disk");

        std::fs::remove_dir_all(&voices).expect("the voice dir is removed");
        assert!(!files.present(), "an evicted voice directory is not present");
    }

    #[test]
    fn present_sees_a_declared_voice_file_that_is_gone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let voices = dir.path().join("voices");
        std::fs::create_dir(&voices).expect("the voice dir is made");
        let model = dir.path().join("model.onnx");
        std::fs::write(&model, b"weights").expect("the model is written");
        let files = ModelFiles::new(model, voices)
            .with_digests(vec![(dir.path().join("voices/af_heart.bin"), "00".repeat(32))]);
        assert!(!files.present(), "a declared voice file that is gone reads as absent");
    }

    #[test]
    fn a_missing_model_file_is_refused_before_a_load() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir(dir.path().join("voices")).expect("the voice dir is made");
        let error = model(dir.path()).verify().expect_err("a missing model is refused");
        assert!(error.to_string().contains("model.onnx"), "got: {error}");
    }
}
