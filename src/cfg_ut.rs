use super::*;
use std::fs;
use tempfile::TempDir;

fn mkfile(root: &Path, name: &str, size: u64) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, vec![0u8; size as usize]).unwrap();
}

fn write_project_conf(root: &Path, yaml: &str) {
    let dir = root.join(".bunkerbox");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("project.conf"), yaml).unwrap();
}

/// ProjectConfig is default-constructed when no config file exists.
#[test]
fn load_or_create_when_no_config_creates_default() {
    let root = TempDir::new().unwrap();
    let cfg = ProjectConfig::load_or_create(root.path()).unwrap();
    assert_eq!(cfg.project.quota.as_deref(), Some("auto"));
    assert!(cfg.project.exclude.is_empty());
    assert!(cfg.project.passthrough.is_empty());
    assert!(root.path().join(ProjectConfig::PATH).exists());
}

/// The .bunkerbox directory is created automatically.
#[test]
fn load_or_create_creates_bunkerbox_dir() {
    let root = TempDir::new().unwrap();
    ProjectConfig::load_or_create(root.path()).unwrap();
    assert!(root.path().join(".bunkerbox").is_dir());
}

/// An existing project.conf is deserialized correctly.
#[test]
fn load_or_create_loads_existing_config() {
    let root = TempDir::new().unwrap();
    write_project_conf(root.path(), "project:\n  quota: 2G\n  exclude:\n    - build/\n    - logs/\n  passthrough: []\n");
    let cfg = ProjectConfig::load_or_create(root.path()).unwrap();
    assert_eq!(cfg.project.quota.as_deref(), Some("2G"));
    assert_eq!(cfg.project.exclude, vec!["build/", "logs/"]);
}

/// Invalid YAML in project.conf produces an error.
#[test]
fn load_or_create_invalid_yaml_is_error() {
    let root = TempDir::new().unwrap();
    write_project_conf(root.path(), "project: @@@");
    assert!(ProjectConfig::load_or_create(root.path()).is_err());
}

/// Old flat env.conf files are migrated to project.conf automatically.
#[test]
fn migrate_from_flat_env_conf() {
    let root = TempDir::new().unwrap();
    let old = root.path().join(".bunkerbox/env.conf");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    fs::write(&old, "quota: 3G\nexclude:\n  - vendor/\npassthrough:\n  - \"make *\"\n").unwrap();

    let cfg = ProjectConfig::load_or_create(root.path()).unwrap();
    assert_eq!(cfg.project.quota.as_deref(), Some("3G"));
    assert_eq!(cfg.project.exclude, vec!["vendor/"]);
    assert_eq!(cfg.project.passthrough, vec!["make *"]);
    assert!(root.path().join(ProjectConfig::PATH).exists());
    assert!(!old.exists());
}

/// to_yaml produces list entries with dashes, not bracket syntax.
#[test]
fn to_yaml_produces_list_format() {
    let cfg = ProjectConfig {
        project: ProjectSection { quota: Some("auto".into()), exclude: vec!["target/".into(), "build/".into()], passthrough: vec!["cargo *".into()] },
        image: ImageOverrides::default(),
    };
    let y = cfg.to_yaml();
    assert!(y.contains("project:"));
    assert!(y.contains("quota: auto"));
    assert!(y.contains("  exclude:"));
    assert!(y.contains("    - target/"));
    assert!(y.contains("    - build/"));
    assert!(y.contains("  passthrough:"));
    assert!(y.contains("    - \"cargo *\""));
    assert!(!y.contains("[]"));
}

/// Empty exclude and passthrough lists use compact bracket notation.
#[test]
fn to_yaml_empty_lists_use_brackets() {
    let cfg = ProjectConfig::default();
    let y = cfg.to_yaml();
    assert!(y.contains("exclude:\n    []"));
    assert!(y.contains("passthrough:\n    []"));
}

/// Image overrides are included in YAML output when present.
#[test]
fn to_yaml_includes_overrides() {
    let cfg = ProjectConfig {
        image: ImageOverrides { workspace: Some(WorkspaceMode::Direct), session_mb: Some(200), ..Default::default() },
        ..Default::default()
    };
    let y = cfg.to_yaml();
    assert!(y.contains("image:"));
    assert!(y.contains("workspace: direct"));
    assert!(y.contains("session_mb: 200"));
}

/// Explicit quota string is parsed into bytes.
#[test]
fn quota_bytes_explicit_size() {
    let cfg = ProjectConfig { project: ProjectSection { quota: Some("2G".into()), ..Default::default() }, ..Default::default() };
    assert_eq!(cfg.quota_bytes(0, Path::new("."), None).unwrap(), 2 * 1024 * 1024 * 1024);
}

/// Megabyte suffix is parsed correctly.
#[test]
fn quota_bytes_explicit_megabytes() {
    let cfg = ProjectConfig { project: ProjectSection { quota: Some("500M".into()), ..Default::default() }, ..Default::default() };
    assert_eq!(cfg.quota_bytes(0, Path::new("."), None).unwrap(), 500 * 1024 * 1024);
}

/// Kilobyte suffix is parsed correctly.
#[test]
fn quota_bytes_explicit_kilobytes() {
    let cfg = ProjectConfig { project: ProjectSection { quota: Some("50K".into()), ..Default::default() }, ..Default::default() };
    assert_eq!(cfg.quota_bytes(0, Path::new("."), None).unwrap(), 50 * 1024);
}

/// Numeric string with no suffix is interpreted as bytes.
#[test]
fn quota_bytes_explicit_bytes_no_suffix() {
    let cfg = ProjectConfig { project: ProjectSection { quota: Some("1048576".into()), ..Default::default() }, ..Default::default() };
    assert_eq!(cfg.quota_bytes(0, Path::new("."), None).unwrap(), 1048576);
}

/// An unparseable quota string produces an error.
#[test]
fn quota_bytes_invalid_size_is_error() {
    let cfg = ProjectConfig { project: ProjectSection { quota: Some("not-a-size".into()), ..Default::default() }, ..Default::default() };
    assert!(cfg.quota_bytes(0, Path::new("."), None).is_err());
}

/// A None quota falls back to auto-computed value (floor at MIN_QUOTA).
#[test]
fn quota_bytes_none_uses_auto() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/lib.rs", 1000);
    let cfg = ProjectConfig { project: ProjectSection { quota: None, ..Default::default() }, ..Default::default() };
    assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
}

/// Auto quota walks the repo and applies floor.
#[test]
fn quota_bytes_auto_computes_from_repo() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/lib.rs", 1000);
    mkfile(root.path(), "README.md", 2000);
    let cfg = ProjectConfig { project: ProjectSection { quota: Some("auto".into()), ..Default::default() }, ..Default::default() };
    assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
}

/// Small repos get the minimum quota floor.
#[test]
fn quota_bytes_auto_respects_min_quota() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "README.md", 10);
    let cfg = ProjectConfig { project: ProjectSection { quota: Some("auto".into()), ..Default::default() }, ..Default::default() };
    assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
}

/// Default exclude patterns skip build output directories during size walk.
#[test]
fn quota_bytes_auto_excludes_default_dirs() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/main.rs", 1_000_000_000);
    mkfile(root.path(), "target/debug/build", 500_000);
    let cfg = ProjectConfig { project: ProjectSection { quota: Some("auto".into()), ..Default::default() }, ..Default::default() };
    assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
}

/// Extra user-specified exclude patterns are honoured during size walk.
#[test]
fn quota_bytes_with_extra_exclude_patterns() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/main.rs", 1_500_000_000);
    mkfile(root.path(), "docs/big.md", 1_500_000_000);
    let cfg = ProjectConfig {
        project: ProjectSection { quota: Some("auto".into()), exclude: vec!["docs/".into()], ..Default::default() },
        ..Default::default()
    };
    assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
}

/// Effective exclude list always includes default patterns.
#[test]
fn effective_exclude_includes_defaults() {
    let cfg = ProjectConfig::default();
    let exclude = cfg.effective_exclude(None);
    assert!(exclude.iter().any(|e| e == "target/"));
    assert!(exclude.iter().any(|e| e == "node_modules/"));
}

/// User-specified excludes are added to the effective list.
#[test]
fn effective_exclude_adds_config_patterns() {
    let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["vendor/".into()], ..Default::default() }, ..Default::default() };
    let exclude = cfg.effective_exclude(None);
    assert!(exclude.contains(&"vendor/".to_string()));
}

/// Duplicate exclude entries between defaults and user config are folded.
#[test]
fn effective_exclude_dedup_with_defaults() {
    let cfg =
        ProjectConfig { project: ProjectSection { exclude: vec!["target/".into(), "build".into()], ..Default::default() }, ..Default::default() };
    let exclude = cfg.effective_exclude(None);
    let target_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "target").count();
    let build_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "build").count();
    assert_eq!(target_count, 1);
    assert_eq!(build_count, 1);
}

/// Trailing-slash variants of the same directory are deduplicated.
#[test]
fn effective_exclude_dedup_with_trailing_slash_variants() {
    let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["dist".into()], ..Default::default() }, ..Default::default() };
    let exclude = cfg.effective_exclude(None);
    let dist_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "dist").count();
    assert_eq!(dist_count, 1);
}

/// Runtime-specific exclude patterns are merged in.
#[test]
fn effective_exclude_adds_runtime_patterns() {
    let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["vendor/".into()], ..Default::default() }, ..Default::default() };
    let runtime = vec!["extra/".to_string()];
    let exclude = cfg.effective_exclude(Some(&runtime));
    assert!(exclude.contains(&"extra/".to_string()));
    assert!(exclude.contains(&"vendor/".to_string()));
}

/// Runtime excludes are deduplicated against config excludes.
#[test]
fn effective_exclude_dedup_runtime_against_config() {
    let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["mydir/".into()], ..Default::default() }, ..Default::default() };
    let runtime = vec!["mydir/".to_string()];
    let exclude = cfg.effective_exclude(Some(&runtime));
    let mydir_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "mydir").count();
    assert_eq!(mydir_count, 1);
}

/// Missing trailing slash in user config is treated the same as with slash.
#[test]
fn effective_exclude_handles_no_trailing_slash_config() {
    let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["folder".into()], ..Default::default() }, ..Default::default() };
    let runtime = vec!["folder/".to_string()];
    let exclude = cfg.effective_exclude(Some(&runtime));
    let folder_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "folder").count();
    assert_eq!(folder_count, 1);
}

/// Even with no user-specified excludes, defaults are present.
#[test]
fn effective_exclude_empty_when_none() {
    let cfg = ProjectConfig::default();
    let exclude = cfg.effective_exclude(None);
    assert!(!exclude.is_empty());
}

/// Well-known internal directories are always excluded.
#[test]
fn is_excluded_hardcoded_names() {
    let exclude: Vec<String> = vec![];
    assert!(is_excluded(".bunker", &exclude));
    assert!(is_excluded(".bunkerbox", &exclude));
    assert!(is_excluded(".git", &exclude));
}

/// A name matching an exclude pattern is excluded.
#[test]
fn is_excluded_by_pattern() {
    let exclude = vec!["target/".to_string()];
    assert!(is_excluded("target", &exclude));
}

/// A name not matching any pattern passes through.
#[test]
fn is_excluded_not_in_patterns() {
    let exclude = vec!["target/".to_string()];
    assert!(!is_excluded("src", &exclude));
}

/// An empty exclude list does not filter anything beyond hardcoded names.
#[test]
fn is_excluded_empty_patterns() {
    let exclude: Vec<String> = vec![];
    assert!(!is_excluded("src", &exclude));
}

/// Trailing slashes are stripped before comparison.
#[test]
fn is_excluded_trailing_slash_stripped() {
    let exclude = vec!["target/".to_string()];
    assert!(is_excluded("target", &exclude));
}

/// File sizes are summed correctly.
#[test]
fn walk_repo_size_counts_files() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "a.txt", 600);
    let exclude: Vec<String> = vec![];
    assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 600);
}

/// An empty directory contributes zero bytes.
#[test]
fn walk_repo_size_handles_empty_dir() {
    let root = TempDir::new().unwrap();
    let exclude: Vec<String> = vec![];
    assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 0);
}

/// Files inside excluded directories are not counted.
#[test]
fn walk_repo_size_skips_excluded_dirs() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/main.rs", 1000);
    mkfile(root.path(), "target/release/binary", 500000);
    let exclude = vec!["target/".to_string()];
    assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 1000);
}

/// .git and .bunkerbox directories are always skipped.
#[test]
fn walk_repo_size_skips_git_and_bunkerbox() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/lib.rs", 500);
    mkfile(root.path(), ".git/config", 200);
    mkfile(root.path(), ".bunkerbox/upper.img", 100000);
    let exclude: Vec<String> = vec![];
    assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 500);
}

/// Symlinks are not followed during the size walk.
#[test]
fn walk_repo_size_handles_symlinks() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/lib.rs", 42);
    let exclude: Vec<String> = vec![];
    assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 42);
}

/// An empty repo gets the minimum quota floor.
#[test]
fn compute_auto_quota_empty_repo_is_floor() {
    let root = TempDir::new().unwrap();
    let exclude: Vec<String> = vec![];
    assert_eq!(compute_auto_quota(root.path(), &exclude).unwrap(), MIN_QUOTA);
}

/// Auto quota applies +10% overhead to the raw size, then floors at MIN_QUOTA.
#[test]
fn compute_auto_quota_applies_10_percent_overhead() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/lib.rs", 1_000_000_000);
    mkfile(root.path(), "README.md", 1_000_000_000);
    let exclude: Vec<String> = vec![];
    assert_eq!(compute_auto_quota(root.path(), &exclude).unwrap(), MIN_QUOTA);
}

/// Excluded directories are not included in auto-quota computation.
#[test]
fn compute_auto_quota_excludes_matching_patterns() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/lib.rs", 1_000_000_000);
    mkfile(root.path(), "vendor/big.bin", 5_000_000_000);
    let exclude = vec!["vendor/".to_string()];
    assert_eq!(compute_auto_quota(root.path(), &exclude).unwrap(), MIN_QUOTA);
}

/// Computed quota below floor is clamped to MIN_QUOTA.
#[test]
fn compute_auto_quota_above_floor() {
    let root = TempDir::new().unwrap();
    mkfile(root.path(), "src/big.rs", 2_000_000_000);
    let exclude: Vec<String> = vec![];
    assert_eq!(compute_auto_quota(root.path(), &exclude).unwrap(), MIN_QUOTA);
}

/// Gigabyte suffix is parsed correctly.
#[test]
fn parse_size_parses_gigabytes() {
    assert_eq!(parse_size("10G"), Some(10 * 1024 * 1024 * 1024));
}

/// Megabyte suffix is parsed correctly.
#[test]
fn parse_size_parses_megabytes() {
    assert_eq!(parse_size("500M"), Some(500 * 1024 * 1024));
}

/// Unrecognised suffix returns None.
#[test]
fn parse_size_returns_none_on_invalid() {
    assert_eq!(parse_size("xyz"), None);
}
