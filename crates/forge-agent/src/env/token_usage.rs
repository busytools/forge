//! Token/cost accounting for the `/usage` view.
//!
//! [`pricing`] is the LiteLLM-sourced per-model USD table used to turn
//! JSONL token counts into a notional cost.

pub mod pricing;
