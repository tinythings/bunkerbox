use super::*;
use std::fs;
use tempfile::TempDir;

fn write_file(root: &Path, name: &str, content: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(name), content).unwrap();
}

/// Relaxed mode returns glob patterns for detected build systems.
#[test]
fn relaxed_returns_globs() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "Cargo.toml", "[package]\nname = \"test\"\n");
    let entries = scan(root.path(), PassthroughMode::Relaxed);
    assert!(entries.contains(&"cargo *".to_string()));
}

/// Paranoid mode returns exact commands with no globs.
#[test]
fn paranoid_returns_exact() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "Cargo.toml", "[package]\nname = \"test\"\n");
    let entries = scan(root.path(), PassthroughMode::Paranoid);
    assert!(entries.contains(&"cargo check".to_string()));
    assert!(entries.contains(&"cargo build".to_string()));
    assert!(entries.contains(&"cargo test".to_string()));
}

/// Paranoid mode output contains zero glob patterns.
#[test]
fn paranoid_no_globs() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "Cargo.toml", "[package]\n");
    write_file(root.path(), "Makefile", "all:\n\techo ok\n");
    let entries = scan(root.path(), PassthroughMode::Paranoid);
    assert!(!entries.iter().any(|e| e.contains('*')));
}

/// Makefile .PHONY targets are extracted in paranoid mode.
#[test]
fn make_paranoid_reads_phony() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "Makefile", ".PHONY: dev test lint\n\ndev:\n\techo ok\n");
    let entries = scan(root.path(), PassthroughMode::Paranoid);
    assert!(entries.contains(&"make dev".to_string()));
    assert!(entries.contains(&"make test".to_string()));
    assert!(entries.contains(&"make lint".to_string()));
}

/// Makefile without .PHONY falls back to standard targets.
#[test]
fn make_paranoid_fallback() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "Makefile", "all:\n\techo ok\n");
    let entries = scan(root.path(), PassthroughMode::Paranoid);
    assert!(entries.contains(&"make build".to_string()));
    assert!(entries.contains(&"make test".to_string()));
}

/// Empty project returns empty list regardless of mode.
#[test]
fn empty_project_both_modes() {
    let root = TempDir::new().unwrap();
    assert!(scan(root.path(), PassthroughMode::Relaxed).is_empty());
    assert!(scan(root.path(), PassthroughMode::Paranoid).is_empty());
}
