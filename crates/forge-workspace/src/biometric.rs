//! The biometric proxy's server half: each device's enrolment, each purpose's
//! opaque blob, and revocation.
//!
//! **What the core stores it cannot read.** A blob is wrapped by a client's
//! platform authenticator; this module keeps the bytes a client sent and hands
//! the same bytes back on a release, and nothing here decodes one. The core
//! still holds the vault's unlocked keys in memory while the vault is open,
//! exactly as it did before this feature - the blob store does not make it
//! hands-off, and the design says so plainly.
//!
//! **A purpose is a registration, not a code path.** A blob is keyed by the
//! purpose string it arrived with and is otherwise opaque, so a second
//! consumer is a new string plus its own blob shape in a client, and nothing
//! in this module changes. `a_second_consumer_needs_only_a_purpose` is the
//! guard on that.

use std::time::SystemTime;

use uuid::Uuid;

use forge_primitives::biometric::{BiometricBlob, BiometricEnrolment, BiometricPurpose};

use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;

/// The largest wrapped blob a client may enrol. The material is a refresh
/// token and a user key, a few hundred bytes; the cap exists because the write
/// is a client command and nothing else bounds it.
const MAX_BLOB_BYTES: usize = 64 * 1024;

/// One device's enrolment as the store holds it: the facts a view draws plus
/// each purpose's wrapped blob.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Enrolment {
    pub id: Uuid,
    pub device: String,
    pub created_at: SystemTime,
    pub last_used_at: Option<SystemTime>,
    pub purposes: Vec<StoredPurpose>,
}

/// One purpose's stored half: the wrapped blob and its stamps. The bytes are
/// opaque; nothing in the core decodes them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredPurpose {
    pub purpose: String,
    pub blob: Vec<u8>,
    pub created_at: SystemTime,
    pub last_used_at: Option<SystemTime>,
}

impl Enrolment {
    /// The facts a view draws, with every blob left behind.
    fn view(&self) -> BiometricEnrolment {
        BiometricEnrolment {
            id: self.id,
            device: self.device.clone(),
            created_at: self.created_at,
            purposes: self
                .purposes
                .iter()
                .map(|held| BiometricPurpose {
                    purpose: held.purpose.clone(),
                    created_at: held.created_at,
                    last_used_at: held.last_used_at,
                })
                .collect(),
        }
    }
}

impl Workspace {
    /// Every device's enrolment, read from the machine-local store.
    ///
    /// A store that cannot be read is an error rather than an empty list:
    /// "no device is enrolled" and "the enrolments could not be read" send a
    /// reader looking in different places.
    pub fn biometric_enrolments(&self) -> Result<Vec<BiometricEnrolment>, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else {
            return Err("the machine-local store is not open".to_owned());
        };
        crate::store::biometric::list(db)
            .map(|rows| rows.iter().map(Enrolment::view).collect())
            .map_err(|error| error.to_string())
    }

    /// Register one purpose's wrapped blob under `id`, creating the device's
    /// enrolment when the id is new and replacing that purpose's bytes when it
    /// is not - which is what a refresh-token rotation's rewrite is.
    pub(crate) fn enrol_biometric(
        &self,
        id: Uuid,
        device: &str,
        purpose: &str,
        blob: &str,
    ) -> Result<(), String> {
        let device = device.trim();
        let purpose = purpose.trim();
        if device.is_empty() {
            return Err("an enrolment needs the device's own name".to_owned());
        }
        if purpose.is_empty() {
            return Err("an enrolment names no purpose".to_owned());
        }
        let blob = BiometricBlob { blob: blob.to_owned() }.decode()?;
        if blob.is_empty() {
            return Err("the wrapped blob is empty".to_owned());
        }
        if blob.len() > MAX_BLOB_BYTES {
            return Err(format!(
                "the wrapped blob is {} bytes, past the {MAX_BLOB_BYTES} byte cap",
                blob.len(),
            ));
        }
        let now = SystemTime::now();
        {
            let db = self.db.lock();
            let Some(db) = db.as_ref() else {
                return Err("the machine-local store is not open".to_owned());
            };
            let mut enrolment = crate::store::biometric::get(db, id)
                .map_err(|error| error.to_string())?
                .unwrap_or(Enrolment {
                    id,
                    device: device.to_owned(),
                    created_at: now,
                    last_used_at: None,
                    purposes: Vec::new(),
                });
            device.clone_into(&mut enrolment.device);
            match enrolment.purposes.iter_mut().find(|held| held.purpose == purpose) {
                // A re-enrol is a rotation rewrite: the bytes move and the
                // purpose keeps the stamp it was first enrolled at.
                Some(held) => held.blob = blob,
                None => enrolment.purposes.push(StoredPurpose {
                    purpose: purpose.to_owned(),
                    blob,
                    created_at: now,
                    last_used_at: None,
                }),
            }
            crate::store::biometric::put(db, &enrolment).map_err(|error| error.to_string())?;
        }
        self.announce_biometric_enrolments();
        Ok(())
    }

    /// The blob enrolled for `purpose` under `id`, stamped as used.
    ///
    /// `None` is "this device has nothing enrolled for that purpose", which
    /// is the answer a client falls back to the password on. A store failure
    /// is an error, because a client that cannot tell the two apart would
    /// silently skip the password path.
    pub(crate) fn release_biometric(
        &self,
        id: Uuid,
        purpose: &str,
    ) -> Result<Option<BiometricBlob>, String> {
        let now = SystemTime::now();
        let answer = {
            let db = self.db.lock();
            let Some(db) = db.as_ref() else {
                return Err("the machine-local store is not open".to_owned());
            };
            let Some(mut enrolment) =
                crate::store::biometric::get(db, id).map_err(|error| error.to_string())?
            else {
                return Ok(None);
            };
            let Some(index) = enrolment.purposes.iter().position(|held| held.purpose == purpose)
            else {
                return Ok(None);
            };
            enrolment.purposes[index].last_used_at = Some(now);
            enrolment.last_used_at = Some(now);
            let answer = BiometricBlob::encode(&enrolment.purposes[index].blob);
            crate::store::biometric::put(db, &enrolment).map_err(|error| error.to_string())?;
            answer
        };
        self.announce_biometric_enrolments();
        Ok(Some(answer))
    }

    /// Drop one purpose's blob, or `None` for the device's whole enrolment.
    /// Revoking the last purpose drops the enrolment with it: a device holding
    /// nothing is a row with no meaning.
    pub(crate) fn revoke_biometric(&self, id: Uuid, purpose: Option<&str>) -> Result<(), String> {
        {
            let db = self.db.lock();
            let Some(db) = db.as_ref() else {
                return Err("the machine-local store is not open".to_owned());
            };
            match purpose {
                Some(purpose) => {
                    let Some(mut enrolment) =
                        crate::store::biometric::get(db, id).map_err(|error| error.to_string())?
                    else {
                        return Ok(());
                    };
                    enrolment.purposes.retain(|held| held.purpose != purpose);
                    if enrolment.purposes.is_empty() {
                        crate::store::biometric::remove(db, id)
                            .map_err(|error| error.to_string())?;
                    } else {
                        crate::store::biometric::put(db, &enrolment)
                            .map_err(|error| error.to_string())?;
                    }
                }
                None => {
                    crate::store::biometric::remove(db, id).map_err(|error| error.to_string())?;
                }
            }
        }
        self.announce_biometric_enrolments();
        Ok(())
    }

    /// Lock the shared vault now, on the reader's own press rather than the
    /// idle clock. The process holds one vault session, so any seat's lock is
    /// every seat's; parked cards are re-stamped so a dock that already drew
    /// one asks for the password again.
    ///
    /// `false` when no vault is configured, which is the only state a view
    /// would not have drawn the action in.
    pub(crate) fn lock_vault(&self) -> bool {
        let session = self.vault.read().clone();
        let Some(session) = session else {
            tracing::debug!(
                event_name = "vault_lock_without_session",
                "a lock arrived with no [vault] configured",
            );
            return false;
        };
        session.lock();
        self.rebase_vault_asks(true);
        tracing::info!(event_name = "vault_locked", "the vault was locked at the reader's press");
        true
    }

    /// Tell every view the enrolment set moved, as the whole set the core
    /// holds. A write that cannot be read back announces nothing and says so;
    /// the next read still answers the truth.
    fn announce_biometric_enrolments(&self) {
        match self.biometric_enrolments() {
            Ok(enrolments) => {
                let _ =
                    self.update_tx.send(SessionUpdate::BiometricEnrolmentsChanged { enrolments });
            }
            Err(error) => tracing::warn!(
                event_name = "biometric_enrolments_unreadable",
                %error,
                "an enrolment write landed but the set could not be read back for the announcement",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::SessionSlot;
    use crate::store::Db;

    /// A workspace with a real machine-local store under a temp dir, the way a
    /// boot opens one.
    fn workspace() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _updates) =
            Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let db = Db::open(&dir.path().join("store.redb")).expect("open store");
        workspace.install_db_for_test(db);
        (workspace, dir)
    }

    fn blob(bytes: &[u8]) -> String {
        BiometricBlob::encode(bytes).blob
    }

    /// Enrol, release and revoke round-trip through the store the way the
    /// commands will.
    #[test]
    fn an_enrolment_releases_what_it_enrolled() {
        let (workspace, _dir) = workspace();
        let id = Uuid::new_v4();

        workspace
            .enrol_biometric(id, "Ved's MacBook Pro", "vault", &blob(b"wrapped-vault"))
            .expect("enrol");

        let enrolments = workspace.biometric_enrolments().expect("read");
        assert_eq!(enrolments.len(), 1);
        assert_eq!(enrolments[0].id, id);
        assert_eq!(enrolments[0].device, "Ved's MacBook Pro");
        assert_eq!(enrolment_purposes(&enrolments[0]), vec!["vault".to_owned()]);

        let released = workspace
            .release_biometric(id, "vault")
            .expect("release")
            .expect("the purpose is enrolled");
        assert_eq!(released.decode().expect("base64"), b"wrapped-vault");

        workspace.revoke_biometric(id, Some("vault")).expect("revoke");
        assert!(
            workspace.biometric_enrolments().expect("read").is_empty(),
            "the empty device is gone"
        );
        assert!(
            workspace.release_biometric(id, "vault").expect("release after revoke").is_none(),
            "a revoked purpose releases nothing",
        );
    }

    /// The brief's own acceptance criterion: a second consumer is a purpose
    /// and a blob shape, not a store change, a table, or code in the first
    /// consumer's path. The two blobs here are deliberately different shapes
    /// and lengths, and each release answers only its own bytes.
    #[test]
    fn a_second_consumer_needs_only_a_purpose() {
        let (workspace, _dir) = workspace();
        let id = Uuid::new_v4();
        let second_shape = [7u8; 300];

        workspace
            .enrol_biometric(id, "phone", "vault", &blob(b"{\"refresh\":\"r\"}"))
            .expect("enrol the vault");
        workspace
            .enrol_biometric(id, "phone", "sso", &blob(&second_shape))
            .expect("enrol a second purpose");

        let enrolments = workspace.biometric_enrolments().expect("read");
        let mut purposes = enrolment_purposes(&enrolments[0]);
        purposes.sort_unstable();
        assert_eq!(purposes, ["sso".to_owned(), "vault".to_owned()]);

        assert_eq!(
            workspace.release_biometric(id, "vault").expect("release").expect("enrolled").decode(),
            Ok(b"{\"refresh\":\"r\"}".to_vec()),
        );
        assert_eq!(
            workspace.release_biometric(id, "sso").expect("release").unwrap().decode(),
            Ok(second_shape.to_vec()),
            "the second consumer's own shape comes back whole",
        );

        // Revoking one purpose leaves the other's bytes exactly as enrolled.
        workspace.revoke_biometric(id, Some("sso")).expect("revoke the second");
        let enrolments = workspace.biometric_enrolments().expect("read");
        assert_eq!(enrolment_purposes(&enrolments[0]), vec!["vault".to_owned()]);
        assert_eq!(
            workspace.release_biometric(id, "vault").expect("release").unwrap().decode(),
            Ok(b"{\"refresh\":\"r\"}".to_vec()),
            "the first consumer's blob survives the second's revocation",
        );
    }

    /// The store keeps the exact bytes the client sent: a release is a byte
    /// copy, never a decode and re-encode by the core.
    #[test]
    fn the_stored_blob_is_the_byte_copy_of_what_was_enrolled() {
        let (workspace, dir) = workspace();
        let id = Uuid::new_v4();
        let bytes: Vec<u8> = (0u8..=255).collect();

        workspace.enrol_biometric(id, "device", "vault", &blob(&bytes)).expect("enrol");
        let released = workspace.release_biometric(id, "vault").expect("release");

        assert_eq!(released.expect("enrolled").decode(), Ok(bytes));
        drop(workspace);

        // The row on disk is the same bytes, read back by a fresh handle the
        // way the next boot reads it.
        let db = Db::open(&dir.path().join("store.redb")).expect("reopen");
        let stored = crate::store::biometric::get(&db, id).expect("read").expect("the row");
        assert_eq!(stored.purposes[0].blob, (0u8..=255).collect::<Vec<u8>>());
    }

    /// The enrolment set survives a restart: the store is the only place a
    /// recruitment lives, and a reboot with no enrolment would be a password
    /// typed for nothing.
    #[test]
    fn enrolments_survive_a_store_reopen() {
        let (workspace, dir) = workspace();
        let id = Uuid::new_v4();
        workspace.enrol_biometric(id, "MacBook Pro", "vault", &blob(b"kept")).expect("enrol");
        drop(workspace);

        let (workspace, _updates) =
            Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let db = Db::open(&dir.path().join("store.redb")).expect("reopen store");
        workspace.install_db_for_test(db);

        let enrolments = workspace.biometric_enrolments().expect("read after reopen");
        assert_eq!(enrolments.len(), 1);
        assert_eq!(enrolments[0].id, id);
        assert_eq!(
            workspace.release_biometric(id, "vault").expect("release").unwrap().decode(),
            Ok(b"kept".to_vec()),
        );
    }

    /// Revoking one device does not touch another's enrolment.
    #[test]
    fn revocation_drops_one_device_and_says_so() {
        let (workspace, _dir) = workspace();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        workspace.enrol_biometric(mine, "desktop", "vault", &blob(b"a")).expect("enrol one");
        workspace.enrol_biometric(theirs, "phone", "vault", &blob(b"b")).expect("enrol two");

        workspace.revoke_biometric(mine, None).expect("revoke the whole device");

        let enrolments = workspace.biometric_enrolments().expect("read");
        assert_eq!(enrolments.len(), 1);
        assert_eq!(enrolments[0].id, theirs);
    }

    /// Every write and release announces the set a second client redraws from;
    /// a silent write is a settings page drawing a list that is no longer
    /// true.
    #[test]
    fn every_change_announces_the_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, mut updates) =
            Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let db = Db::open(&dir.path().join("store.redb")).expect("open store");
        workspace.install_db_for_test(db);
        let id = Uuid::new_v4();

        let mut announced = Vec::new();
        workspace.enrol_biometric(id, "device", "vault", &blob(b"one")).expect("enrol");
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::BiometricEnrolmentsChanged { enrolments } =
                std::sync::Arc::unwrap_or_clone(update.update)
            {
                announced.push(enrolments);
            }
        }
        assert_eq!(announced.len(), 1, "the enrol announced the set");
        assert_eq!(announced[0].len(), 1);

        workspace.release_biometric(id, "vault").expect("release");
        let mut released_announcements = 0;
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::BiometricEnrolmentsChanged { enrolments } =
                std::sync::Arc::unwrap_or_clone(update.update)
            {
                assert!(
                    enrolments[0].purposes[0].last_used_at.is_some(),
                    "the release's stamp is in the announcement",
                );
                released_announcements += 1;
            }
        }
        assert_eq!(released_announcements, 1, "the release announced the new stamp");

        workspace.revoke_biometric(id, Some("vault")).expect("revoke");
        let mut revoked = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::BiometricEnrolmentsChanged { enrolments } =
                std::sync::Arc::unwrap_or_clone(update.update)
            {
                revoked.push(enrolments);
            }
        }
        assert_eq!(revoked, vec![Vec::new()], "the revoke announced the empty set");
    }

    /// The enrolment material answered through the command is the session's,
    /// and a locked or absent session answers `null`.
    #[test]
    fn the_enrolment_material_command_answers_the_session_or_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _updates) =
            Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());

        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        workspace
            .dispatch(crate::protocol::Command::VaultEnrolmentMaterial { respond: Some(sender) })
            .expect("the command routes");
        assert!(receiver.try_recv().expect("answered").is_none(), "no [vault] means no material");

        let config = crate::config::VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "a@b.c".to_owned(),
        };
        let session =
            Arc::new(crate::vault::test_session(&config, None, dir.path()).expect("a session"));
        crate::vault::test_install_unlocked(
            &session,
            forge_connectors::vault::crypto::Unlocked {
                user_key: forge_connectors::vault::crypto::make_symmetric_key(),
                private_key: None,
                org_keys: std::collections::HashMap::new(),
                failed_orgs: Vec::new(),
                user_key_fingerprint: String::new(),
            },
        );
        *workspace.vault.write() = Some(session);

        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        workspace
            .dispatch(crate::protocol::Command::VaultEnrolmentMaterial { respond: Some(sender) })
            .expect("the command routes");
        let material = receiver.try_recv().expect("answered").expect("the material");
        assert_eq!(material.refresh_token, "test-refresh");
        assert!(!material.user_key.is_empty(), "the user key rides with it");
    }

    /// A blob that is not base64, an empty one, an oversized one and a
    /// nameless purpose are all refused before anything is stored.
    #[test]
    fn a_blob_that_cannot_be_stored_is_refused() {
        let (workspace, _dir) = workspace();
        let id = Uuid::new_v4();

        assert!(workspace.enrol_biometric(id, "device", "vault", "not base64!").is_err());
        assert!(workspace.enrol_biometric(id, "device", "vault", &blob(b"")).is_err());
        assert!(
            workspace
                .enrol_biometric(id, "device", "vault", &blob(&vec![0u8; MAX_BLOB_BYTES + 1]))
                .is_err(),
            "a blob past the cap is refused",
        );
        assert!(workspace.enrol_biometric(id, "device", " ", &blob(b"x")).is_err());
        assert!(workspace.enrol_biometric(id, "  ", "vault", &blob(b"x")).is_err());
        assert!(
            workspace.biometric_enrolments().expect("read").is_empty(),
            "nothing a refusal touched was stored",
        );
    }

    /// The lock drops the session's keys and re-stamps a parked card so a dock
    /// that already drew one asks for the password again.
    #[test]
    fn locking_the_vault_locks_the_session_and_rebases_the_cards() {
        use crate::config::VaultConfig;
        use crate::vault::{test_install_unlocked, test_session};
        use forge_connectors::vault::crypto;
        use forge_primitives::vault::{VaultDraft, VaultTool};

        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _updates) =
            Workspace::testing_stub_with_config_dir(dir.path().to_path_buf());
        let config =
            VaultConfig { url: "http://127.0.0.1:1".to_owned(), email: "a@b.c".to_owned() };
        let session = Arc::new(test_session(&config, None, dir.path()).expect("a session"));
        test_install_unlocked(
            &session,
            crypto::Unlocked {
                user_key: crypto::make_symmetric_key(),
                private_key: None,
                org_keys: std::collections::HashMap::new(),
                failed_orgs: Vec::new(),
                user_key_fingerprint: String::new(),
            },
        );
        *workspace.vault.write() = Some(Arc::clone(&session));
        let caller = SessionSlot::lead("Default", "forge");
        let draft = VaultDraft::new(VaultTool::Get, "read the password for 'bank'");
        workspace.register_vault_ask(&caller, draft, false);
        assert!(!session.is_locked());

        assert!(workspace.lock_vault());

        assert!(session.is_locked(), "the lock drops the keys");
        let asks = workspace.vault_asks(&caller, true);
        assert!(
            asks.iter().any(|ask| matches!(
                ask,
                crate::protocol::PendingAsk::Vault(draft) if draft.needs_unlock
            )),
            "the parked card asks for the password again",
        );
    }

    /// A lock with no `[vault]` configured is a no-op that answers false,
    /// rather than a refusal a view has no card to draw.
    #[test]
    fn a_lock_with_no_vault_answers_false() {
        let (workspace, _dir) = workspace();
        assert!(!workspace.lock_vault());
    }

    fn enrolment_purposes(enrolment: &BiometricEnrolment) -> Vec<String> {
        enrolment.purposes.iter().map(|held| held.purpose.clone()).collect()
    }
}
