#![allow(
    missing_docs,
    reason = "log target constants — names are self-describing"
)]

pub mod targets {
    pub const APP_AUTH: &str = "app.auth";
    pub const APP_CACHE: &str = "app.cache";
    pub const APP_CONFIG: &str = "app.config";
    pub const APP_COMMAND: &str = "app.command";
    pub const APP_INPUT: &str = "app.input";
    pub const APP_LIFECYCLE: &str = "app.lifecycle";
    pub const APP_NETWORK: &str = "app.network";
    pub const APP_PASTE: &str = "app.paste";
    pub const APP_PERF: &str = "app.perf";
    pub const APP_PERMISSION: &str = "app.permission";
    pub const APP_RENDER: &str = "app.render";
    pub const APP_SESSION: &str = "app.session";
    pub const APP_TOOL: &str = "app.tool";
    pub const APP_UPDATE: &str = "app.update";
    pub const BRIDGE_LIFECYCLE: &str = "bridge.lifecycle";
    pub const BRIDGE_MCP: &str = "bridge.mcp";
    pub const BRIDGE_PERMISSION: &str = "bridge.permission";
    pub const BRIDGE_PROTOCOL: &str = "bridge.protocol";
    pub const BRIDGE_SDK: &str = "bridge.sdk";
}
