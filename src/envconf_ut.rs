use super::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn config_yaml(quota: &str, exclude: &[&str]) -> String {
    let mut yaml = format!(
        "# test config\n\
         quota: {quota}\n\
         exclude:\n"
    );
    for pat in exclude {
        yaml.push_str(&format!("  - {pat}\n"));
    }
    yaml
}

fn write_env_conf(root: &Path, yaml: &str) {
    let dir = root.join(".bunkerbox");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("env.conf"), yaml).unwrap();
}

fn mkfile(root: &Path, rel: &str, size: u64) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, vec![0u8; size as usize]).unwrap();
}

#[test]
fn load_or_create_when_no_config_creates_default() {
    let root = TempDir::new().unwrap();
    let cfg = EnvConfig::load_or_create(root.path()).unwrap();

    assert_eq!(cfg.quota.as_deref(), Some("auto"));
    assert!(cfg.exclude.is_empty());

    let path = root.path().join(".bunkerbox/env.conf");
    assert!(path.is_file());

    let contents = fs::read_to_string(&path).unwrap();
    assert!(contents.contains("quota: auto"));
    assert!(contents.contains("target/"));
    assert!(contents.contains("node_modules/"));
}

#[test]
fn load_or_create_loads_existing_config() {
    let root = TempDir::new().unwrap();
    let yaml = config_yaml("2G", &["build/", "logs/"]);
    write_env_conf(root.path(), &yaml);

    let cfg = EnvConfig::load_or_create(root.path()).unwrap();
    assert_eq!(cfg.quota.as_deref(), Some("2G"));
    assert_eq!(cfg.exclude, vec!["build/", "logs/"]);
}

#[test]
fn load_or_create_invalid_yaml_is_error() {
    let root = TempDir::new().unwrap();
    write_env_conf(root.path(), "quota: [1, 2, 3]\n");

    let err = EnvConfig::load_or_create(root.path()).unwrap_err();
    assert!(err.contains("failed to parse"));
}

#[test]
fn load_or_create_creates_bunkerbox_dir() {
    let root = TempDir::new().unwrap();
    EnvConfig::load_or_create(root.path()).unwrap();

    let dir = root.path().join(".bunkerbox");
    assert!(dir.is_dir());
    assert!(dir.join("env.conf").is_file());
}

#[test]
fn quota_bytes_auto_computes_from_repo() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/main.rs", 1000);
    mkfile(root.path(), "src/lib.rs", 2000);
    mkfile(root.path(), "target/debug/build", 500_000); // excluded

    let cfg = EnvConfig { quota: Some("auto".into()), exclude: vec![] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    // 1000 + 2000 = 3000 * 1.1 = 3300, floor is 1G => 1G
    assert_eq!(bytes, 1024 * 1024 * 1024);
}

#[test]
fn quota_bytes_auto_respects_min_quota() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "small.txt", 10);

    let cfg = EnvConfig { quota: Some("auto".into()), exclude: vec![] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    // 10 * 1.1 = 11, floor is 1G
    assert_eq!(bytes, 1024 * 1024 * 1024);
}

#[test]
fn quota_bytes_auto_excludes_default_dirs() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/main.rs", 1_000_000_000);
    mkfile(root.path(), "target/debug/huge.o", 100_000_000); // DEFAULT_EXCLUDE

    let cfg = EnvConfig { quota: Some("auto".into()), exclude: vec![] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    // Only src/main.rs counts: 1_000_000_000 * 1.1 = 1_100_000_000, > 1G floor
    assert_eq!(bytes, 1_100_000_000);
}

#[test]
fn quota_bytes_none_uses_auto() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "data.bin", 100_000_000);

    let cfg = EnvConfig { quota: None, exclude: vec![] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    // 100_000_000 * 1.1 = 110_000_000, floor 1G → 1G
    assert_eq!(bytes, 1024 * 1024 * 1024);
}

#[test]
fn quota_bytes_explicit_size() {
    let root = TempDir::new().unwrap();
    // 2G = 2 * 1024^3 = 2147483648
    let cfg = EnvConfig { quota: Some("2G".into()), exclude: vec![] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    assert_eq!(bytes, 2 * 1024 * 1024 * 1024);
}

#[test]
fn quota_bytes_explicit_megabytes() {
    let root = TempDir::new().unwrap();
    let cfg = EnvConfig { quota: Some("500M".into()), exclude: vec![] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    assert_eq!(bytes, 500 * 1024 * 1024);
}

#[test]
fn quota_bytes_explicit_kilobytes() {
    let root = TempDir::new().unwrap();
    let cfg = EnvConfig { quota: Some("50K".into()), exclude: vec![] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    assert_eq!(bytes, 50 * 1024);
}

#[test]
fn quota_bytes_explicit_bytes_no_suffix() {
    let root = TempDir::new().unwrap();
    let cfg = EnvConfig { quota: Some("1048576".into()), exclude: vec![] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    assert_eq!(bytes, 1048576);
}

#[test]
fn quota_bytes_invalid_size_is_error() {
    let root = TempDir::new().unwrap();
    let cfg = EnvConfig { quota: Some("not-a-size".into()), exclude: vec![] };
    let err = cfg.quota_bytes(0, root.path(), None).unwrap_err();
    assert!(err.contains("invalid quota"));
}

#[test]
fn quota_bytes_with_extra_exclude_patterns() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/main.rs", 1_500_000_000);
    mkfile(root.path(), "vendor/big.rs", 50_000_000); // excluded via config

    let cfg = EnvConfig { quota: Some("auto".into()), exclude: vec!["vendor/".into()] };
    let bytes = cfg.quota_bytes(0, root.path(), None).unwrap();
    // Only src/main.rs: 1_500_000_000 * 1.1 = 1_650_000_000, > 1G floor
    assert_eq!(bytes, 1_650_000_000);
}

#[test]
fn effective_exclude_includes_defaults() {
    let cfg = EnvConfig { quota: None, exclude: vec![] };
    let exclude = cfg.effective_exclude(None);

    assert!(exclude.contains(&"target/".to_string()));
    assert!(exclude.contains(&"node_modules/".to_string()));
    assert!(exclude.contains(&".venv/".to_string()));
    assert!(exclude.contains(&"build/".to_string()));
}

#[test]
fn effective_exclude_adds_config_patterns() {
    let cfg = EnvConfig { quota: None, exclude: vec!["mybuild/".into(), "logs/".into()] };
    let exclude = cfg.effective_exclude(None);

    assert!(exclude.contains(&"target/".to_string())); // default
    assert!(exclude.contains(&"mybuild/".to_string())); // config
    assert!(exclude.contains(&"logs/".to_string())); // config
}

#[test]
fn effective_exclude_dedup_with_defaults() {
    let cfg = EnvConfig { quota: None, exclude: vec!["target/".into(), "build".into()] };
    let exclude = cfg.effective_exclude(None);

    // Should only have one "target/" and one "build"
    let target_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "target").count();
    let build_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "build").count();
    assert_eq!(target_count, 1, "target/ should not be duplicated");
    assert_eq!(build_count, 1, "build should not be duplicated");
}

#[test]
fn effective_exclude_dedup_with_trailing_slash_variants() {
    let cfg = EnvConfig { quota: None, exclude: vec!["dist/".into()] };
    // "dist/" is already a default with trailing slash.  Should dedup.
    let exclude = cfg.effective_exclude(None);
    let dist_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "dist").count();
    assert_eq!(dist_count, 1);
}

#[test]
fn effective_exclude_adds_runtime_patterns() {
    let cfg = EnvConfig { quota: None, exclude: vec![] };
    let runtime: Vec<String> = vec!["extra/".into(), "tmp/".into()];
    let exclude = cfg.effective_exclude(Some(&runtime));

    assert!(exclude.contains(&"extra/".to_string()));
    assert!(exclude.contains(&"tmp/".to_string()));
}

#[test]
fn effective_exclude_dedup_runtime_against_config() {
    let cfg = EnvConfig { quota: None, exclude: vec!["mydir/".into()] };
    let runtime: Vec<String> = vec!["mydir/".into(), "other/".into()];
    let exclude = cfg.effective_exclude(Some(&runtime));

    let mydir_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "mydir").count();
    assert_eq!(mydir_count, 1);
    assert!(exclude.contains(&"other/".to_string()));
}

#[test]
fn effective_exclude_handles_no_trailing_slash_config() {
    let cfg = EnvConfig { quota: None, exclude: vec!["folder".into()] };
    let runtime: Vec<String> = vec!["folder/".into()]; // same as folder with slash
    let exclude = cfg.effective_exclude(Some(&runtime));

    let folder_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "folder").count();
    assert_eq!(folder_count, 1);
}

#[test]
fn effective_exclude_empty_when_none() {
    let cfg = EnvConfig { quota: None, exclude: vec![] };
    let exclude = cfg.effective_exclude(None);
    // Should still contain defaults
    assert!(!exclude.is_empty());
    assert!(exclude.len() >= DEFAULT_EXCLUDE.len());
}

#[test]
fn compute_auto_quota_applies_10_percent_overhead() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "a.txt", 100_000_000); // 100MB

    let quota = compute_auto_quota(root.path(), &[]).unwrap();
    // 100MB * 1.1 = 110MB, which is < 1G floor → 1G
    assert_eq!(quota, MIN_QUOTA);
}

#[test]
fn compute_auto_quota_above_floor() {
    let root = TempDir::new().unwrap();
    // 2GB of files → 2.2GB quota, above 1G floor
    mkfile(root.path(), "a.bin", 1_000_000_000);
    mkfile(root.path(), "b.bin", 1_000_000_000);

    let quota = compute_auto_quota(root.path(), &[]).unwrap();
    assert_eq!(quota, 2_200_000_000);
}

#[test]
fn compute_auto_quota_excludes_matching_patterns() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/a.txt", 100_000);
    mkfile(root.path(), "build/out.o", 10_000_000); // excluded

    let excluded: Vec<String> = vec!["build".into()];
    let quota = compute_auto_quota(root.path(), &excluded).unwrap();
    // 100_000 * 1.1 = 110_000, floor is 1G
    assert_eq!(quota, MIN_QUOTA);
}

#[test]
fn compute_auto_quota_empty_repo_is_floor() {
    let root = TempDir::new().unwrap();

    let quota = compute_auto_quota(root.path(), &[]).unwrap();
    assert_eq!(quota, MIN_QUOTA);
}

#[test]
fn walk_repo_size_counts_files() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "a.rs", 100);
    mkfile(root.path(), "b.rs", 200);
    mkfile(root.path(), "sub/c.rs", 300);

    let size = walk_repo_size(root.path(), &[]).unwrap();
    assert_eq!(size, 600);
}

#[test]
fn walk_repo_size_skips_excluded_dirs() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/a.txt", 1000);
    mkfile(root.path(), "target/b.o", 5000); // excluded

    let excluded: Vec<String> = vec!["target".into()];
    let size = walk_repo_size(root.path(), &excluded).unwrap();
    assert_eq!(size, 1000);
}

#[test]
fn walk_repo_size_skips_git_and_bunkerbox() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/a.txt", 500);
    mkfile(root.path(), ".git/objects/ab", 9999); // hardcoded skip
    mkfile(root.path(), ".bunkerbox/data", 9999); // hardcoded skip
    mkfile(root.path(), ".bunker/data", 9999); // hardcoded skip

    let size = walk_repo_size(root.path(), &[]).unwrap();
    assert_eq!(size, 500);
}

#[test]
fn walk_repo_size_handles_symlinks() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "real.txt", 42);
    std::os::unix::fs::symlink("real.txt", root.path().join("link.txt")).ok();

    let size = walk_repo_size(root.path(), &[]).unwrap();
    // Should only count the real file
    assert_eq!(size, 42);
}

#[test]
fn walk_repo_size_handles_empty_dir() {
    let root = TempDir::new().unwrap();
    // Just an empty root
    let size = walk_repo_size(root.path(), &[]).unwrap();
    assert_eq!(size, 0);
}

#[test]
fn is_excluded_hardcoded_names() {
    assert!(is_excluded(".bunker", &[]));
    assert!(is_excluded(".bunkerbox", &[]));
    assert!(is_excluded(".git", &[]));
}

#[test]
fn is_excluded_by_pattern() {
    let patterns: Vec<String> = vec!["target".into(), "build".into()];
    assert!(is_excluded("target", &patterns));
    assert!(is_excluded("build", &patterns));
}

#[test]
fn is_excluded_not_in_patterns() {
    let patterns: Vec<String> = vec!["target".into()];
    assert!(!is_excluded("src", &patterns));
    assert!(!is_excluded("lib", &patterns));
}

#[test]
fn is_excluded_trailing_slash_stripped() {
    let patterns: Vec<String> = vec!["target/".into()];
    assert!(is_excluded("target", &patterns));
}

#[test]
fn is_excluded_empty_patterns() {
    assert!(!is_excluded("src", &[]));
}
