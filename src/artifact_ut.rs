use super::*;
use std::fs;
use std::io::Read;
use tempfile::tempdir;

fn limits() -> ArtifactLimits {
    ArtifactLimits::new(Duration::from_secs(1), 4, 1024, 2048).unwrap()
}

#[test]
fn policy_rejects_unsafe_and_duplicate_paths() {
    for paths in [
        vec!["/absolute/file".to_string()],
        vec!["../escape".to_string()],
        vec!["dir/./file".to_string()],
        vec!["dir/*".to_string()],
        vec!["same".to_string(), "same".to_string()],
    ] {
        assert!(ArtifactPolicy::new(paths).is_err());
    }
}

#[test]
fn local_spool_is_manifested_and_published_without_buffering_the_file() {
    let temp = tempdir().unwrap();
    let job = temp.path().join("job");
    let workspace = temp.path().join("workspace");
    let jobs = temp.path().join("jobs");
    fs::create_dir_all(job.join("out")).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir(&jobs).unwrap();
    fs::write(job.join("out/result.bin"), b"artifact bytes").unwrap();

    let policy = ArtifactPolicy::new(vec!["out/result.bin".to_string()]).unwrap();
    let spool = LocalArtifactSpool::capture(&job, &jobs, &policy, limits()).unwrap();
    let manifest = spool.manifest().clone();
    assert_eq!(manifest.total_bytes(), 14);
    assert_eq!(manifest.entries()[0].path(), "out/result.bin");

    let mut publication = ArtifactPublication::new(&workspace, [1; 16], manifest).unwrap();
    let mut source = spool.open_entry(0).unwrap();
    publication.copy_from_reader(0, &mut source).unwrap();
    publication.publish().unwrap();

    let mut actual = Vec::new();
    fs::File::open(workspace.join(".bunkerbox/artifacts/01010101010101010101010101010101/out/result.bin")).unwrap().read_to_end(&mut actual).unwrap();
    assert_eq!(actual, b"artifact bytes");
}

#[test]
fn publication_rejects_collisions_and_cleans_partial_staging() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let policy = ArtifactPolicy::new(vec!["result".to_string()]).unwrap();
    let entry = ArtifactEntry::new("result", 0o644, 1, sha256(b"x")).unwrap();
    let manifest = ArtifactManifest::new(vec![entry], 1, &policy, limits()).unwrap();

    let publication = ArtifactPublication::new(&workspace, [2; 16], manifest.clone()).unwrap();
    drop(publication);
    assert!(!workspace.join(".bunkerbox/artifacts/.staging/02020202020202020202020202020202").exists());

    let mut publication = ArtifactPublication::new(&workspace, [2; 16], manifest).unwrap();
    let mut writer = publication.begin(0).unwrap();
    writer.write_chunk(b"x").unwrap();
    publication.complete(writer).unwrap();
    publication.publish().unwrap();
    assert!(ArtifactPublication::new(
        &workspace,
        [2; 16],
        ArtifactManifest::new(vec![ArtifactEntry::new("result", 0o644, 1, sha256(b"x")).unwrap()], 1, &policy, limits(),).unwrap(),
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn publication_stays_anchored_when_workspace_component_is_swapped() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    let original_bunkerbox = temp.path().join("original-bunkerbox");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&outside).unwrap();
    let policy = ArtifactPolicy::new(vec!["nested/result".to_string()]).unwrap();
    let entry = ArtifactEntry::new("nested/result", 0o644, 1, sha256(b"x")).unwrap();
    let manifest = ArtifactManifest::new(vec![entry], 1, &policy, limits()).unwrap();

    let mut publication = ArtifactPublication::new(&workspace, [3; 16], manifest).unwrap();
    fs::rename(workspace.join(ARTIFACT_ROOT), &original_bunkerbox).unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join(ARTIFACT_ROOT)).unwrap();

    let mut writer = publication.begin(0).unwrap();
    writer.write_chunk(b"x").unwrap();
    publication.complete(writer).unwrap();
    publication.publish().unwrap();

    let request = "03030303030303030303030303030303";
    assert_eq!(fs::read(original_bunkerbox.join(format!("artifacts/{request}/nested/result"))).unwrap(), b"x");
    assert!(!outside.join(format!("artifacts/{request}")).exists());
}

#[cfg(unix)]
#[test]
fn local_capture_rejects_symlink_outputs() {
    let temp = tempdir().unwrap();
    let job = temp.path().join("job");
    let jobs = temp.path().join("jobs");
    fs::create_dir(&job).unwrap();
    fs::create_dir(&jobs).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", job.join("result")).unwrap();
    let policy = ArtifactPolicy::new(vec!["result".to_string()]).unwrap();
    assert!(LocalArtifactSpool::capture(&job, &jobs, &policy, limits()).is_err());
}

fn sha256(value: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(value).into()
}
