use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn executable(path: &Path) {
    fs::write(path, b"binary").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn synchronize(root: &Path, names: &[&str], remote: &Path, vscomm: &Path) {
    synchronize_remote_wrappers(names.iter().map(|name| (*name).to_string()), root, remote, vscomm).unwrap();
}

#[test]
fn configured_arbitrary_wrappers_survive_vscomm_install() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    synchronize(root.path(), &["make", "cargo", "build-my-car"], &remote, &vscomm);
    install_vscomm_links(vec!["make".into(), "cargo".into(), "build-my-car".into()], root.path(), &vscomm, "").unwrap();

    for name in ["make", "cargo", "build-my-car"] {
        assert_eq!(fs::read_link(root.path().join(name)).unwrap(), remote);
        assert!(is_managed_remote_wrapper(root.path(), name).unwrap());
    }
}

#[test]
fn removing_a_configured_tool_removes_only_its_managed_wrapper() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    synchronize(root.path(), &["make", "build-my-car"], &remote, &vscomm);
    synchronize(root.path(), &["build-my-car"], &remote, &vscomm);

    assert!(fs::symlink_metadata(root.path().join("make")).is_err());
    assert_eq!(fs::read_link(root.path().join("build-my-car")).unwrap(), remote);
    assert!(!is_managed_remote_wrapper(root.path(), "make").unwrap());
    assert!(is_managed_remote_wrapper(root.path(), "build-my-car").unwrap());
}

#[test]
fn unmanaged_entries_are_never_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);
    fs::write(root.path().join("build-my-car"), b"unmanaged").unwrap();

    assert!(synchronize_remote_wrappers(vec!["build-my-car".into()], root.path(), &remote, &vscomm).is_err());
    assert_eq!(fs::read(root.path().join("build-my-car")).unwrap(), b"unmanaged");
}

#[test]
fn stale_managed_links_are_repaired_and_repeated_install_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    synchronize(root.path(), &["build-my-car"], &remote, &vscomm);
    synchronize(root.path(), &["build-my-car"], &remote, &vscomm);
    fs::remove_file(root.path().join("build-my-car")).unwrap();
    symlink(root.path().join("old/bunkerbox-remote"), root.path().join("build-my-car")).unwrap();
    synchronize(root.path(), &["build-my-car"], &remote, &vscomm);

    assert_eq!(fs::read_link(root.path().join("build-my-car")).unwrap(), remote);
}

#[test]
fn vscomm_to_remote_transition_is_allowed_only_for_the_configured_name() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    install_vscomm_links(vec!["build-my-car".into()], root.path(), &vscomm, "").unwrap();
    synchronize(root.path(), &["build-my-car"], &remote, &vscomm);

    assert_eq!(fs::read_link(root.path().join("build-my-car")).unwrap(), remote);
}

#[test]
fn changed_state_tracked_link_becomes_unmanaged_and_is_not_removed() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    let other = root.path().join("other");
    executable(&remote);
    executable(&vscomm);
    executable(&other);

    synchronize(root.path(), &["build-my-car"], &remote, &vscomm);
    fs::remove_file(root.path().join("build-my-car")).unwrap();
    symlink(&other, root.path().join("build-my-car")).unwrap();
    assert!(synchronize_remote_wrappers(vec!["build-my-car".into()], root.path(), &remote, &vscomm).is_err());

    assert_eq!(fs::read_link(root.path().join("build-my-car")).unwrap(), other);
    assert!(!is_managed_remote_wrapper(root.path(), "build-my-car").unwrap());
}

#[test]
fn invalid_and_reserved_wrapper_names_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    for name in ["", ".", "..", "../escape", "bunkerbox", "bunkerbox-remote", REMOTE_WRAPPER_STATE_FILE] {
        assert!(validate_remote_wrapper_name(name.to_string()).is_err(), "accepted reserved or invalid wrapper: {name}");
        assert!(synchronize_remote_wrappers(vec![name.to_string()], root.path(), &remote, &vscomm).is_err());
    }
}

#[test]
fn no_state_means_existing_remote_link_is_unmanaged() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);
    symlink(&remote, root.path().join("build-my-car")).unwrap();

    assert!(synchronize_remote_wrappers(vec!["build-my-car".into()], root.path(), &remote, &vscomm).is_err());
    assert_eq!(fs::read_link(root.path().join("build-my-car")).unwrap(), remote);
}

#[test]
fn wrapper_state_is_private_and_symlink_state_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    let outside = root.path().join("outside");
    executable(&remote);
    executable(&vscomm);
    fs::write(&outside, b"outside").unwrap();

    synchronize(root.path(), &["build-my-car"], &remote, &vscomm);
    let state = root.path().join(REMOTE_WRAPPER_STATE_FILE);
    let metadata = fs::metadata(&state).unwrap();
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.permissions().mode() & 0o077, 0);
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });

    fs::remove_file(&state).unwrap();
    symlink(&outside, &state).unwrap();
    assert!(synchronize_remote_wrappers(Vec::new(), root.path(), &remote, &vscomm).is_err());
    assert_eq!(fs::read_link(state).unwrap(), outside);
}
