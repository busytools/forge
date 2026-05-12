//! App-level error enum. Definition lives in
//! `forge_primitives::error` (Phase 0 of the MVVM refactor).
//! This module re-exports it so existing `crate::error::AppError`
//! imports keep resolving.

pub use forge_primitives::error::AppError;
