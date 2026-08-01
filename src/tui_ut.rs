use super::{dispatch_ui_command, mouse_to_bytes, MouseEncoding, MouseTracking, OverlayState, Term};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

#[test]
fn internal_error_creates_a_non_modal_toast() {
    let mut state = OverlayState::new();

    dispatch_ui_command(&mut state, "error", "show", "bunkerbox-vscomm", "connection failed");

    assert!(state.error_toast.is_some());
    assert!(!state.popup.visible);
    let toast = state.error_toast.as_ref().unwrap();
    assert_eq!(toast.title, "bunkerbox-vscomm");
    assert_eq!(toast.message, "connection failed");
}

#[test]
fn cursor_report_uses_position_after_prior_bytes_in_same_chunk() {
    let mut term = Term::new(24, 80);

    term.process(b"\x1b[10;20H\x1b[6n");

    assert_eq!(term.drain_responses(), vec![b"\x1b[10;20R".to_vec()]);
}

#[test]
fn cursor_report_handles_split_query() {
    let mut term = Term::new(24, 80);

    term.process(b"\x1b[10;20H\x1b[");
    assert!(term.drain_responses().is_empty());
    term.process(b"6n");

    assert_eq!(term.drain_responses(), vec![b"\x1b[10;20R".to_vec()]);
}

#[test]
fn cursor_mode_handles_split_sequences() {
    let mut term = Term::new(24, 80);

    term.process(b"\x1b[?");
    term.process(b"1h");
    assert!(term.application_cursor_keys());

    term.process(b"\x1b[?1");
    term.process(b"l");
    assert!(!term.application_cursor_keys());
}

#[test]
fn window_size_report_handles_split_query() {
    let mut term = Term::new(24, 80);

    term.process(b"\x1b[18");
    assert!(term.drain_responses().is_empty());
    term.process(b"t");

    assert_eq!(term.drain_responses(), vec![b"\x1b[8;24;80t".to_vec()]);
}

#[test]
fn mouse_modes_track_multiple_private_parameters() {
    let mut term = Term::new(24, 80);

    term.process(b"\x1b[?1000;1006h");

    assert_eq!(term.mouse_tracking(), MouseTracking::Normal);
    assert_eq!(term.mouse_encoding(), MouseEncoding::Sgr);

    term.process(b"\x1b[?1000l\x1b[?1006l");
    assert_eq!(term.mouse_tracking(), MouseTracking::Off);
    assert_eq!(term.mouse_encoding(), MouseEncoding::X10);
}

#[test]
fn sgr_mouse_click_uses_one_based_coordinates() {
    let event = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 9, row: 4, modifiers: KeyModifiers::NONE };

    assert_eq!(mouse_to_bytes(event, MouseTracking::Normal, MouseEncoding::Sgr), Some(b"\x1b[<0;10;5M".to_vec()));
}

#[test]
fn sgr_mouse_release_and_modifiers_are_encoded() {
    let event =
        MouseEvent { kind: MouseEventKind::Up(MouseButton::Right), column: 2, row: 3, modifiers: KeyModifiers::SHIFT | KeyModifiers::CONTROL };

    assert_eq!(mouse_to_bytes(event, MouseTracking::Normal, MouseEncoding::Sgr), Some(b"\x1b[<22;3;4m".to_vec()));
}

#[test]
fn legacy_mouse_encodings_use_their_wire_formats() {
    let event = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 0, row: 0, modifiers: KeyModifiers::NONE };
    assert_eq!(mouse_to_bytes(event, MouseTracking::Normal, MouseEncoding::X10), Some(vec![0x1b, b'[', b'M', 32, 33, 33]));
    assert_eq!(mouse_to_bytes(event, MouseTracking::Normal, MouseEncoding::Urxvt), Some(b"\x1b[32;1;1M".to_vec()));
}

#[test]
fn mouse_motion_requires_the_requested_tracking_level() {
    let event = MouseEvent { kind: MouseEventKind::Moved, column: 5, row: 6, modifiers: KeyModifiers::NONE };

    assert_eq!(mouse_to_bytes(event, MouseTracking::Button, MouseEncoding::Sgr), None);
    assert_eq!(mouse_to_bytes(event, MouseTracking::Any, MouseEncoding::Sgr), Some(b"\x1b[<35;6;7M".to_vec()));
}
