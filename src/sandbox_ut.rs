use super::*;

#[test]
fn parse_version_handles_various_outputs() {
    assert_eq!(parse_bwrap_version("bubblewrap 0.11.0"), Some((0, 11, 0)));
    assert_eq!(parse_bwrap_version("bwrap 0.10.0"), Some((0, 10, 0)));
    assert_eq!(parse_bwrap_version("bubblewrap v0.9.0"), Some((0, 9, 0)));
    assert_eq!(parse_bwrap_version("0.10.0"), Some((0, 10, 0)));
    assert_eq!(parse_bwrap_version(""), None);
    assert_eq!(parse_bwrap_version("unknown thing here"), None);
}

#[test]
fn version_comparison_works() {
    assert!((0, 10, 0) >= MIN_BWRAP_VERSION);
    assert!((0, 11, 0) >= MIN_BWRAP_VERSION);
    assert!((0, 9, 0) < MIN_BWRAP_VERSION);
    assert!((0, 10, 1) >= MIN_BWRAP_VERSION);
}
