use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::vscomm::buildsys;

pub const DEFAULT_SHARE_DIR: &str = "/usr/share/bunkerbox";

const DEFAULT_EXCLUDE: &[&str] = &[
    "target/",
    "node_modules/",
    ".venv/",
    "venv/",
    "build/",
    "__pycache__/",
    "dist/",
    ".next/",
    ".gradle/",
    "cmake-build-debug/",
    "cmake-build-release/",
];

const MIN_QUOTA: u64 = 5 * 1024 * 1024 * 1024;

// ── Enums ──────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    Bridge,
    Host,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceMode {
    #[serde(alias = "share")]
    #[default]
    Cow,
    Direct,
    #[serde(alias = "clone")]
    Isolated,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HomeMode {
    Persist,
    Temporary,
}

// ── Runtime config (shared, immutable, ships in package) ─

#[derive(Debug, Deserialize)]
pub struct RuntimeConfig {
    pub oci: PathBuf,
    pub image: String,
    pub network: Option<NetworkMode>,
    pub allow: Option<Vec<String>>,
    pub workspace: Option<WorkspaceMode>,
    pub workspace_quota: Option<String>,
    pub workspace_exclude: Option<Vec<String>>,
    pub home: Option<HomeMode>,
    pub home_path: Option<PathBuf>,
    #[serde(default)]
    pub encrypt: Option<Vec<String>>,
    pub session_mb: Option<u32>,
}

impl RuntimeConfig {
    pub fn invoked_name() -> Result<String, String> {
        let arg0 = env::args_os().next().ok_or_else(|| "missing argv[0]".to_string())?;
        Path::new(&arg0).file_name().and_then(|n| n.to_str()).map(String::from).ok_or_else(|| "invalid argv[0]".to_string())
    }

    pub fn for_invoked_name(share_dir: &Path) -> Result<Option<Self>, String> {
        let name = Self::invoked_name()?;
        if name == "bunkerbox" {
            return Ok(None);
        }
        let path = share_dir.join(format!("{name}.conf"));
        serde_yaml::from_str(&fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))
            .map(Some)
    }

    pub fn workspace_quota_bytes(&self) -> u64 {
        let raw = self.workspace_quota.as_deref().unwrap_or("10G");
        parse_size(raw).unwrap_or(10 * 1024 * 1024 * 1024)
    }

    pub fn session_mb(&self) -> u32 {
        self.session_mb.unwrap_or(50)
    }

    #[allow(dead_code)]
    pub fn apply_overrides(&self, overrides: &ImageOverrides) -> AppliedRuntime {
        AppliedRuntime {
            workspace: overrides.workspace.or(self.workspace).unwrap_or_default(),
            workspace_quota: self.workspace_quota_bytes(),
            workspace_exclude: self.workspace_exclude.clone().unwrap_or_default(),
            home: overrides.home.or(self.home),
            home_path: overrides.home_path.clone().or_else(|| self.home_path.clone()),
            session_mb: overrides.session_mb.or(self.session_mb).unwrap_or(50),
            network: self.network,
            allow: match (&self.allow, &overrides.allow) {
                (Some(base), Some(extra)) => Some(base.iter().chain(extra.iter()).cloned().collect()),
                (Some(base), None) => Some(base.clone()),
                (None, Some(extra)) => Some(extra.clone()),
                (None, None) => None,
            },
            encrypt: self.encrypt.clone(),
        }
    }
}

#[allow(dead_code)]
pub struct AppliedRuntime {
    pub workspace: WorkspaceMode,
    pub workspace_quota: u64,
    pub workspace_exclude: Vec<String>,
    pub home: Option<HomeMode>,
    pub home_path: Option<PathBuf>,
    pub session_mb: u32,
    pub network: Option<NetworkMode>,
    pub allow: Option<Vec<String>>,
    pub encrypt: Option<Vec<String>>,
}

// ── Project config (.bunkerbox/project.conf) ──────────

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub project: ProjectSection,
    #[serde(default)]
    pub image: ImageOverrides,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ProjectSection {
    #[serde(default)]
    pub quota: Option<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub passthrough: Vec<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ImageOverrides {
    #[serde(default)]
    pub workspace: Option<WorkspaceMode>,
    #[serde(default)]
    pub home: Option<HomeMode>,
    #[serde(default)]
    pub home_path: Option<PathBuf>,
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub session_mb: Option<u32>,
}

impl ProjectConfig {
    pub const PATH: &str = ".bunkerbox/project.conf";

    fn legacy_path() -> &'static Path {
        Path::new(".bunkerbox/env.conf")
    }

    pub fn load_or_create(repo_root: &Path) -> Result<Self, String> {
        let path = repo_root.join(Self::PATH);
        if path.exists() {
            return serde_yaml::from_str(&fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))
                .map(|mut cfg: Self| {
                    cfg.auto_fill_passthrough(repo_root, &path);
                    cfg
                });
        }

        let legacy = repo_root.join(Self::legacy_path());
        if legacy.exists() {
            return Self::migrate_from_env_conf(repo_root, &legacy, &path);
        }

        let cfg = ProjectConfig {
            project: ProjectSection { quota: Some("auto".into()), exclude: Vec::new(), passthrough: buildsys::scan(repo_root) },
            image: ImageOverrides::default(),
        };
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("failed to create {}: {e}", path.parent().unwrap().display()))?;
        fs::write(&path, cfg.to_yaml()).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        Ok(cfg)
    }

    fn migrate_from_env_conf(_repo_root: &Path, legacy_path: &Path, new_path: &Path) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct LegacyEnvConfig {
            #[serde(default)]
            quota: Option<String>,
            #[serde(default)]
            exclude: Vec<String>,
            #[serde(default)]
            passthrough: Vec<String>,
        }

        let old: LegacyEnvConfig =
            serde_yaml::from_str(&fs::read_to_string(legacy_path).map_err(|e| format!("failed to read {}: {e}", legacy_path.display()))?)
                .map_err(|e| format!("failed to parse legacy {}: {e}", legacy_path.display()))?;

        let cfg = ProjectConfig {
            project: ProjectSection { quota: old.quota, exclude: old.exclude, passthrough: old.passthrough },
            image: ImageOverrides::default(),
        };

        fs::write(new_path, cfg.to_yaml()).map_err(|e| format!("failed to write {}: {e}", new_path.display()))?;
        let _ = fs::remove_file(legacy_path);

        Ok(cfg)
    }

    fn auto_fill_passthrough(&mut self, repo_root: &Path, path: &Path) {
        if self.project.passthrough.is_empty() {
            self.project.passthrough = buildsys::scan(repo_root);
            if !self.project.passthrough.is_empty() {
                let _ = fs::write(path, self.to_yaml());
            }
        }
    }

    pub fn quota_bytes(&self, _runtime_default: u64, repo_root: &Path, runtime_exclude: Option<&[String]>) -> Result<u64, String> {
        match self.project.quota.as_deref() {
            None | Some("auto") => compute_auto_quota(repo_root, &self.effective_exclude(runtime_exclude)),
            Some(s) => parse_size(s).ok_or_else(|| format!("invalid quota: {s}")),
        }
    }

    pub fn effective_exclude(&self, runtime_exclude: Option<&[String]>) -> Vec<String> {
        DEFAULT_EXCLUDE
            .iter()
            .map(|s| s.to_string())
            .chain(self.project.exclude.iter().cloned())
            .chain(runtime_exclude.into_iter().flatten().cloned())
            .fold(Vec::new(), |mut acc, pat| {
                let p = pat.trim_end_matches('/');
                if !acc.iter().any(|e| e.trim_end_matches('/') == p) {
                    acc.push(pat);
                }
                acc
            })
    }

    pub fn to_yaml(&self) -> String {
        let mut y = String::new();
        y.push_str("# Bunkerbox project configuration\n");
        y.push_str("# Edit this file to customize behavior.\n\n");

        y.push_str("project:\n");
        Self::yaml_field(&mut y, "  quota", self.project.quota.as_deref().unwrap_or("auto"));
        y.push_str("  exclude:\n");
        if self.project.exclude.is_empty() {
            y.push_str("    []\n");
        } else {
            for pat in &self.project.exclude {
                y.push_str(&format!("    - {pat}\n"));
            }
        }
        y.push_str("  passthrough:\n");
        let pt = &self.project.passthrough;
        if pt.is_empty() {
            y.push_str("    []\n");
        } else {
            for cmd in pt {
                y.push_str(&format!("    - \"{cmd}\"\n"));
            }
        }

        if self.image.has_override() {
            y.push('\n');
            y.push_str("# Override shared runtime defaults:\n");
            y.push_str("image:\n");
            if let Some(ref ws) = self.image.workspace {
                y.push_str(&format!(
                    "  workspace: {}\n",
                    match ws {
                        WorkspaceMode::Cow => "cow",
                        WorkspaceMode::Direct => "direct",
                        WorkspaceMode::Isolated => "isolated",
                    }
                ));
            }
            if let Some(ref hm) = self.image.home {
                y.push_str(&format!(
                    "  home: {}\n",
                    match hm {
                        HomeMode::Persist => "persist",
                        HomeMode::Temporary => "temporary",
                    }
                ));
            }
            if let Some(ref hp) = self.image.home_path {
                y.push_str(&format!("  home_path: {}\n", hp.display()));
            }
            if let Some(ref mb) = self.image.session_mb {
                y.push_str(&format!("  session_mb: {mb}\n"));
            }
            if let Some(ref allow) = self.image.allow {
                y.push_str("  allow:\n");
                for host in allow {
                    y.push_str(&format!("    - {host}\n"));
                }
            }
        }

        y
    }

    fn yaml_field(y: &mut String, key: &str, value: &str) {
        y.push_str(&format!("{key}: {value}\n"));
    }
}

impl ImageOverrides {
    fn has_override(&self) -> bool {
        self.workspace.is_some() || self.home.is_some() || self.home_path.is_some() || self.session_mb.is_some() || self.allow.is_some()
    }
}

// ── Size parsing ───────────────────────────────────────

pub(crate) fn parse_size(raw: &str) -> Option<u64> {
    let raw = raw.trim().to_uppercase();
    let (num_str, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let num: u64 = num_str.parse().ok()?;
    Some(match unit {
        "" | "B" => num,
        "K" | "KB" => num * 1024,
        "M" | "MB" => num * 1024 * 1024,
        "G" | "GB" => num * 1024 * 1024 * 1024,
        _ => return None,
    })
}

// ── Quota computation ──────────────────────────────────

fn compute_auto_quota(repo_root: &Path, exclude: &[String]) -> Result<u64, String> {
    Ok(walk_repo_size(repo_root, exclude)?.saturating_mul(11).saturating_div(10).max(MIN_QUOTA))
}

fn walk_repo_size(dir: &Path, exclude: &[String]) -> Result<u64, String> {
    let mut total: u64 = 0;
    for entry in fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))? {
        let Ok(entry) = entry else {
            continue;
        };
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        if is_excluded(name_str, exclude) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total = total.saturating_add(walk_repo_size(&entry.path(), exclude)?);
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn is_excluded(name: &str, exclude: &[String]) -> bool {
    name == ".bunker" || name == ".bunkerbox" || name == ".git" || exclude.iter().any(|p| name == p.trim_end_matches('/'))
}

// ── Tests ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
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

    // ── ProjectConfig loading ──

    #[test]
    fn load_or_create_when_no_config_creates_default() {
        let root = TempDir::new().unwrap();
        let cfg = ProjectConfig::load_or_create(root.path()).unwrap();
        assert_eq!(cfg.project.quota.as_deref(), Some("auto"));
        assert!(cfg.project.exclude.is_empty());
        assert!(cfg.project.passthrough.is_empty());
        assert!(root.path().join(ProjectConfig::PATH).exists());
    }

    #[test]
    fn load_or_create_creates_bunkerbox_dir() {
        let root = TempDir::new().unwrap();
        ProjectConfig::load_or_create(root.path()).unwrap();
        assert!(root.path().join(".bunkerbox").is_dir());
    }

    #[test]
    fn load_or_create_loads_existing_config() {
        let root = TempDir::new().unwrap();
        write_project_conf(root.path(), "project:\n  quota: 2G\n  exclude:\n    - build/\n    - logs/\n  passthrough: []\n");
        let cfg = ProjectConfig::load_or_create(root.path()).unwrap();
        assert_eq!(cfg.project.quota.as_deref(), Some("2G"));
        assert_eq!(cfg.project.exclude, vec!["build/", "logs/"]);
    }

    #[test]
    fn load_or_create_invalid_yaml_is_error() {
        let root = TempDir::new().unwrap();
        write_project_conf(root.path(), "project: @@@");
        assert!(ProjectConfig::load_or_create(root.path()).is_err());
    }

    // ── Legacy migration ──

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

    // ── YAML output ──

    #[test]
    fn to_yaml_produces_list_format() {
        let cfg = ProjectConfig {
            project: ProjectSection {
                quota: Some("auto".into()),
                exclude: vec!["target/".into(), "build/".into()],
                passthrough: vec!["cargo *".into()],
            },
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

    #[test]
    fn to_yaml_empty_lists_use_brackets() {
        let cfg = ProjectConfig::default();
        let y = cfg.to_yaml();
        assert!(y.contains("exclude:\n    []"));
        assert!(y.contains("passthrough:\n    []"));
    }

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

    // ── Quota ──

    #[test]
    fn quota_bytes_explicit_size() {
        let cfg = ProjectConfig { project: ProjectSection { quota: Some("2G".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(cfg.quota_bytes(0, Path::new("."), None).unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn quota_bytes_explicit_megabytes() {
        let cfg = ProjectConfig { project: ProjectSection { quota: Some("500M".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(cfg.quota_bytes(0, Path::new("."), None).unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn quota_bytes_explicit_kilobytes() {
        let cfg = ProjectConfig { project: ProjectSection { quota: Some("50K".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(cfg.quota_bytes(0, Path::new("."), None).unwrap(), 50 * 1024);
    }

    #[test]
    fn quota_bytes_explicit_bytes_no_suffix() {
        let cfg = ProjectConfig { project: ProjectSection { quota: Some("1048576".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(cfg.quota_bytes(0, Path::new("."), None).unwrap(), 1048576);
    }

    #[test]
    fn quota_bytes_invalid_size_is_error() {
        let cfg = ProjectConfig { project: ProjectSection { quota: Some("not-a-size".into()), ..Default::default() }, ..Default::default() };
        assert!(cfg.quota_bytes(0, Path::new("."), None).is_err());
    }

    #[test]
    fn quota_bytes_none_uses_auto() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/lib.rs", 1000);
        let cfg = ProjectConfig { project: ProjectSection { quota: None, ..Default::default() }, ..Default::default() };
        assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn quota_bytes_auto_computes_from_repo() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/lib.rs", 1000);
        mkfile(root.path(), "README.md", 2000);
        let cfg = ProjectConfig { project: ProjectSection { quota: Some("auto".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn quota_bytes_auto_respects_min_quota() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "README.md", 10);
        let cfg = ProjectConfig { project: ProjectSection { quota: Some("auto".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn quota_bytes_auto_excludes_default_dirs() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/main.rs", 1_000_000_000);
        mkfile(root.path(), "target/debug/build", 500_000);
        let cfg = ProjectConfig { project: ProjectSection { quota: Some("auto".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(cfg.quota_bytes(0, root.path(), None).unwrap(), 5 * 1024 * 1024 * 1024);
    }

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

    // ── Exclude ──

    #[test]
    fn effective_exclude_includes_defaults() {
        let cfg = ProjectConfig::default();
        let exclude = cfg.effective_exclude(None);
        assert!(exclude.iter().any(|e| e == "target/"));
        assert!(exclude.iter().any(|e| e == "node_modules/"));
    }

    #[test]
    fn effective_exclude_adds_config_patterns() {
        let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["vendor/".into()], ..Default::default() }, ..Default::default() };
        let exclude = cfg.effective_exclude(None);
        assert!(exclude.contains(&"vendor/".to_string()));
    }

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

    #[test]
    fn effective_exclude_dedup_with_trailing_slash_variants() {
        let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["dist".into()], ..Default::default() }, ..Default::default() };
        let exclude = cfg.effective_exclude(None);
        let dist_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "dist").count();
        assert_eq!(dist_count, 1);
    }

    #[test]
    fn effective_exclude_adds_runtime_patterns() {
        let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["vendor/".into()], ..Default::default() }, ..Default::default() };
        let runtime = vec!["extra/".to_string()];
        let exclude = cfg.effective_exclude(Some(&runtime));
        assert!(exclude.contains(&"extra/".to_string()));
        assert!(exclude.contains(&"vendor/".to_string()));
    }

    #[test]
    fn effective_exclude_dedup_runtime_against_config() {
        let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["mydir/".into()], ..Default::default() }, ..Default::default() };
        let runtime = vec!["mydir/".to_string()];
        let exclude = cfg.effective_exclude(Some(&runtime));
        let mydir_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "mydir").count();
        assert_eq!(mydir_count, 1);
    }

    #[test]
    fn effective_exclude_handles_no_trailing_slash_config() {
        let cfg = ProjectConfig { project: ProjectSection { exclude: vec!["folder".into()], ..Default::default() }, ..Default::default() };
        let runtime = vec!["folder/".to_string()];
        let exclude = cfg.effective_exclude(Some(&runtime));
        let folder_count = exclude.iter().filter(|e| e.trim_end_matches('/') == "folder").count();
        assert_eq!(folder_count, 1);
    }

    #[test]
    fn effective_exclude_empty_when_none() {
        let cfg = ProjectConfig::default();
        let exclude = cfg.effective_exclude(None);
        assert!(!exclude.is_empty());
    }

    // ── Exclude logic ──

    #[test]
    fn is_excluded_hardcoded_names() {
        let exclude: Vec<String> = vec![];
        assert!(is_excluded(".bunker", &exclude));
        assert!(is_excluded(".bunkerbox", &exclude));
        assert!(is_excluded(".git", &exclude));
    }

    #[test]
    fn is_excluded_by_pattern() {
        let exclude = vec!["target/".to_string()];
        assert!(is_excluded("target", &exclude));
    }

    #[test]
    fn is_excluded_not_in_patterns() {
        let exclude = vec!["target/".to_string()];
        assert!(!is_excluded("src", &exclude));
    }

    #[test]
    fn is_excluded_empty_patterns() {
        let exclude: Vec<String> = vec![];
        assert!(!is_excluded("src", &exclude));
    }

    #[test]
    fn is_excluded_trailing_slash_stripped() {
        let exclude = vec!["target/".to_string()];
        assert!(is_excluded("target", &exclude));
    }

    // ── Size computation ──

    #[test]
    fn walk_repo_size_counts_files() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "a.txt", 600);
        let exclude: Vec<String> = vec![];
        assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 600);
    }

    #[test]
    fn walk_repo_size_handles_empty_dir() {
        let root = TempDir::new().unwrap();
        let exclude: Vec<String> = vec![];
        assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 0);
    }

    #[test]
    fn walk_repo_size_skips_excluded_dirs() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/main.rs", 1000);
        mkfile(root.path(), "target/release/binary", 500000);
        let exclude = vec!["target/".to_string()];
        assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 1000);
    }

    #[test]
    fn walk_repo_size_skips_git_and_bunkerbox() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/lib.rs", 500);
        mkfile(root.path(), ".git/config", 200);
        mkfile(root.path(), ".bunkerbox/upper.img", 100000);
        let exclude: Vec<String> = vec![];
        assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 500);
    }

    #[test]
    fn walk_repo_size_handles_symlinks() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/lib.rs", 42);
        let exclude: Vec<String> = vec![];
        assert_eq!(walk_repo_size(root.path(), &exclude).unwrap(), 42);
    }

    // ── Auto quota ──

    #[test]
    fn compute_auto_quota_empty_repo_is_floor() {
        let root = TempDir::new().unwrap();
        let exclude: Vec<String> = vec![];
        assert_eq!(compute_auto_quota(root.path(), &exclude).unwrap(), MIN_QUOTA);
    }

    #[test]
    fn compute_auto_quota_applies_10_percent_overhead() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/lib.rs", 1_000_000_000);
        mkfile(root.path(), "README.md", 1_000_000_000);
        let exclude: Vec<String> = vec![];
        assert_eq!(compute_auto_quota(root.path(), &exclude).unwrap(), MIN_QUOTA);
    }

    #[test]
    fn compute_auto_quota_excludes_matching_patterns() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/lib.rs", 1_000_000_000);
        mkfile(root.path(), "vendor/big.bin", 5_000_000_000);
        let exclude = vec!["vendor/".to_string()];
        assert_eq!(compute_auto_quota(root.path(), &exclude).unwrap(), MIN_QUOTA);
    }

    #[test]
    fn compute_auto_quota_above_floor() {
        let root = TempDir::new().unwrap();
        mkfile(root.path(), "src/big.rs", 2_000_000_000);
        let exclude: Vec<String> = vec![];
        assert_eq!(compute_auto_quota(root.path(), &exclude).unwrap(), MIN_QUOTA);
    }

    // ── Size parsing ──

    #[test]
    fn parse_size_parses_gigabytes() {
        assert_eq!(parse_size("10G"), Some(10 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_size_parses_megabytes() {
        assert_eq!(parse_size("500M"), Some(500 * 1024 * 1024));
    }

    #[test]
    fn parse_size_returns_none_on_invalid() {
        assert_eq!(parse_size("xyz"), None);
    }
}
