use super::*;
use crate::platform;
use crate::process::JobWorkspace;
use bunkerbox_worker_protocol::{WorkerEntryKind, WorkerRelativePath, WorkerSessionId, WorkerUploadEntry, WorkerUploadId};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use tempfile::{tempdir, TempDir};

const SESSION: WorkerSessionId = WorkerSessionId([1; 16]);
const UPLOAD: WorkerUploadId = WorkerUploadId([2; 16]);

fn store_fixture() -> (TempDir, UploadStore) {
    let temp = tempdir().unwrap();
    let root_path = temp.path().join("root");
    fs::create_dir(&root_path).unwrap();
    fs::set_permissions(&root_path, fs::Permissions::from_mode(0o700)).unwrap();
    let root = platform::open_root(&root_path).unwrap();
    let store = UploadStore::new(&root).unwrap();
    (temp, store)
}

fn entries(contents: &[u8]) -> Vec<WorkerUploadEntry> {
    vec![
        WorkerUploadEntry::directory("src", 0o755).unwrap(),
        WorkerUploadEntry::file("src/input", 0o644, contents.len() as u64, Sha256::digest(contents).into()).unwrap(),
    ]
}

#[test]
fn completed_upload_is_reopened_and_materialized_by_opaque_identity() {
    let (_temp, store) = store_fixture();
    let contents = b"stored";
    let mut transaction = store.begin(SESSION, UPLOAD, entries(contents)).unwrap();
    transaction.accept_chunk(&WorkerRelativePath::new("src/input").unwrap(), 0, contents).unwrap();
    transaction.commit().unwrap();
    drop(transaction);

    let stored = store.open_completed(SESSION, UPLOAD).unwrap();
    let jobs = store.jobs_directory().unwrap();
    let job = JobWorkspace::create(&jobs).unwrap();
    stored.materialize(job.root()).unwrap();
    let file = open_relative_file(job.root(), "src/input").unwrap();
    let mut actual = Vec::new();
    (&file).read_to_end(&mut actual).unwrap();
    assert_eq!(actual, contents);
    assert!(store.open_completed(WorkerSessionId([9; 16]), UPLOAD).is_err());
}

#[test]
fn incomplete_and_digest_failed_uploads_are_removed_and_token_cannot_be_reused_while_active() {
    let (_temp, store) = store_fixture();
    let contents = b"stored";
    {
        let mut transaction = store.begin(SESSION, UPLOAD, entries(contents)).unwrap();
        transaction.accept_chunk(&WorkerRelativePath::new("src/input").unwrap(), 1, contents).unwrap_err();
    }
    let mut transaction = store.begin(SESSION, UPLOAD, entries(contents)).unwrap();
    transaction.accept_chunk(&WorkerRelativePath::new("src/input").unwrap(), 0, b"wrong!").unwrap();
    assert!(store.begin(SESSION, UPLOAD, entries(contents)).is_err());
    assert!(transaction.commit().is_err());
    drop(transaction);
    assert!(store.begin(SESSION, UPLOAD, entries(contents)).is_ok());
}

#[test]
fn manifest_requires_declared_parent_directories_and_cleanup_is_token_scoped() {
    let (_temp, store) = store_fixture();
    let data = b"x";
    let missing_parent = vec![WorkerUploadEntry::file("missing/file", 0o644, 1, Sha256::digest(data).into()).unwrap()];
    assert!(store.begin(SESSION, UPLOAD, missing_parent).is_err());

    let mut transaction = store.begin(SESSION, UPLOAD, entries(data)).unwrap();
    transaction.accept_chunk(&WorkerRelativePath::new("src/input").unwrap(), 0, data).unwrap();
    transaction.commit().unwrap();
    drop(transaction);
    store.cleanup(SESSION, UPLOAD).unwrap();
    assert!(store.open_completed(SESSION, UPLOAD).is_err());
}

#[test]
fn replaced_stored_file_symlink_is_rejected_during_materialization() {
    let (temp, store) = store_fixture();
    let data = b"x";
    let mut transaction = store.begin(SESSION, UPLOAD, entries(data)).unwrap();
    transaction.accept_chunk(&WorkerRelativePath::new("src/input").unwrap(), 0, data).unwrap();
    transaction.commit().unwrap();
    drop(transaction);
    let files =
        temp.path().join("root/.bunkerbox-worker/sessions/01010101010101010101010101010101/uploads/02020202020202020202020202020202/files/src/input");
    fs::remove_file(&files).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/passwd", &files).unwrap();
    let stored = store.open_completed(SESSION, UPLOAD).unwrap();
    let jobs = store.jobs_directory().unwrap();
    let job = JobWorkspace::create(&jobs).unwrap();
    assert!(stored.materialize(job.root()).is_err());
}

#[test]
fn unsupported_entry_kind_is_not_accepted_by_the_manifest_constructor() {
    assert!(WorkerUploadEntry::new("node", WorkerEntryKind::Directory, 0o755, 1, None).is_err());
}
