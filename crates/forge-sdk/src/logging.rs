//! Tracing target constants for forge-sdk.
//!
//! Same shape as `forge_agent::logging::targets` - keep target strings
//! consistent across crates so the subscriber's env-filter picks up
//! SDK-side logs under coherent prefixes.

pub mod targets {
    /// The reader-task that decodes stream-json frames coming back
    /// from the `claude` subprocess.
    pub const SDK_READER: &str = "sdk.reader";
    /// Bridge lifecycle events. Same target string the agent crate
    /// uses, so the subscriber's `bridge.lifecycle=debug` directive
    /// picks up SDK-side dispatch traces too.
    pub const BRIDGE_LIFECYCLE: &str = "bridge.lifecycle";
}
