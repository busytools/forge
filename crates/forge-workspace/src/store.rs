//! Machine-local embedded database (redb).
//!
//! A single redb file at `<app-support>/db.redb` holds forge's durable
//! per-machine state. One forge instance runs per machine, so one DB;
//! redb takes an exclusive file lock on open, so a second opener on the
//! same path fails here rather than corrupting.
//!
//! This wrapper is deliberately general - open plus the raw handle.
//! Table logic lives per-tenant in the submodules: Gotify subscriptions
//! ([`gotify`]), durable crons ([`cron`]), dynamic workers
//! ([`dynamic_workers`]), review threads ([`review`]), forge state
//! ([`state`], the spinner override + account-usage cache), the
//! `/usage` view's per-file token summaries ([`token_usage`]), cached
//! model pricing ([`pricing`]), cached OpenRouter model catalogs
//! ([`model_catalog`]), plugin update history ([`plugins`]), and the
//! catalog's per-file worker-tag scans ([`session_tags`]).

use std::path::Path;

use anyhow::Context;

pub mod cron;
pub mod dynamic_workers;
pub mod gotify;
pub mod model_catalog;
pub mod plugins;
pub mod pricing;
pub mod review;
pub mod session_tags;
pub mod state;
pub mod token_usage;

/// Handle to the machine-local redb database.
pub struct Db {
    inner: redb::Database,
}

impl Db {
    /// Open (or create) the database at `path`. Fails if redb cannot
    /// take its exclusive file lock (e.g. a second config dir on the
    /// same machine already holds it).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let inner = redb::Database::create(path)
            .with_context(|| format!("open redb database at {}", path.display()))?;
        Ok(Self { inner })
    }

    pub(crate) fn database(&self) -> &redb::Database {
        &self.inner
    }
}
