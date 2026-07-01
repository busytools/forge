//! Cron MCP - durable scheduled prompts (`mcp__forge__cron__*`).
//!
//! A forge cron fires a prompt into a project's session on a schedule
//! and survives forge restarts (persisted to `forge-cron.toml`; see
//! [`crate::cron_store`]). Unlike the cloud routines (`create_trigger` /
//! `CronCreate`), which fire into cloud-hosted sessions, these durably
//! target the local forge process.
//!
//! The tools (`cron__create` / `cron__list` / `cron__delete`) are
//! ANY-CALLER, scoped to the caller's own project - mirroring
//! `workers__list`, not the lead-only `workers__spawn`.
//!
//! - [`schedule`] - pure due-check + boot catch-up math (+ the recurring
//!   next-fire computation once the parser dep lands).

pub(crate) mod schedule;
