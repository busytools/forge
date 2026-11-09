//! `biometric_enrolments()`: the device enrolments the settings page lists
//! and the unlock card reads.

use forge_primitives::biometric::BiometricEnrolment;

use super::ViewSurface;

impl ViewSurface {
    /// Every device's enrolment, with no wrapped blob in it: a view draws
    /// which purposes a device enrolled and when each was last released, and
    /// the bytes stay behind a release.
    ///
    /// A store that cannot be read is an error rather than an empty list, so
    /// the page can tell "no device is enrolled" from "the list could not be
    /// read".
    pub fn biometric_enrolments(&self) -> Result<Vec<BiometricEnrolment>, String> {
        self.workspace.biometric_enrolments()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::biometric::BiometricBlob;
    use forge_workspace::Command;

    use super::*;

    /// The two commands a client drives answer with what a view reads: an
    /// enrol lands in the list, and a release hands back the same bytes.
    #[test]
    fn an_enrolment_is_what_the_view_reads_and_what_a_release_answers() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let id = uuid::Uuid::new_v4();
        let blob = BiometricBlob::encode(b"wrapped-secret");

        workspace
            .dispatch(Command::BiometricEnrol {
                enrolment: id,
                device: "Ved's MacBook Pro".to_owned(),
                purpose: "vault".to_owned(),
                blob: blob.blob.clone(),
            })
            .expect("the enrol routes");

        let view = ViewSurface::new(Arc::clone(&workspace)).biometric_enrolments().expect("read");
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].id, id);
        assert_eq!(view[0].device, "Ved's MacBook Pro");
        assert_eq!(view[0].purposes[0].purpose, "vault");
        assert!(
            serde_json::to_value(&view[0]).expect("renders").get("blob").is_none(),
            "the list a view reads carries no wrapped blob",
        );

        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        workspace
            .dispatch(Command::BiometricRelease {
                enrolment: id,
                purpose: "vault".to_owned(),
                respond: Some(sender),
            })
            .expect("the release routes");
        let answer = receiver.try_recv().expect("answered").expect("the purpose is enrolled");
        assert_eq!(answer, blob);
    }

    /// A release naming a purpose the device never enrolled answers `null`,
    /// which is the state a client falls back to the password on.
    #[test]
    fn a_release_for_an_unenrolled_purpose_answers_nothing() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();

        workspace
            .dispatch(Command::BiometricRelease {
                enrolment: uuid::Uuid::new_v4(),
                purpose: "vault".to_owned(),
                respond: Some(sender),
            })
            .expect("the release routes");

        assert_eq!(receiver.try_recv().expect("answered"), None);
    }
}
