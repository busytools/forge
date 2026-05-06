//! Identity newtypes used across forge crates.
//!
//! Wraps `String` rather than UUID-typed because the upstream `claude`
//! CLI emits IDs as opaque strings; preserving that shape avoids
//! accidental parse errors at the codec boundary.

use serde::{Deserialize, Serialize};

/// Session identifier emitted by the `claude` CLI's `system/init` frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Per-tool-call identifier carried in `can_use_tool` and tool messages.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct ToolUseId(pub String);

/// Per-message identifier (used by rewind / fork operations).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct MessageId(pub String);

macro_rules! id_impls {
    ($name:ident) => {
        impl $name {
            #[must_use]
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

id_impls!(SessionId);
id_impls!(ToolUseId);
id_impls!(MessageId);

// Convenience comparisons so call sites can write `id == "literal"`
// without unwrapping the newtype.
macro_rules! id_str_eq {
    ($name:ident) => {
        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }
        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }
        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }
    };
}

id_str_eq!(SessionId);
id_str_eq!(ToolUseId);
id_str_eq!(MessageId);
