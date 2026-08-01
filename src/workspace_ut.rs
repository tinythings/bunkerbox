use super::*;
use std::os::unix::fs::symlink;
use tempfile::TempDir;

fn workspace() -> TempDir {
    TempDir::new().unwrap()
}

fn directory(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn resolve(root: &Path, cwd: &str) -> WorkspaceCwd {
    WorkspaceCwd::resolve(root, Path::new(cwd)).unwrap()
}

#[test]
fn resolves_workspace_root() {
    let root = workspace();
    let cwd = resolve(root.path(), "/workspace");

    assert_eq!(cwd.host_path(), root.path().canonicalize().unwrap());
    assert_eq!(cwd.relative_path(), Path::new(""));
    assert_eq!(cwd.guest_path(), Path::new("/workspace"));
}

#[test]
fn resolves_nested_directory() {
    let root = workspace();
    let nested = directory(root.path(), "src/lib");
    let cwd = resolve(root.path(), "/workspace/src/lib");

    assert_eq!(cwd.host_path(), nested.canonicalize().unwrap());
    assert_eq!(cwd.relative_path(), Path::new("src/lib"));
    assert_eq!(cwd.guest_path(), Path::new("/workspace/src/lib"));
}

#[test]
fn rejects_parent_directory_component() {
    let root = workspace();
    directory(root.path(), "bar");

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("/workspace/foo/../bar")).is_err());
}

#[test]
fn normalizes_repeated_separators() {
    let root = workspace();
    directory(root.path(), "foo");
    let cwd = resolve(root.path(), "/workspace//foo");

    assert_eq!(cwd.relative_path(), Path::new("foo"));
    assert_eq!(cwd.guest_path(), Path::new("/workspace/foo"));
}

#[test]
fn normalizes_current_directory_components() {
    let root = workspace();
    directory(root.path(), "foo");
    let cwd = resolve(root.path(), "/workspace/./foo");

    assert_eq!(cwd.relative_path(), Path::new("foo"));
    assert_eq!(cwd.guest_path(), Path::new("/workspace/foo"));
}

#[test]
fn rejects_workspace_string_prefix_sibling() {
    let root = workspace();

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("/workspace-other/foo")).is_err());
}

#[test]
fn rejects_unrelated_absolute_path() {
    let root = workspace();

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("/tmp/foo")).is_err());
}

#[test]
fn rejects_relative_path() {
    let root = workspace();

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("foo")).is_err());
}

#[test]
fn rejects_missing_path() {
    let root = workspace();

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("/workspace/missing")).is_err());
}

#[test]
fn rejects_file_as_working_directory() {
    let root = workspace();
    std::fs::write(root.path().join("file"), b"data").unwrap();

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("/workspace/file")).is_err());
}

#[test]
fn rejects_symlink_outside_workspace() {
    let root = workspace();
    let outside = workspace();
    symlink(outside.path(), root.path().join("outside-link")).unwrap();

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("/workspace/outside-link")).is_err());
}

#[test]
fn accepts_inside_symlink_and_preserves_guest_path() {
    let root = workspace();
    let target = directory(root.path(), "real/foo");
    symlink(&target, root.path().join("foo-link")).unwrap();

    let cwd = resolve(root.path(), "/workspace/foo-link");

    assert_eq!(cwd.host_path(), target.canonicalize().unwrap());
    assert_eq!(cwd.relative_path(), Path::new("foo-link"));
    assert_eq!(cwd.guest_path(), Path::new("/workspace/foo-link"));
}

#[test]
fn rejects_dangling_symlink() {
    let root = workspace();
    symlink(root.path().join("missing"), root.path().join("dangling-link")).unwrap();

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("/workspace/dangling-link")).is_err());
}

#[test]
fn rejects_symlink_to_common_prefix_sibling() {
    let parent = workspace();
    let root = parent.path().join("project");
    let sibling = parent.path().join("project-other");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    symlink(&sibling, root.join("sibling-link")).unwrap();

    assert!(WorkspaceCwd::resolve(&root, Path::new("/workspace/sibling-link")).is_err());
}

#[test]
fn accepts_workspace_root_symlink() {
    let parent = workspace();
    let target = workspace();
    let link = parent.path().join("workspace-link");
    symlink(target.path(), &link).unwrap();

    let cwd = resolve(&link, "/workspace");

    assert_eq!(cwd.host_path(), target.path().canonicalize().unwrap());
    assert_eq!(cwd.relative_path(), Path::new(""));
}

#[test]
fn rejects_symlink_loop() {
    let root = workspace();
    symlink(root.path().join("loop-b"), root.path().join("loop-a")).unwrap();
    symlink(root.path().join("loop-a"), root.path().join("loop-b")).unwrap();

    assert!(WorkspaceCwd::resolve(root.path(), Path::new("/workspace/loop-a")).is_err());
}
