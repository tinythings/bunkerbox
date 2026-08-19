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

fn limited_store(limits: WorkerStateLimits) -> (TempDir, UploadStore) {
    let temp = tempdir().unwrap();
    let root_path = temp.path().join("root");
    fs::create_dir(&root_path).unwrap();
    fs::set_permissions(&root_path, fs::Permissions::from_mode(0o700)).unwrap();
    let root = platform::open_root(&root_path).unwrap();
    let store = UploadStore::new_with_limits(&root, limits).unwrap();
    (temp, store)
}

fn entries(contents: &[u8]) -> Vec<WorkerUploadEntry> {
    vec![
        WorkerUploadEntry::directory("src", 0o755).unwrap(),
        WorkerUploadEntry::file("src/input", 0o644, contents.len() as u64, Sha256::digest(contents).into()).unwrap(),
    ]
}

fn symlink_entries() -> Vec<WorkerUploadEntry> {
    vec![
        WorkerUploadEntry::directory("src", 0o755).unwrap(),
        WorkerUploadEntry::symlink("src/link", 0o777, "../target").unwrap(),
        WorkerUploadEntry::directory("target", 0o755).unwrap(),
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
fn completed_upload_recreates_relative_symlinks_without_file_contents() {
    let (_temp, store) = store_fixture();
    let mut transaction = store.begin(SESSION, UPLOAD, symlink_entries()).unwrap();
    transaction.commit().unwrap();
    drop(transaction);

    let stored = store.open_completed(SESSION, UPLOAD).unwrap();
    let jobs = store.jobs_directory().unwrap();
    let job = JobWorkspace::create(&jobs).unwrap();
    stored.materialize(job.root()).unwrap();

    let link = open_relative_file(job.root(), "src/link");
    assert!(link.is_err());
    let src = platform::open_dir_at(job.root(), "src").unwrap();
    let metadata = platform::stat_at(&src, "link").unwrap();
    assert_eq!(metadata.st_mode & libc::S_IFMT, libc::S_IFLNK);
}

#[test]
fn worker_materialization_rejects_replaced_symlink_parent() {
    let (temp, store) = store_fixture();
    let mut transaction = store.begin(SESSION, UPLOAD, symlink_entries()).unwrap();
    transaction.commit().unwrap();
    drop(transaction);

    let stored = store.open_completed(SESSION, UPLOAD).unwrap();
    let jobs = store.jobs_directory().unwrap();
    let job = JobWorkspace::create(&jobs).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    platform::create_symlink_at(job.root(), "src", outside.to_str().unwrap()).unwrap();

    assert!(stored.materialize(job.root()).is_err());
    assert!(!outside.join("link").exists());
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

#[test]
fn live_upload_reservation_enforces_count_and_releases_on_drop() {
    let limits = WorkerStateLimits { max_uploads: 1, ..WorkerStateLimits::default() };
    let (_temp, store) = limited_store(limits);
    let contents = b"stored";
    let transaction = store.begin(SESSION, UPLOAD, entries(contents)).unwrap();
    assert!(store.begin(WorkerSessionId([3; 16]), WorkerUploadId([4; 16]), entries(contents)).is_err());
    drop(transaction);
    assert!(store.begin(WorkerSessionId([3; 16]), WorkerUploadId([4; 16]), entries(contents)).is_ok());
}

#[test]
fn live_job_reservation_enforces_bytes_and_releases_on_drop() {
    let limits = WorkerStateLimits { max_jobs: 1, max_job_bytes: 5, ..WorkerStateLimits::default() };
    let (_temp, store) = limited_store(limits);
    let reservation = store.reserve_job(5, 1).unwrap();
    assert!(store.reserve_job(1, 1).is_err());
    drop(reservation);
    assert!(store.reserve_job(5, 1).is_ok());
}
