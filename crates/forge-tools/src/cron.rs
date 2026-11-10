//! The cron integration: the durable schedule set, the policy over it, the
//! MCP facade and its three tools, and the schedule math.
//!
//! A forge cron fires a prompt into the session that owns it, on a
//! schedule, and survives forge restarts. The workspace keeps the spine -
//! `forge.toml`'s project catalogue, the session pool, the Command bus, the
//! redb store and the supervisor that ticks this subsystem - and this crate
//! owns the entries, the due-fire pass and the tools.
//!
//! - [`schedule`] - pure due-check, next-fire and boot catch-up math.
//! - [`subsystem`] - the state, the mutation policy and the fire pass.
//! - [`facade`] - the tools' caller-scoped view over the subsystem.
//! - [`CronTools`] - the `cron__*` tool provider.

pub mod facade;
pub mod schedule;
pub mod subsystem;
mod tools;

pub use subsystem::{
    CRON_TICK_INTERVAL, Cron, CronCooling, CronEmit, CronFireOutcome, CronHost, CronSeams,
    CronStore, IST_OFFSET_MINUTES,
};
pub use tools::CronTools;
