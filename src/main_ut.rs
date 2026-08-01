use super::{decode_workspace_handoff, encode_workspace_handoff};
use std::path::Path;

#[test]
fn workspace_handoff_round_trips_a_path() {
    let frame = encode_workspace_handoff(b"/workspace/project").unwrap();
    let path = decode_workspace_handoff(&frame).unwrap();

    assert_eq!(path, Path::new("/workspace/project"));
}

#[test]
fn ui_message_is_not_a_workspace_handoff() {
    let ui_message = b"@popup\0info\0Bunkerbox\0Starting...\0\n";

    assert!(decode_workspace_handoff(ui_message).is_err());
}

#[test]
fn workspace_handoff_rejects_truncated_payload() {
    let frame = encode_workspace_handoff(b"/workspace/project").unwrap();

    assert!(decode_workspace_handoff(&frame[..frame.len() - 1]).is_err());
}
