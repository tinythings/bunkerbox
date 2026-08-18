use super::{
    dispatch_ui_command, guest_rows, mouse_to_bytes, process_status_bytes, HostPopup, HostUiState, MouseEncoding, MouseTracking, OverlayState,
    RemoteSetupState, SetupScreen, Term,
};
use crate::cfg::ProjectConfig;
use crate::remote_target::{ActiveBuildTarget, BuildTargetCatalog, RemoteConfigDraft, RemoteTargetDraft};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[test]
fn guest_viewport_reserves_exactly_one_physical_row() {
    assert_eq!(guest_rows(24), 23);
    assert_eq!(guest_rows(1), 1);
    assert_eq!(guest_rows(0), 1);
}

#[test]
fn host_target_shortcut_is_consumed_before_guest_bytes() {
    let catalog = BuildTargetCatalog::localhost_only(PathBuf::from("/tmp/project"), ProjectConfig::default()).unwrap();
    let active = ActiveBuildTarget::new();
    let mut host = HostUiState::new(&catalog);
    let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL | KeyModifiers::ALT);
    assert!(host.handle_key(key, &catalog, &active));
    assert_eq!(host.popup, HostPopup::Targets);
}

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
fn startup_status_is_processed_before_child_status() {
    let overlay = Arc::new(Mutex::new(OverlayState::new()));
    let mut buffer = Vec::new();
    let mut message = b"@".to_vec();
    message.extend_from_slice(&crate::vscomm::encode_ui_payload("status", "set", "", "Preparing workspace..."));
    message.push(b'\n');

    let split = message.len() / 2;
    process_status_bytes(&mut buffer, &message[..split], &overlay);
    assert!(!overlay.lock().unwrap().popup.visible);
    process_status_bytes(&mut buffer, &message[split..], &overlay);
    assert!(overlay.lock().unwrap().popup.visible);
}

#[test]
fn hiding_password_clears_sensitive_popup_state() {
    let mut state = OverlayState::new();
    dispatch_ui_command(&mut state, "password", "show", "Password", "Enter password");
    assert!(state.popup.password_value().is_some());
    state.popup.hide();
    assert!(state.popup.password_value().is_none());
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

#[test]
fn remote_setup_shortcut_is_consumed_and_opens_empty_first_run_form() {
    let catalog = BuildTargetCatalog::localhost_only(PathBuf::from("/tmp/project"), ProjectConfig::default()).unwrap();
    let active = ActiveBuildTarget::new();
    let mut host = HostUiState::new(&catalog);
    let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL | KeyModifiers::ALT);

    assert!(host.handle_key(key, &catalog, &active));
    assert_eq!(host.popup, HostPopup::Setup);
    assert!(host.setup.as_ref().unwrap().draft.targets.is_empty());
    assert!(host.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &catalog, &active));
    assert_eq!(host.popup, HostPopup::None);
}

#[test]
fn remote_setup_target_form_editing_is_draft_only_until_save() {
    let catalog = BuildTargetCatalog::localhost_only(PathBuf::from("/tmp/project"), ProjectConfig::default()).unwrap();
    let active = ActiveBuildTarget::new();
    let mut host = HostUiState::new(&catalog);
    host.open_setup(&catalog);

    host.handle_setup_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &catalog);
    assert_eq!(host.setup.as_ref().unwrap().screen, SetupScreen::TargetForm);
    host.handle_setup_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &catalog);
    host.handle_setup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &catalog);
    assert_eq!(host.setup.as_ref().unwrap().screen, SetupScreen::List);
    assert!(host.setup.as_ref().unwrap().draft.targets.is_empty());

    assert!(host.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &catalog, &active));
    assert_eq!(host.popup, HostPopup::None);
}

#[test]
fn remote_setup_delete_requires_confirmation_and_only_changes_the_draft() {
    let catalog = BuildTargetCatalog::localhost_only(PathBuf::from("/tmp/project"), ProjectConfig::default()).unwrap();
    let active = ActiveBuildTarget::new();
    let mut draft = RemoteConfigDraft::default();
    draft.targets.insert(
        "builder".to_string(),
        RemoteTargetDraft { ssh: "builder@build.example.test".to_string(), workspace: "/var/tmp/work".to_string(), project: None, resources: None },
    );
    let mut host = HostUiState::new(&catalog);
    host.setup = Some(RemoteSetupState::new(draft));
    host.popup = HostPopup::Setup;

    host.handle_setup_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &catalog);
    assert_eq!(host.setup.as_ref().unwrap().screen, SetupScreen::ConfirmDelete);
    host.handle_setup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &catalog);
    assert!(host.setup.as_ref().unwrap().draft.targets.contains_key("builder"));
    host.handle_setup_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &catalog);
    host.handle_setup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &catalog);
    assert!(!host.setup.as_ref().unwrap().draft.targets.contains_key("builder"));
    assert_eq!(host.popup, HostPopup::Setup);
    assert_eq!(active.current(), "localhost");
}

#[test]
fn malformed_remote_setup_is_reported_without_creating_an_editable_draft() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join(".bunkerbox")).unwrap();
    let config = project.path().join(".bunkerbox").join(crate::remote_target::REMOTE_PROJECT_CONFIG_FILE_NAME);
    std::fs::write(&config, "targets: [broken]\n").unwrap();
    let catalog = BuildTargetCatalog::localhost_only(project.path().to_path_buf(), ProjectConfig::default()).unwrap();
    let active = ActiveBuildTarget::new();
    let mut host = HostUiState::new(&catalog);

    assert!(host.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL | KeyModifiers::ALT), &catalog, &active));
    assert_eq!(host.popup, HostPopup::ConfigError);
    assert!(host.setup.is_none());
    host.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), &catalog, &active);
    assert!(host.config_error.as_ref().unwrap().view);
    host.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &catalog, &active);
    assert_eq!(host.popup, HostPopup::None);
    assert_eq!(std::fs::read_to_string(config).unwrap(), "targets: [broken]\n");
}

#[test]
fn remote_setup_save_persists_for_next_run_without_mutating_frozen_runtime_state() {
    let project = tempfile::tempdir().unwrap();
    let catalog = BuildTargetCatalog::localhost_only(project.path().to_path_buf(), ProjectConfig::default()).unwrap();
    let active = ActiveBuildTarget::new();
    let mut draft = RemoteConfigDraft::default();
    draft.targets.insert(
        "builder".to_string(),
        RemoteTargetDraft { ssh: "builder@build.example.test".to_string(), workspace: "/var/tmp/work".to_string(), project: None, resources: None },
    );
    let mut host = HostUiState::new(&catalog);
    host.setup = Some(RemoteSetupState::new(draft));
    host.popup = HostPopup::Setup;

    host.handle_setup_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &catalog);
    assert_eq!(host.popup, HostPopup::None);
    assert_eq!(active.current(), "localhost");
    assert_eq!(catalog.summaries().iter().map(|target| target.label()).collect::<Vec<_>>(), vec!["localhost"]);
    assert!(host.confirmation.as_ref().unwrap().0.contains("next run"));
    let saved = crate::remote_target::RemoteConfigDraft::load_optional(project.path(), catalog.base_project()).unwrap().unwrap();
    assert!(saved.targets.contains_key("builder"));
}
