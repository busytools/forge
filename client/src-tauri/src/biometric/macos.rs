//! The macOS gate: a Keychain item whose access-control policy the system
//! enforces.
//!
//! The blob is unreadable without Touch ID (or the device password when no
//! biometric is enrolled), because the item carries
//! `kSecAccessControlBiometryCurrentSet` rather than being a plain entry with
//! a prompt in front of it. The policy lives with the item: a read through any
//! other process meets the same wall, and a fingerprint set changed under the
//! item makes it unreadable, which is the OS's own invalidation.
//!
//! **The item is in the data protection keychain.** `kSecAttrAccessControl`
//! is supported there only, so the add and every query name
//! `kSecUseDataProtectionKeychain`.
#![allow(unsafe_code, reason = "Security and LocalAuthentication are C and Objective-C APIs")]

use std::ptr;

use base64::Engine as _;
use core_foundation::base::{CFOptionFlags, CFType, CFTypeRef, OSStatus, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::{CFDictionaryRef, CFMutableDictionary};
use core_foundation::string::{CFString, CFStringRef};
use objc2_local_authentication::{LAContext, LAPolicy};
use security_framework::access_control::{ProtectionMode, SecAccessControl};
use security_framework_sys::access_control::{
    kSecAccessControlBiometryCurrentSet, kSecAccessControlUserPresence,
};
use security_framework_sys::base::{errSecAuthFailed, errSecItemNotFound, errSecSuccess};
use security_framework_sys::item::{
    kSecAttrAccessControl, kSecAttrAccount, kSecAttrService, kSecClass, kSecClassGenericPassword,
    kSecReturnAttributes, kSecReturnData, kSecUseAuthenticationUI, kSecUseAuthenticationUISkip,
    kSecUseDataProtectionKeychain, kSecValueData,
};
use security_framework_sys::keychain_item::{SecItemAdd, SecItemCopyMatching, SecItemDelete};

use super::{Availability, Gate, Release};

/// The Keychain service every forge biometric item lives under. The account
/// is the purpose, so a second consumer is a new account and nothing else.
const SERVICE: &str = "dev.vedhavyas.forge.biometric";

/// The prompt the system shows above the gesture.
const PROMPT: &str = "unlock your vault";

/// The results this module matches: the framework's own names are lower-case
/// C spellings, and a pattern wants Rust's.
const SEC_SUCCESS: OSStatus = errSecSuccess;
const SEC_ITEM_NOT_FOUND: OSStatus = errSecItemNotFound;
const SEC_AUTH_FAILED: OSStatus = errSecAuthFailed;
/// The user cancelled the system prompt.
const ERR_SEC_USER_CANCELED: OSStatus = -128;
/// The prompt could not be shown: a locked screen, or a read that asked not
/// to authenticate.
const ERR_SEC_INTERACTION_NOT_ALLOWED: OSStatus = -25308;
/// The process is not signed for the data protection keychain.
const ERR_SEC_MISSING_ENTITLEMENT: OSStatus = -34018;

// `kSecUseOperationPrompt` is a stable Security constant that
// security-framework-sys 2.17 does not bind. Declared here rather than
// framing the system's default prompt, which names the item and not the act.
unsafe extern "C" {
    static kSecUseOperationPrompt: CFStringRef;
}

/// A framework constant as a wrapped string. Every caller passes one of
/// Security.framework's own statics, which are immortal and read-only; the
/// read is unsafe only because an extern static is not type-checked by the
/// compiler.
macro_rules! constant {
    ($name:ident) => {{
        // SAFETY: `$name` is one of Security.framework's own constants.
        unsafe { wrap_constant($name) }
    }};
}

/// The macOS gate: one Keychain item per purpose, under one service.
pub struct KeychainGate;

impl Gate for KeychainGate {
    async fn availability(&self) -> Availability {
        // **The probe is a synchronous keychain add and delete**, so it runs
        // on the blocking pool with the rest of the gate's work rather than
        // on an async worker.
        match tokio::task::spawn_blocking(Self::availability_now).await {
            Ok(availability) => availability,
            Err(_) => Availability {
                available: false,
                biometric: false,
                reason: Some("the keychain check did not run".to_owned()),
            },
        }
    }

    async fn enrolled(&self, purpose: &str) -> bool {
        let purpose = purpose.to_owned();
        // A Keychain query is a synchronous call into securityd; off the
        // async workers, so a slow one cannot stall the page's other work.
        tokio::task::spawn_blocking(move || Self::enrolled_now(&purpose)).await.unwrap_or(false)
    }

    async fn store(&self, purpose: &str, blob: &[u8]) -> Result<(), String> {
        let purpose = purpose.to_owned();
        let blob = blob.to_vec();
        match tokio::task::spawn_blocking(move || Self::store_now(&purpose, &blob)).await {
            Ok(result) => result,
            Err(error) => Err(format!("the keychain task did not run: {error}")),
        }
    }

    async fn release(&self, purpose: &str) -> Release {
        let purpose = purpose.to_owned();
        // The prompt blocks this thread until the person answers, which is
        // exactly why it runs on the blocking pool.
        match tokio::task::spawn_blocking(move || Self::read(&purpose, false)).await {
            Ok(release) => release,
            Err(error) => {
                Release::Refused { reason: format!("the keychain task did not run: {error}") }
            }
        }
    }

    async fn remove(&self, purpose: &str) -> Result<(), String> {
        let purpose = purpose.to_owned();
        match tokio::task::spawn_blocking(move || Self::remove_now(&purpose)).await {
            Ok(result) => result,
            Err(error) => Err(format!("the keychain task did not run: {error}")),
        }
    }
}

impl KeychainGate {
    fn availability_now() -> Availability {
        // **A data protection item with an access control needs a signed app
        // with an application identifier**, so an ad-hoc build or one run
        // from a checkout cannot enrol even though the sensor is there. The
        // probe asks the same question `store` asks, and a failure is drawn
        // as the reason rather than promising a gesture that cannot land.
        static PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let purpose = format!(
            "availability-{}-{}",
            std::process::id(),
            PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // **A crashed probe's item can collide after a restart**: the purpose
        // carries the pid and a counter, and both can repeat, so the probe
        // clears the purpose before it writes its own.
        let _ = Self::remove_now(&purpose);
        let refusal = match Self::add_now(&purpose, b"x") {
            Ok(SEC_SUCCESS) => None,
            Ok(other) => Some(message(other)),
            Err(error) => Some(error),
        };
        let _ = Self::remove_now(&purpose);
        if let Some(reason) = refusal {
            return Availability { available: false, biometric: false, reason: Some(reason) };
        }
        // Biometry usable now: enrolled, unlocked, not locked out. A Mac with
        // no Touch ID still answers, through the device password, which the
        // card draws as the non-biometric fallback.
        Availability { available: true, biometric: biometric_available(), reason: None }
    }

    fn enrolled_now(purpose: &str) -> bool {
        let query = base_query(purpose);
        let mut out: CFTypeRef = ptr::null();
        // SAFETY: `query` is a live dictionary of the framework's own key
        // types for the duration of the call, and `out` is a valid place for
        // the framework to write the item's attributes.
        let status = unsafe { SecItemCopyMatching(dictionary_ref(&query), &raw mut out) };
        if !out.is_null() {
            // SAFETY: a successful query returns a create-rule reference,
            // which wrapping and dropping releases exactly once.
            drop(unsafe { CFType::wrap_under_create_rule(out) });
        }
        status == errSecSuccess
    }

    fn store_now(purpose: &str, blob: &[u8]) -> Result<(), String> {
        // **Delete before add.** The policy is fixed when the item is created,
        // so replacing a blob is a new item; a duplicate would keep the old
        // bytes and the old policy.
        Self::remove_now(purpose)?;
        match Self::add_now(purpose, blob)? {
            SEC_SUCCESS => Ok(()),
            other => Err(message(other)),
        }
    }

    /// The item's add, answering the framework's status. Split out so the
    /// tests' reachability probe asks exactly the question the store asks:
    /// a probe that wrote a plainer item would pass where this one is refused.
    fn add_now(purpose: &str, blob: &[u8]) -> Result<OSStatus, String> {
        // `biometric_available`, not `availability_now`: the latter probes by
        // calling this add, and asking it here would recurse.
        let flags = access_flags(biometric_available());
        let control = SecAccessControl::create_with_protection(
            Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
            flags,
        )
        .map_err(|error| format!("the access-control policy could not be built: {error}"))?;

        let mut attributes = base_attributes(purpose);
        attributes.add(&constant!(kSecValueData), &CFData::from_buffer(blob).as_CFType());
        attributes.add(&constant!(kSecAttrAccessControl), &control.as_CFType());
        // SAFETY: `attributes` is a live dictionary for the duration of the
        // call and the result reference is not asked for.
        Ok(unsafe { SecItemAdd(dictionary_ref(&attributes), ptr::null_mut()) })
    }

    fn remove_now(purpose: &str) -> Result<(), String> {
        let query = base_query(purpose);
        // SAFETY: `query` is a live dictionary of the framework's own key
        // types for the duration of the call.
        let status = unsafe { SecItemDelete(dictionary_ref(&query)) };
        match status {
            SEC_SUCCESS | SEC_ITEM_NOT_FOUND => Ok(()),
            other => Err(message(other)),
        }
    }

    /// The release query, with `skip` making the read refuse rather than
    /// prompt. The skipping form is the test's: it proves the item's policy
    /// wants a gesture on a machine where the test cannot answer one.
    fn read(purpose: &str, skip: bool) -> Release {
        // **Data only**: asking for the attributes as well would make the
        // framework answer a dictionary, which the success arm below wraps as
        // CFData. The query is the item's own attributes plus the data.
        let mut query = base_attributes(purpose);
        query.add(&constant!(kSecReturnData), &CFBoolean::true_value().as_CFType());
        if skip {
            query.add(
                &constant!(kSecUseAuthenticationUI),
                &constant!(kSecUseAuthenticationUISkip).as_CFType(),
            );
        } else {
            query.add(&constant!(kSecUseOperationPrompt), &CFString::new(PROMPT).as_CFType());
        }
        let mut out: CFTypeRef = ptr::null();
        // SAFETY: `query` is a live dictionary for the duration of the call,
        // and `out` is a valid place for the framework to write the item's
        // data.
        let status = unsafe { SecItemCopyMatching(dictionary_ref(&query), &raw mut out) };
        match status {
            SEC_SUCCESS => {
                // SAFETY: a successful read returns a create-rule CFData,
                // which wrapping and dropping releases exactly once.
                let data = unsafe { CFData::wrap_under_create_rule(out.cast()) };
                Release::Released {
                    blob: base64::engine::general_purpose::STANDARD.encode(data.bytes()),
                }
            }
            SEC_ITEM_NOT_FOUND => Release::NotEnrolled,
            other => Release::Refused { reason: message(other) },
        }
    }
}

/// The access-control flags a store uses: the biometric-only set where the
/// sensor is there, user presence (the device password) where it is not,
/// rather than an item nothing on that Mac can read.
fn access_flags(biometric: bool) -> CFOptionFlags {
    if biometric { kSecAccessControlBiometryCurrentSet } else { kSecAccessControlUserPresence }
}

fn account(purpose: &str) -> CFString {
    CFString::new(&format!("purpose-{purpose}"))
}

/// The framework owns the constant for the process's life, so the get rule's
/// retain never frees one.
fn wrap_constant(reference: CFStringRef) -> CFString {
    // SAFETY: the caller's macro passes a `kSec...` constant, which is
    // immortal.
    unsafe { CFString::wrap_under_get_rule(reference) }
}

/// The attributes every item of this service carries.
fn base_attributes(purpose: &str) -> CFMutableDictionary<CFString, CFType> {
    let mut attributes = CFMutableDictionary::new();
    attributes.add(&constant!(kSecClass), &constant!(kSecClassGenericPassword).as_CFType());
    attributes.add(&constant!(kSecAttrService), &CFString::new(SERVICE).as_CFType());
    attributes.add(&constant!(kSecAttrAccount), &account(purpose).as_CFType());
    attributes.add(&constant!(kSecUseDataProtectionKeychain), &CFBoolean::true_value().as_CFType());
    attributes
}

/// The matching query for one purpose: the attributes, never the data.
fn base_query(purpose: &str) -> CFMutableDictionary<CFString, CFType> {
    let mut query = base_attributes(purpose);
    query.add(&constant!(kSecReturnAttributes), &CFBoolean::true_value().as_CFType());
    query
}

fn dictionary_ref(dictionary: &CFMutableDictionary<CFString, CFType>) -> CFDictionaryRef {
    // A `CFMutableDictionary` is a `CFDictionary`; the cast only drops the
    // mutability the C calls do not take.
    dictionary.as_concrete_TypeRef().cast()
}

/// Whether a biometric gesture can be evaluated right now. The unsafe call is
/// wrapped once, so the gate's own body stays a question.
fn biometric_available() -> bool {
    // SAFETY: a fresh context evaluates a policy constant; the error output is
    // not used.
    unsafe {
        let context = LAContext::new();
        context.canEvaluatePolicy_error(LAPolicy::DeviceOwnerAuthenticationWithBiometrics)
    }
    .is_ok()
}

/// The platform's own words for a failed call.
fn message(status: OSStatus) -> String {
    match status {
        ERR_SEC_USER_CANCELED => "the gesture was cancelled".to_owned(),
        ERR_SEC_INTERACTION_NOT_ALLOWED => {
            "the authenticator could not ask for the gesture".to_owned()
        }
        ERR_SEC_MISSING_ENTITLEMENT => {
            "the app is not signed for the keychain it stores the blob in".to_owned()
        }
        SEC_AUTH_FAILED => "the authenticator refused the gesture".to_owned(),
        other => format!("the keychain answered {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live item on this machine, under a purpose no real consumer uses.
    ///
    /// **What this test cannot do is read the blob**: that needs a person to
    /// answer the system's gesture, and an automated run has none. What it
    /// pins instead is the property that makes the gesture necessary - a
    /// data read that asks not to authenticate finds nothing, so the item's
    /// policy is in force - and that the item is otherwise a normal entry the
    /// gate can find, replace and drop.
    ///
    /// **A runner outside the graphical login skips with the platform's
    /// reason** rather than failing: a process launched from the system
    /// launchd domain cannot open the login keychain at all, so the failure
    /// would be the session's and not the gate's. Any other status is the
    /// gate's and fails below.
    #[tokio::test]
    async fn the_item_is_stored_under_a_policy_that_wants_a_gesture() {
        if let Some(reason) = keychain_unreachable() {
            eprintln!("skipped: {reason}");
            return;
        }
        let gate = KeychainGate;
        let purpose = format!("test-{}", std::process::id());
        let blob = b"wrapped secret for the gate test";

        gate.store(&purpose, blob).await.expect("the item is stored");

        assert!(gate.enrolled(&purpose).await, "the item is found by its attributes");
        assert_eq!(
            KeychainGate::read(&purpose, true),
            Release::NotEnrolled,
            "a read that refuses to authenticate cannot see the blob",
        );

        // A second store replaces the bytes rather than duplicating the item.
        gate.store(&purpose, b"second").await.expect("the item is replaced");
        assert!(gate.enrolled(&purpose).await, "the replacement is still one item");

        gate.remove(&purpose).await.expect("the item is removed");
        assert!(!gate.enrolled(&purpose).await, "nothing is left behind");
    }

    /// A second consumer is a second purpose on the same gate: no new item
    /// class, no code path, and dropping one leaves the other's item alone.
    /// This is the brief's acceptance criterion on the client half.
    #[tokio::test]
    async fn a_second_consumer_is_only_a_purpose() {
        if let Some(reason) = keychain_unreachable() {
            eprintln!("skipped: {reason}");
            return;
        }
        let gate = KeychainGate;
        let first = format!("test-first-{}", std::process::id());
        let second = format!("test-second-{}", std::process::id());

        gate.store(&first, b"first consumer's shape").await.expect("the first is stored");
        gate.store(&second, b"second").await.expect("the second is stored");
        assert!(
            gate.enrolled(&first).await && gate.enrolled(&second).await,
            "both purposes are enrolled"
        );

        gate.remove(&first).await.expect("the first is removed");
        assert!(!gate.enrolled(&first).await, "the first is gone");
        assert!(gate.enrolled(&second).await, "the second consumer's item is untouched");

        gate.remove(&second).await.expect("the second is removed");
    }

    /// Whether this process's session can reach a keychain at all, and why not
    /// when it cannot.
    ///
    /// **The probe asks the gate's own question**: an access-controlled data
    /// protection item, the shape `store` creates, under a purpose unique to
    /// the process so parallel test processes cannot race on one account. A
    /// plain login-keychain item answers a narrower question and succeeds on
    /// a runner whose binary is unsigned, which is how CI once failed here
    /// after the probe said the keychain was reachable.
    fn keychain_unreachable() -> Option<String> {
        let purpose = format!("self-check-{}", std::process::id());
        let status = KeychainGate::add_now(&purpose, b"x").ok();
        // Delete whatever the probe wrote, whatever it answered.
        let _ = KeychainGate::remove_now(&purpose);
        harness_refusal(status?)
    }

    /// Why the probe's status means this harness cannot exercise the live
    /// item, or `None` when it can.
    ///
    /// **-34018 is the harness's refusal, not the gate's**: an
    /// access-controlled item needs a signed app with an application
    /// identifier, so CI's ad-hoc binary and an uninstalled debug build are
    /// refused while a plain login-keychain item still succeeds. The live
    /// path waits on the Developer ID-signed app, row afcdebf4.
    fn harness_refusal(status: OSStatus) -> Option<String> {
        const ERR_SEC_WR_PERM: OSStatus = -61;
        const ERR_SEC_NOT_AVAILABLE: OSStatus = -25291;
        match status {
            ERR_SEC_WR_PERM | ERR_SEC_NOT_AVAILABLE => Some(
                "this process's session cannot open the login keychain (run the shell's tests \
                 from a graphical login, or the installed app)"
                    .to_owned(),
            ),
            ERR_SEC_MISSING_ENTITLEMENT => Some(
                "this build is not signed for the data protection keychain, so the live \
                 keychain item cannot be created here (the Developer ID-signed app is row \
                 afcdebf4)"
                    .to_owned(),
            ),
            _ => None,
        }
    }

    /// The read's success arm, on an item that needs no gesture: a plain data
    /// protection item carries no access control, so the gate's own query
    /// returns its bytes and the CFData wrap is exercised wherever the
    /// machine can make one.
    #[test]
    fn a_plain_item_reads_back_as_its_bytes() {
        if let Some(reason) = keychain_unreachable() {
            eprintln!("skipped: {reason}");
            return;
        }
        let purpose = format!("plain-{}", std::process::id());
        let mut attributes = base_attributes(&purpose);
        attributes.add(&constant!(kSecValueData), &CFData::from_buffer(b"the bytes").as_CFType());
        // SAFETY: `attributes` is a live dictionary for the duration of the
        // call and no result reference is asked for.
        let status = unsafe { SecItemAdd(dictionary_ref(&attributes), ptr::null_mut()) };
        assert_eq!(status, SEC_SUCCESS, "the plain item is added: {}", message(status));
        assert_eq!(
            KeychainGate::read(&purpose, true),
            Release::Released { blob: "dGhlIGJ5dGVz".to_owned() },
            "the gate reads the item's bytes out of the CFData",
        );
        let _ = KeychainGate::remove_now(&purpose);
    }

    /// The policy choice is pinned: a sensor present gets the biometric-only
    /// set, and a Mac without one falls back to user presence rather than an
    /// item nothing can read.
    #[test]
    fn the_access_control_follows_the_sensor() {
        assert_eq!(access_flags(true), kSecAccessControlBiometryCurrentSet);
        assert_ne!(access_flags(true), kSecAccessControlUserPresence);
        assert_eq!(access_flags(false), kSecAccessControlUserPresence);
    }

    /// The classifier the probe relies on: the named refusals skip with their
    /// reasons, and an unknown status is the gate's, not the harness's, so it
    /// goes down to the real call rather than passing as a skip.
    #[test]
    fn the_harness_refusal_names_only_the_conditions_it_knows() {
        assert!(harness_refusal(SEC_SUCCESS).is_none());
        assert!(
            harness_refusal(ERR_SEC_MISSING_ENTITLEMENT)
                .is_some_and(|reason| reason.contains("afcdebf4")),
            "an unsigned build is the harness's own refusal"
        );
        assert!(harness_refusal(-61).is_some(), "errSecWrPerm");
        assert!(harness_refusal(-25291).is_some(), "errSecNotAvailable");
        assert!(harness_refusal(-12345).is_none(), "an unknown status is the gate's");
    }

    /// Availability answers what the platform can do now: an LAContext is
    /// constructible and its policy evaluates one way or the other.
    #[tokio::test]
    async fn availability_answers_without_prompting() {
        let availability = KeychainGate.availability().await;
        // **Availability asks the same question the store asks**, so a
        // harness that cannot create the item says so here too rather than
        // promising a gesture that cannot land.
        if keychain_unreachable().is_some() {
            assert!(!availability.available, "an unreachable gate is unavailable");
            assert!(availability.reason.is_some(), "and names the platform's reason");
        } else {
            assert!(availability.available, "the gate is usable on this machine");
            assert!(availability.reason.is_none());
        }
    }
}
