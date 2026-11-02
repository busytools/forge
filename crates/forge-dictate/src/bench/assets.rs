//! The public bench slice that ships in the binary: LibriSpeech test-clean
//! utterances, their transcripts, and the attribution the licence asks for.
//!
//! Embedded once here and installed beside the models on first use, so an
//! installed binary carries the corpus and neither bench depends on the repo
//! tree. The clip bytes are the asset; the manifest beside them is the digest
//! list the reader verifies by name.

use std::path::Path;

use crate::Error;

/// The manifest the installed slice is read through: one clip per entry,
/// with its digest and the words it was read from.
pub(crate) const MANIFEST: &str = include_str!("../../assets/public-corpus/manifest.json");

/// The licence and provenance the asset ships with, written beside the
/// clips so a reader of the files has them where the audio is.
const ATTRIBUTION: &str = include_str!("../../assets/public-corpus/ATTRIBUTION.md");

/// Every clip: its file name and its bytes.
const CLIPS: &[(&str, &[u8])] = &[
    ("clip-0000.wav", include_bytes!("../../assets/public-corpus/clip-0000.wav")),
    ("clip-0001.wav", include_bytes!("../../assets/public-corpus/clip-0001.wav")),
    ("clip-0002.wav", include_bytes!("../../assets/public-corpus/clip-0002.wav")),
    ("clip-0003.wav", include_bytes!("../../assets/public-corpus/clip-0003.wav")),
    ("clip-0004.wav", include_bytes!("../../assets/public-corpus/clip-0004.wav")),
    ("clip-0005.wav", include_bytes!("../../assets/public-corpus/clip-0005.wav")),
    ("clip-0006.wav", include_bytes!("../../assets/public-corpus/clip-0006.wav")),
    ("clip-0007.wav", include_bytes!("../../assets/public-corpus/clip-0007.wav")),
    ("clip-0008.wav", include_bytes!("../../assets/public-corpus/clip-0008.wav")),
    ("clip-0009.wav", include_bytes!("../../assets/public-corpus/clip-0009.wav")),
    ("clip-0010.wav", include_bytes!("../../assets/public-corpus/clip-0010.wav")),
    ("clip-0011.wav", include_bytes!("../../assets/public-corpus/clip-0011.wav")),
    ("clip-0012.wav", include_bytes!("../../assets/public-corpus/clip-0012.wav")),
    ("clip-0013.wav", include_bytes!("../../assets/public-corpus/clip-0013.wav")),
    ("clip-0014.wav", include_bytes!("../../assets/public-corpus/clip-0014.wav")),
    ("clip-0015.wav", include_bytes!("../../assets/public-corpus/clip-0015.wav")),
    ("clip-0016.wav", include_bytes!("../../assets/public-corpus/clip-0016.wav")),
    ("clip-0017.wav", include_bytes!("../../assets/public-corpus/clip-0017.wav")),
    ("clip-0018.wav", include_bytes!("../../assets/public-corpus/clip-0018.wav")),
    ("clip-0019.wav", include_bytes!("../../assets/public-corpus/clip-0019.wav")),
    ("clip-0020.wav", include_bytes!("../../assets/public-corpus/clip-0020.wav")),
    ("clip-0021.wav", include_bytes!("../../assets/public-corpus/clip-0021.wav")),
    ("clip-0022.wav", include_bytes!("../../assets/public-corpus/clip-0022.wav")),
    ("clip-0023.wav", include_bytes!("../../assets/public-corpus/clip-0023.wav")),
    ("clip-0024.wav", include_bytes!("../../assets/public-corpus/clip-0024.wav")),
    ("clip-0025.wav", include_bytes!("../../assets/public-corpus/clip-0025.wav")),
    ("clip-0026.wav", include_bytes!("../../assets/public-corpus/clip-0026.wav")),
    ("clip-0027.wav", include_bytes!("../../assets/public-corpus/clip-0027.wav")),
    ("clip-0028.wav", include_bytes!("../../assets/public-corpus/clip-0028.wav")),
    ("clip-0029.wav", include_bytes!("../../assets/public-corpus/clip-0029.wav")),
    ("clip-0030.wav", include_bytes!("../../assets/public-corpus/clip-0030.wav")),
    ("clip-0031.wav", include_bytes!("../../assets/public-corpus/clip-0031.wav")),
];

/// Install the bundled slice into `dir` when the slice there is not this
/// binary's, and leave it alone when it is.
///
/// The manifest lands last: a half-written slice then reads as an empty tier
/// or a refusal, never as certified audio.
pub(crate) fn install(dir: &Path) -> Result<(), Error> {
    let manifest_path = dir.join("manifest.json");
    let installed = std::fs::read(&manifest_path).ok();
    if installed.as_deref() == Some(MANIFEST.as_bytes()) {
        let whole = CLIPS
            .iter()
            .all(|(name, bytes)| std::fs::read(dir.join(name)).is_ok_and(|have| have == *bytes));
        if whole {
            return Ok(());
        }
    }

    std::fs::create_dir_all(dir).map_err(|source| Error::Io { path: dir.to_path_buf(), source })?;
    for (name, bytes) in CLIPS {
        let path = dir.join(name);
        std::fs::write(&path, bytes).map_err(|source| Error::Io { path, source })?;
    }
    let attribution = dir.join("ATTRIBUTION.md");
    std::fs::write(&attribution, ATTRIBUTION)
        .map_err(|source| Error::Io { path: attribution, source })?;
    std::fs::write(&manifest_path, MANIFEST)
        .map_err(|source| Error::Io { path: manifest_path, source })?;
    Ok(())
}
