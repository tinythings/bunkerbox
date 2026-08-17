use super::*;
use std::os::unix::fs::PermissionsExt;

fn executable(path: &Path) {
    fs::write(path, b"binary").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn remote_make_ownership_survives_local_passthrough_install() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    install_remote_make_link(root.path(), &remote, true).unwrap();
    install_vscomm_links(vec!["make".into(), "cargo".into()], root.path(), &vscomm, "").unwrap();

    assert_eq!(fs::read_link(root.path().join("make")).unwrap(), remote);
    assert_eq!(fs::read_link(root.path().join("cargo")).unwrap(), vscomm);
}

#[test]
fn remote_cargo_ownership_survives_local_passthrough_install() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    fs::create_dir(&native).unwrap();
    executable(&native.join("cargo"));
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    install_remote_cargo_link(root.path(), &remote, true).unwrap();
    install_vscomm_links(vec!["cargo".into()], root.path(), &vscomm, native.to_str().unwrap()).unwrap();

    assert_eq!(fs::read_link(root.path().join("cargo")).unwrap(), remote);
}

#[test]
fn disabled_remote_make_preserves_native_and_vscomm_behavior() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    fs::create_dir(&native).unwrap();
    let native_make = native.join("make");
    executable(&native_make);
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    install_remote_make_link(root.path(), &remote, false).unwrap();
    install_vscomm_links(vec!["make".into(), "cargo".into()], root.path(), &vscomm, native.to_str().unwrap()).unwrap();

    assert_eq!(fs::read_link(root.path().join("make")).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(fs::read_link(root.path().join("cargo")).unwrap(), vscomm);
}

#[test]
fn disabled_remote_cargo_preserves_native_behavior() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    fs::create_dir(&native).unwrap();
    executable(&native.join("cargo"));
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    install_remote_cargo_link(root.path(), &remote, true).unwrap();
    install_remote_cargo_link(root.path(), &remote, false).unwrap();
    install_vscomm_links(vec!["cargo".into()], root.path(), &vscomm, native.to_str().unwrap()).unwrap();

    assert!(fs::symlink_metadata(root.path().join("cargo")).is_err());
}

#[test]
fn remote_make_wins_when_native_make_is_present() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    fs::create_dir(&native).unwrap();
    executable(&native.join("make"));
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    install_remote_make_link(root.path(), &remote, true).unwrap();
    install_vscomm_links(vec!["make".into()], root.path(), &vscomm, native.to_str().unwrap()).unwrap();

    assert_eq!(fs::read_link(root.path().join("make")).unwrap(), remote);
}

#[test]
fn remote_cargo_wins_when_native_cargo_is_present() {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    fs::create_dir(&native).unwrap();
    executable(&native.join("cargo"));
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    install_remote_cargo_link(root.path(), &remote, true).unwrap();
    install_vscomm_links(vec!["cargo".into()], root.path(), &vscomm, native.to_str().unwrap()).unwrap();

    assert_eq!(fs::read_link(root.path().join("cargo")).unwrap(), remote);
}

#[test]
fn repeated_install_is_idempotent_and_stale_managed_links_are_replaced() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    let vscomm = root.path().join("bunkerbox-vscomm");
    executable(&remote);
    executable(&vscomm);

    install_remote_make_link(root.path(), &remote, true).unwrap();
    install_remote_make_link(root.path(), &remote, true).unwrap();
    install_vscomm_links(vec!["make".into(), "cargo".into()], root.path(), &vscomm, "").unwrap();
    install_vscomm_links(vec!["make".into(), "cargo".into()], root.path(), &vscomm, "").unwrap();
    assert_eq!(fs::read_link(root.path().join("make")).unwrap(), remote);
    assert_eq!(fs::read_link(root.path().join("cargo")).unwrap(), vscomm);

    fs::remove_file(root.path().join("make")).unwrap();
    symlink(root.path().join("old/bunkerbox-remote"), root.path().join("make")).unwrap();
    install_remote_make_link(root.path(), &remote, true).unwrap();
    assert_eq!(fs::read_link(root.path().join("make")).unwrap(), remote);

    install_remote_make_link(root.path(), &remote, false).unwrap();
    assert!(fs::symlink_metadata(root.path().join("make")).is_err());
}

#[test]
fn repeated_cargo_install_is_idempotent_and_repairs_stale_managed_links() {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("bunkerbox-remote");
    executable(&remote);

    install_remote_cargo_link(root.path(), &remote, true).unwrap();
    install_remote_cargo_link(root.path(), &remote, true).unwrap();
    fs::remove_file(root.path().join("cargo")).unwrap();
    symlink(root.path().join("old/bunkerbox-remote"), root.path().join("cargo")).unwrap();
    install_remote_cargo_link(root.path(), &remote, true).unwrap();

    assert_eq!(fs::read_link(root.path().join("cargo")).unwrap(), remote);
}
