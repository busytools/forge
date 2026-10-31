//! The socket's binary frame tags, shared by every Rust half that speaks it.

/// The binary kind a compressed JSON frame crosses as.
///
/// Kinds 0, 1 and 2 are a dictation frame, a browser frame and its audio, all
/// `forge-server`'s own. This one is a whole `ServerMessage` body, gzipped,
/// for a client that declared `deflate=1` on its socket URL: the server's
/// connection builds it and the shell's socket decodes it, so the tag lives
/// here rather than as a copy in each.
pub const COMPRESSED_JSON_TAG: u8 = 3;

#[cfg(test)]
mod tests {
    use super::COMPRESSED_JSON_TAG;

    /// `client/src/socket.ts` keeps its own copy of this tag, because a page
    /// is not a Rust half. Renumbering the tag here without that copy is a
    /// frame no browser can read.
    #[test]
    fn the_compressed_tag_is_the_one_the_web_page_hardcodes() {
        assert_eq!(COMPRESSED_JSON_TAG, 3);
    }
}
