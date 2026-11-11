//! `forge-tools` - one module per integration, inbound or outbound, plus the
//! cron scheduler's own family.
//!
//! Each module owns the network client, payload mapping and matching rules
//! for one integration, plus the tool family and held asks that integration
//! brings; the cron module owns the durable schedule set and the policy over
//! it. What a module needs from its host arrives through the
//! ports that module declares, implemented by forge-workspace - Gotify's
//! [`GotifyHost`](gotify::GotifyHost), the vault family's
//! [`vault::VaultHost`] and [`vault::VaultEmit`], the Slack family's
//! [`slack::SlackStore`], [`slack::SlackEmit`] and
//! [`slack::facade::SlackFacadeHost`], the cron family's
//! [`cron::CronStore`], [`cron::CronEmit`], [`cron::CronHost`] and
//! [`cron::facade::CronFacadeHost`] - so each module stays transport +
//! mapping and is testable offline. Four integrations exist today, and
//! there is still deliberately no generic connector trait: the ports differ
//! because what each integration needs differs.

pub mod cron;
pub mod gotify;
pub mod slack;
pub mod vault;
