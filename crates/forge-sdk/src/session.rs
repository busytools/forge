//! `$CLAUDE_CONFIG_DIR`-aware path resolution for the on-disk artefacts
//! the `claude` binary persists.
//!
//! Filesystem-backed scanners + mutations over JSONL transcripts moved
//! to `forge_agent::userdata::catalog` in 2026-05-05; only path
//! resolution + OAuth credential reading stay here.

pub mod paths;
