use super::Term;

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
