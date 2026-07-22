use super::*;

#[test]
fn test_parse_host_port_with_port() {
    let (host, port) = parse_host_port("example.com:443").unwrap();
    assert_eq!(host, "example.com");
    assert_eq!(port, "443");
}

#[test]
fn test_parse_host_port_without_port() {
    let (host, port) = parse_host_port("example.com").unwrap();
    assert_eq!(host, "example.com");
    assert_eq!(port, "443");
}

#[test]
fn test_is_allowed_exact_match() {
    let allow = vec!["crates.io".to_string()];
    assert!(is_allowed("crates.io", &allow));
}

#[test]
fn test_is_allowed_subdomain_match() {
    let allow = vec!["crates.io".to_string()];
    assert!(is_allowed("static.crates.io", &allow));
}

#[test]
fn test_is_allowed_not_matched() {
    let allow = vec!["crates.io".to_string()];
    assert!(!is_allowed("evil.com", &allow));
}

#[test]
fn test_is_allowed_case_insensitive() {
    let allow = vec!["Crates.IO".to_string()];
    assert!(is_allowed("static.crates.io", &allow));
}

#[test]
fn test_is_allowed_partial_no_match() {
    let allow = vec!["crates.io".to_string()];
    assert!(!is_allowed("notcrates.io", &allow));
}
