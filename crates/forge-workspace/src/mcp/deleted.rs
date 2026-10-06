//! The record-echo envelope the delete-shaped tool results answer with.

/// The result a delete or unsubscribe answers with: the status word plus
/// the record as it stood just before removal, so the caller can name what
/// the call touched. A family with more to say adds its own keys beside
/// these (`tasks__delete` adds `children_removed`).
pub(crate) fn removed_record(removed: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "status": "deleted", "removed": removed })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope is a contract every adopting family shares: the status
    /// word a reader matches and the key the record sits under. Pinned
    /// once here so the families cannot drift apart on it.
    #[test]
    fn the_envelope_names_the_status_and_the_removed_record() {
        let envelope = removed_record(&serde_json::json!({ "id": "x" }));
        assert_eq!(envelope["status"], "deleted");
        assert_eq!(envelope["removed"]["id"], "x");
    }
}
