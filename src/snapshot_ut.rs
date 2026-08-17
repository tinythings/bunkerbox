use super::*;
use crate::cfg::{ProjectConfig, ProjectSection, RemoteSection};
use crate::remote::WorkspaceSessionId;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

fn session(value: u8) -> WorkspaceSessionId {
    WorkspaceSessionId([value; 16])
}

fn builder(store: &TempDir, limits: SnapshotLimits, patterns: &[&str]) -> SnapshotBuilder {
    SnapshotBuilder::new(
        SnapshotStore::new(store.path()),
        limits,
        SnapshotExclusionPolicy::from_patterns(patterns.iter().map(|pattern| (*pattern).to_string())).unwrap(),
    )
}

fn build_at(source: &TempDir, store: &TempDir, limits: SnapshotLimits, patterns: &[&str]) -> Result<WorkspaceSnapshot, String> {
    builder(store, limits, patterns).build_root(source.path(), session(1))
}

fn remote_config(patterns: &[&str]) -> ProjectConfig {
    ProjectConfig {
        project: ProjectSection {
            remote: RemoteSection { exclude: patterns.iter().map(|pattern| (*pattern).to_string()).collect(), ..Default::default() },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn remote_builder(store: &TempDir, limits: SnapshotLimits, config: &ProjectConfig) -> SnapshotBuilder {
    SnapshotBuilder::new(SnapshotStore::new(store.path()), limits, SnapshotExclusionPolicy::from_remote_config(config, None).unwrap())
}

fn build_remote_at(source: &TempDir, store: &TempDir, limits: SnapshotLimits, config: &ProjectConfig) -> Result<WorkspaceSnapshot, String> {
    remote_builder(store, limits, config).build_root(source.path(), session(1))
}

fn write_file(root: &Path, path: &str, contents: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn snapshots_nested_modified_and_untracked_files_with_modes() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write_file(source.path(), "src/main.rs", b"modified");
    write_file(source.path(), "agent/new.txt", b"created");
    let executable = source.path().join("tool.sh");
    fs::write(&executable, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

    let snapshot = build_at(&source, &store, SnapshotLimits::default(), &[]).unwrap();
    let paths = snapshot.entries().iter().map(|entry| entry.path().as_str()).collect::<Vec<_>>();
    assert_eq!(paths, vec!["agent", "agent/new.txt", "src", "src/main.rs", "tool.sh"]);
    assert_eq!(snapshot.total_file_bytes(), 8 + 7 + 10);
    let tool = snapshot.entries().iter().find(|entry| entry.path().as_str() == "tool.sh").unwrap();
    assert_eq!(tool.kind(), SnapshotEntryKind::RegularFile);
    assert_eq!(tool.mode(), 0o755);
    assert!(tool.content_digest().is_some());
    assert!(store.path().to_string_lossy().is_empty() || format!("{:?}", snapshot).contains("SnapshotId"));
}

#[test]
fn identical_trees_have_deterministic_manifest_identity() {
    let source_a = TempDir::new().unwrap();
    let source_b = TempDir::new().unwrap();
    let store_a = TempDir::new().unwrap();
    let store_b = TempDir::new().unwrap();
    for source in [&source_a, &source_b] {
        write_file(source.path(), "b/file", b"same");
        write_file(source.path(), "a.txt", b"content");
    }

    let first = build_at(&source_a, &store_a, SnapshotLimits::default(), &[]).unwrap();
    let second = build_at(&source_b, &store_b, SnapshotLimits::default(), &[]).unwrap();
    assert_eq!(first.handle().snapshot_id(), second.handle().snapshot_id());
    assert_eq!(first.entries().iter().map(|entry| entry.path().as_str()).collect::<Vec<_>>(), vec!["a.txt", "b", "b/file"]);
}

#[test]
fn snapshot_store_resolves_only_the_bound_session() {
    let source = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    write_file(source.path(), "file", b"data");
    let snapshot = build_at(&source, &store_dir, SnapshotLimits::default(), &[]).unwrap();
    let store = SnapshotStore::new(store_dir.path());
    assert_eq!(store.resolve(snapshot.handle()).unwrap(), snapshot);

    let wrong_session = SnapshotHandle { session_id: session(2), snapshot_id: snapshot.handle().snapshot_id() };
    assert!(store.resolve(&wrong_session).is_err());
    assert!(!format!("{:?}", snapshot.handle()).contains(&source.path().to_string_lossy().to_string()));
}

#[test]
fn exclusions_prune_defaults_basenames_and_anchored_subtrees() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    for path in ["target/out", "nested/target/out", "docs/generated/file", ".git/config", ".bunkerbox/state", ".env", ".ssh/key"] {
        write_file(source.path(), path, b"excluded");
    }
    write_file(source.path(), "docs/keep/file", b"included");
    let snapshot = build_at(&source, &store, SnapshotLimits::default(), &["target/", "docs/generated/"]).unwrap();
    let paths = snapshot.entries().iter().map(|entry| entry.path().as_str()).collect::<Vec<_>>();
    assert_eq!(paths, vec!["docs", "docs/keep", "docs/keep/file", "nested"]);
}

#[test]
fn config_and_runtime_exclusions_use_explicit_snapshot_semantics() {
    let config = ProjectConfig { project: ProjectSection { exclude: vec!["vendor/".into()], ..Default::default() }, ..Default::default() };
    let policy = SnapshotExclusionPolicy::from_config(&config, Some(&["generated/tree/".to_string()])).unwrap();
    assert!(policy.excludes("vendor/file"));
    assert!(policy.excludes("generated/tree/file"));
    assert!(!policy.excludes("vendorized/file"));
}

#[test]
fn remote_exclusions_use_basename_and_root_anchored_semantics() {
    let config = remote_config(&[".tmp", "docs/generated"]);
    let policy = SnapshotExclusionPolicy::from_remote_config(&config, None).unwrap();

    assert!(policy.excludes(".tmp/electron"));
    assert!(policy.excludes("nested/.tmp/electron"));
    assert!(policy.excludes("docs/generated/file"));
    assert!(!policy.excludes("nested/docs/generated/file"));
    assert!(!policy.excludes("docs/generated-other/file"));
}

#[test]
fn mandatory_snapshot_exclusions_remain_enforced_for_remote_config() {
    let config = remote_config(&[".git", ".bunker", ".bunkerbox", ".ssh", ".env", ".envrc"]);
    let policy = SnapshotExclusionPolicy::from_remote_config(&config, None).unwrap();

    for path in [".git/config", "nested/.bunker/state", ".bunkerbox/control", ".ssh/key", ".env", "nested/.envrc"] {
        assert!(policy.excludes(path), "{path}");
    }
}

#[test]
fn malformed_exclusions_are_rejected() {
    for pattern in ["/absolute", "foo/../bar", "foo//bar", "foo/./bar", ""] {
        assert!(SnapshotExclusionPolicy::from_patterns([pattern.to_string()]).is_err(), "{pattern}");
    }
}

#[test]
fn every_symlink_is_rejected_without_following_it() {
    let cases = ["internal-file", "internal-dir", "external", "dangling", "loop-a"];
    for case in cases {
        let source = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        write_file(source.path(), "real/file", b"data");
        match case {
            "internal-file" => symlink("real/file", source.path().join(case)).unwrap(),
            "internal-dir" => symlink("real", source.path().join(case)).unwrap(),
            "external" => {
                let outside = TempDir::new().unwrap();
                symlink(outside.path(), source.path().join(case)).unwrap();
            }
            "dangling" => symlink("missing", source.path().join(case)).unwrap(),
            "loop-a" => {
                symlink("loop-b", source.path().join("loop-a")).unwrap();
                symlink("loop-a", source.path().join("loop-b")).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(build_at(&source, &store, SnapshotLimits::default(), &[]).is_err(), "{case}");
    }
}

#[test]
fn remote_exclusions_skip_oversized_files_and_symlinks_before_validation() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write_file(source.path(), ".tmp/electron", b"oversized");
    let symlink_path = source.path().join(".venv-docs/bin/python");
    fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
    symlink("missing-python", &symlink_path).unwrap();

    let config = remote_config(&[".tmp", ".venv-docs"]);
    let snapshot = build_remote_at(&source, &store, SnapshotLimits { max_file_bytes: 4, ..SnapshotLimits::default() }, &config).unwrap();

    assert!(snapshot.entries().is_empty());
}

#[test]
fn remote_exclusions_do_not_bypass_snapshot_validation_outside_excluded_trees() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write_file(source.path(), "other/electron", b"oversized");
    let config = remote_config(&[".tmp", ".venv-docs"]);
    assert!(build_remote_at(&source, &store, SnapshotLimits { max_file_bytes: 4, ..SnapshotLimits::default() }, &config).is_err());

    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    let symlink_path = source.path().join("other/python");
    fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
    symlink("missing-python", &symlink_path).unwrap();
    assert!(build_remote_at(&source, &store, SnapshotLimits::default(), &config).is_err());
}

#[test]
fn special_files_and_hard_links_are_rejected() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write_file(source.path(), "regular", b"data");
    fs::hard_link(source.path().join("regular"), source.path().join("alias")).unwrap();
    assert!(build_at(&source, &store, SnapshotLimits::default(), &[]).is_err());

    let fifo = source.path().join("pipe");
    let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
    assert!(build_at(&source, &store, SnapshotLimits::default(), &[]).is_err());

    fs::remove_file(&fifo).unwrap();
    let socket_path = source.path().join("socket");
    let _listener = UnixListener::bind(&socket_path).unwrap();
    assert!(build_at(&source, &store, SnapshotLimits::default(), &[]).is_err());
}

#[test]
fn trusted_limits_reject_entries_and_cleanup_staging() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write_file(source.path(), "a", b"1234");
    write_file(source.path(), "b", b"5678");
    let limits = SnapshotLimits { max_entries: 1, ..SnapshotLimits::default() };
    assert!(build_at(&source, &store, limits, &[]).is_err());
    assert_eq!(fs::read_dir(store.path().join(".staging")).unwrap().count(), 0);

    let limits = SnapshotLimits { max_file_bytes: 3, ..SnapshotLimits::default() };
    assert!(build_at(&source, &store, limits, &[]).is_err());
    assert_eq!(fs::read_dir(store.path().join(".staging")).unwrap().count(), 0);

    let limits = SnapshotLimits { max_total_bytes: 5, ..SnapshotLimits::default() };
    assert!(build_at(&source, &store, limits, &[]).is_err());
    assert_eq!(fs::read_dir(store.path().join(".staging")).unwrap().count(), 0);
}

#[test]
fn path_manifest_and_deadline_limits_are_enforced() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write_file(source.path(), "long-name", b"data");
    let limits = SnapshotLimits { max_component_bytes: 4, ..SnapshotLimits::default() };
    assert!(build_at(&source, &store, limits, &[]).is_err());

    let limits = SnapshotLimits { max_manifest_bytes: 1, ..SnapshotLimits::default() };
    assert!(build_at(&source, &store, limits, &[]).is_err());

    let limits = SnapshotLimits { max_duration: Duration::from_nanos(1), ..SnapshotLimits::default() };
    assert!(build_at(&source, &store, limits, &[]).is_err());
}

#[test]
fn zero_session_is_not_accepted_as_snapshot_authority() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write_file(source.path(), "file", b"data");
    assert!(builder(&store, SnapshotLimits::default(), &[]).build_root(source.path(), WorkspaceSessionId([0; 16])).is_err());
}

#[test]
fn unreadable_workspace_entry_fails_when_supported() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    let path = source.path().join("private");
    fs::write(&path, b"secret").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let result = build_at(&source, &store, SnapshotLimits::default(), &[]);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert!(result.is_err());
    }
}

#[test]
fn source_path_is_not_in_snapshot_metadata() {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    write_file(source.path(), "file", b"data");
    let snapshot = build_at(&source, &store, SnapshotLimits::default(), &[]).unwrap();
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains(&source.path().to_string_lossy().to_string()));
    assert!(!debug.contains(&store.path().to_string_lossy().to_string()));
}

#[test]
fn materialization_recreates_nested_files_and_modes() {
    let source = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    write_file(source.path(), "src/main.rs", b"fn main() {}\n");
    let executable = source.path().join("tool.sh");
    fs::write(&executable, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let snapshot = build_at(&source, &store_dir, SnapshotLimits::default(), &[]).unwrap();
    let store = SnapshotStore::new(store_dir.path());
    let destination = store_dir.path().join("materialized");

    let materialized = store.materialize(snapshot.handle(), &destination).unwrap();
    assert_eq!(materialized.root(), destination);
    assert_eq!(fs::read(destination.join("src/main.rs")).unwrap(), b"fn main() {}\n");
    assert_eq!(fs::metadata(destination.join("tool.sh")).unwrap().permissions().mode() & 0o777, 0o755);
}

#[test]
fn materialization_rejects_existing_destination_and_cleans_digest_failures() {
    let source = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    write_file(source.path(), "file", b"contents");
    let snapshot = build_at(&source, &store_dir, SnapshotLimits::default(), &[]).unwrap();
    let store = SnapshotStore::new(store_dir.path());
    let existing = store_dir.path().join("existing");
    fs::create_dir(&existing).unwrap();
    assert!(store.materialize(snapshot.handle(), &existing).is_err());

    let staged_file = store.snapshot_path(snapshot.handle()).join("files/file");
    fs::write(staged_file, b"tampered").unwrap();
    let destination = store_dir.path().join("failed");
    assert!(store.materialize(snapshot.handle(), &destination).is_err());
    assert!(!destination.exists());
}

#[test]
fn materialization_rejects_destination_symlink() {
    let source = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    write_file(source.path(), "file", b"contents");
    let snapshot = build_at(&source, &store_dir, SnapshotLimits::default(), &[]).unwrap();
    let destination = store_dir.path().join("link");
    symlink(outside.path(), &destination).unwrap();
    assert!(SnapshotStore::new(store_dir.path()).materialize(snapshot.handle(), &destination).is_err());
    assert!(outside.path().read_dir().unwrap().next().is_none());
}
