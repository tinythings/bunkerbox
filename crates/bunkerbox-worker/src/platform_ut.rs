use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn root_requires_private_owned_directory_without_following_final_symlink() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(open_root(&root).is_ok());

    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(open_root(&root).is_err());

    let link = temp.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&root, &link).unwrap();
    assert!(open_root(Path::new(&link)).is_err());
}

#[test]
fn descriptor_relative_tree_removal_does_not_follow_symlinks() {
    let temp = tempdir().unwrap();
    let root_path = temp.path().join("root");
    let outside = temp.path().join("outside");
    fs::create_dir(&root_path).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::set_permissions(&root_path, fs::Permissions::from_mode(0o700)).unwrap();
    let root = open_root(&root_path).unwrap();
    create_dir_at(&root, "state", 0o700).unwrap();
    fs::write(outside.join("outside.txt"), b"outside").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root_path.join("state/link")).unwrap();
    remove_tree_at(&root, "state").unwrap();
    assert!(outside.join("outside.txt").exists());
}
