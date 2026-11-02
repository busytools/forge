//! Machine-local embedded database (redb).
//!
//! One redb file per config dir at
//! `<app-support>/stores/<stable-key>.redb` holds that config dir's durable
//! per-machine state. The key is a SHA-256 of the canonicalised config-dir
//! path, so a Rust release cannot move it; the single-instance lock keeps its
//! own digest and is unaffected. redb takes an exclusive file lock on open,
//! so a second opener on the same path fails here rather than corrupting.
//!
//! This wrapper is deliberately general - open plus the raw handle.
//! Table logic lives per-tenant in the submodules: Gotify subscriptions
//! ([`gotify`]), Slack subscriptions ([`slack`]), durable crons
//! ([`cron`]) and the schedules their fired prompts came from
//! ([`cron_names`]), session identities ([`sessions`]), review
//! threads ([`review`]), forge state
//! ([`state`], the account-usage cache), the
//! usage scan's per-file token summaries ([`token_usage`]), cached
//! model pricing ([`pricing`]), plugin update history ([`plugins`]),
//! and the catalog's per-file worker-tag scans ([`session_tags`]).
//!
//! [`dynamic_workers`] is the retired worker table: nothing writes it,
//! and it holds only until the boot sweep has moved every row onto
//! [`sessions`].
//!
//! [`model_catalog`] is retired rather than live: it exists only to drop
//! the table the deleted catalog machinery left behind in older stores.
//!
//! [`settings`] is the same, for the table the `/spinner` override lived in.

use std::path::Path;

use anyhow::Context;

pub mod bench_results;
pub mod cron;
pub mod cron_names;
pub mod dictate_models;
pub mod dynamic_workers;
pub mod gotify;
pub mod handoffs;
pub mod model_catalog;
pub mod plugins;
pub mod pricing;
pub mod review;
pub mod session_tags;
pub mod sessions;
pub mod settings;
pub mod slack;
pub mod speaking_results;
pub mod state;
pub mod task_archive;
pub mod task_chases;
pub mod task_history;
pub mod tasks;
pub mod token_usage;

/// This config dir's store file: `<app_support>/stores/<stable-key>.redb`.
///
/// The key is stable across Rust releases; the single-instance lock keeps its
/// own [`forge_sdk::config_dir_hash`], so a new binary never misses a running
/// old process's lock.
pub fn path_for(app_support: &Path, config_dir: &Path) -> std::path::PathBuf {
    app_support.join("stores").join(format!("{}.redb", forge_sdk::config_dir_store_key(config_dir)))
}

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

    /// Compact the file in place, reclaiming what a dropped table left
    /// behind. Returns whether it did anything. redb takes `&mut self`,
    /// so the caller needs the handle owned - the boot compacts before it
    /// is wrapped for the rest of the process.
    pub fn compact(&mut self) -> anyhow::Result<bool> {
        self.inner.compact().context("compact redb database")
    }
}
