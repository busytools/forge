//! The biometric proxy's shapes: one device's enrolment, the purposes it
//! carries, and the blob a release hands back.
//!
//! **No wrapped blob rides the enrolment list.** A view draws which purposes a
//! device enrolled and when each was last released; the bytes stay behind the
//! release command, which answers the client that unwraps them. The list is
//! read by every client of the server, so a blob there would be every device's
//! ciphertext handed to every viewer for nothing.
//!
//! A purpose is the consumer's own name (`"vault"` in v1) and is opaque to the
//! core: the blob's shape is the consumer's business, and a second consumer is
//! a second purpose string rather than a change to any of these types.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One purpose's enrolment as a view draws it: the consumer's own name and
/// when it was enrolled and last released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiometricPurpose {
    /// The consumer that owns the blob (`"vault"` in v1).
    pub purpose: String,
    /// When this purpose was enrolled on the device.
    pub created_at: SystemTime,
    /// When the blob was last released, or `None` until it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<SystemTime>,
}

/// One device's enrolment: the id its client minted, the device's own name,
/// and the purposes it registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiometricEnrolment {
    /// The id the enrolling client minted for this device, which every later
    /// release and revoke names.
    pub id: Uuid,
    /// The device's own name for itself, as the settings page draws it.
    pub device: String,
    /// When the device first enrolled.
    pub created_at: SystemTime,
    pub purposes: Vec<BiometricPurpose>,
}

/// The blob a release answers with: exactly the bytes the client enrolled,
/// base64 on the wire and opaque to the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiometricBlob {
    /// The wrapped secret, base64.
    pub blob: String,
}

impl BiometricBlob {
    /// Wrap the stored bytes for the wire.
    pub fn encode(blob: &[u8]) -> Self {
        use base64::Engine as _;
        Self { blob: base64::engine::general_purpose::STANDARD.encode(blob) }
    }

    /// The stored bytes back. A string that is not base64 is refused rather
    /// than read as some other blob.
    pub fn decode(&self) -> Result<Vec<u8>, String> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(self.blob.as_bytes())
            .map_err(|error| format!("the wrapped blob is not base64: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn an_enrolment() -> BiometricEnrolment {
        BiometricEnrolment {
            id: Uuid::nil(),
            device: "Ved's MacBook Pro".to_owned(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000),
            purposes: vec![BiometricPurpose {
                purpose: "vault".to_owned(),
                created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000),
                last_used_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_100)),
            }],
        }
    }

    /// The list crosses the socket and comes back whole, which is what the
    /// home read and its update both rely on.
    #[test]
    fn an_enrolment_round_trips() {
        let enrolment = an_enrolment();
        let json = serde_json::to_string(&enrolment).expect("an enrolment serialises");
        let back: BiometricEnrolment = serde_json::from_str(&json).expect("an enrolment decodes");
        assert_eq!(enrolment, back);
    }

    /// The list a view draws carries the facts and no blob. A blob field here
    /// would fail this rather than reach every client that reads the home.
    #[test]
    fn the_view_carries_no_blob() {
        let value = serde_json::to_value(an_enrolment()).expect("an enrolment serialises");
        let keys = |value: &serde_json::Value| {
            let mut keys: Vec<String> =
                value.as_object().expect("an enrolment is an object").keys().cloned().collect();
            keys.sort_unstable();
            keys
        };
        assert_eq!(keys(&value), ["created_at", "device", "id", "purposes"]);
        assert_eq!(keys(&value["purposes"][0]), ["created_at", "last_used_at", "purpose"]);
        assert!(
            !value.to_string().contains("blob"),
            "the enrolment list names no blob at all: {value}",
        );
    }

    /// The wire form of a blob is the same bytes back, which is what makes the
    /// release able to hand the client something it can unwrap.
    #[test]
    fn a_blob_encodes_and_decodes_to_the_same_bytes() {
        let bytes = vec![0u8, 1, 2, 250, 255];
        let wire = BiometricBlob::encode(&bytes);
        assert_eq!(wire.decode().expect("a base64 blob decodes"), bytes);
    }

    /// A blob the client sent that is not base64 is refused at enrol rather
    /// than stored as some other bytes.
    #[test]
    fn a_blob_that_is_not_base64_is_refused() {
        let wire = BiometricBlob { blob: "not base64!".to_owned() };
        assert!(wire.decode().is_err());
    }
}
