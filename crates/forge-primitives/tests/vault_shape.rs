//! The vault family's ask shape, as another crate reads it: the draft a held
//! call parks and the tool names it answers to. The wire spelling is the
//! family's double-underscore MCP name (`vault__get`), which is also what the
//! card and the audit line carry.

use forge_primitives::vault::{VaultDraft, VaultTool};

#[test]
fn the_thirteen_tools_name_themselves_and_parse_back() {
    assert_eq!(VaultTool::ALL.len(), 13);
    for tool in VaultTool::ALL {
        assert_eq!(VaultTool::parse(tool.as_str()), Some(tool));
    }
}

#[test]
fn a_draft_serialises_without_a_secret_field() {
    let draft = VaultDraft::new(VaultTool::Get, "read the password for 'bank'");
    let value = serde_json::to_value(&draft).expect("a draft serialises");
    let object = value.as_object().expect("a draft serialises as an object");
    assert!(!object.contains_key("password"));
    assert!(!object.contains_key("secret"));
    assert!(!object.contains_key("value"));
    assert!(!object.contains_key("totp"));
}
