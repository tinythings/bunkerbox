use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::vscomm::buildsys::{self, PassthroughMode};

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

/// Controls whether the guest VM environment is forwarded to host commands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvMode {
    /// Guest environment is selectively passed to the host command.
    /// Passthrough commands run inside a bubblewrap sandbox.
    #[default]
    Relaxed,
    /// Guest environment is fully dropped. Commands inherit the host daemon's
    /// environment only. Glob patterns in `passthrough` are rejected.
    /// Passthrough commands run inside a bubblewrap sandbox.
    Paranoid,
    /// Guest environment is filtered and passed through, but the bubblewrap
    /// sandbox is disabled. The command runs directly on the host. Use with care.
    Dangerous,
}

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

    /// Load the first valid runtime config found in a share directory.
    pub fn load_from_share_dir(share_dir: &Path) -> Option<Self> {
        let entries = fs::read_dir(share_dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "conf") {
                if let Ok(cfg) = serde_yaml::from_str(&fs::read_to_string(&path).ok()?) {
                    return Some(cfg);
                }
            }
        }
        None
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
    pub session_mb: u32,
    pub network: Option<NetworkMode>,
    pub allow: Option<Vec<String>>,
    pub encrypt: Option<Vec<String>>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub project: ProjectSection,
    #[serde(default)]
    pub image: ImageOverrides,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SandboxConfig {
    /// Optional absolute path to a bubblewrap binary.
    /// If unset, "bwrap" is resolved from PATH.
    pub bwrap: Option<PathBuf>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ProjectSection {
    #[serde(default)]
    pub env: EnvMode,
    #[serde(default)]
    pub quota: Option<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub passthrough: Vec<String>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ImageOverrides {
    #[serde(default)]
    pub workspace: Option<WorkspaceMode>,
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
                .and_then(|mut cfg: Self| {
                    cfg.auto_fill_passthrough(repo_root, &path);
                    cfg.validate()?;
                    Ok(cfg)
                });
        }

        let legacy = repo_root.join(Self::legacy_path());
        if legacy.exists() {
            return Self::migrate_from_env_conf(repo_root, &legacy, &path);
        }

        let cfg = ProjectConfig {
            project: ProjectSection {
                env: EnvMode::default(),
                quota: Some("auto".into()),
                exclude: Vec::new(),
                passthrough: buildsys::scan(repo_root, PassthroughMode::Relaxed),
                sandbox: SandboxConfig::default(),
            },
            image: ImageOverrides::default(),
        };
        cfg.validate()?;
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("failed to create {}: {e}", path.parent().unwrap().display()))?;
        fs::write(&path, cfg.to_yaml()).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        if self.project.env == EnvMode::Paranoid {
            for entry in &self.project.passthrough {
                if entry.contains('*') {
                    return Err(format!(
                        "paranoid env mode: glob patterns are not allowed in passthrough. Found '{entry}'. Use exact command names only."
                    ));
                }
            }
        }
        if let Some(ref bwrap) = self.project.sandbox.bwrap {
            if !Path::is_absolute(bwrap) {
                return Err(format!("sandbox.bwrap must be an absolute path, got: {}", bwrap.display()));
            }
        }
        Ok(())
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
            project: ProjectSection {
                env: EnvMode::default(),
                quota: old.quota,
                exclude: old.exclude,
                passthrough: old.passthrough,
                sandbox: SandboxConfig::default(),
            },
            image: ImageOverrides::default(),
        };
        cfg.validate()?;

        fs::write(new_path, cfg.to_yaml()).map_err(|e| format!("failed to write {}: {e}", new_path.display()))?;
        let _ = fs::remove_file(legacy_path);

        Ok(cfg)
    }

    fn auto_fill_passthrough(&mut self, repo_root: &Path, path: &Path) {
        if self.project.passthrough.is_empty() {
            self.project.passthrough = buildsys::scan(repo_root, PassthroughMode::Relaxed);
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
        match self.project.env {
            EnvMode::Relaxed => {}
            EnvMode::Paranoid => Self::yaml_field(&mut y, "  env", "paranoid"),
            EnvMode::Dangerous => Self::yaml_field(&mut y, "  env", "dangerous"),
        }
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

        if self.project.sandbox.bwrap.is_some() {
            y.push('\n');
            y.push_str("sandbox:\n");
            if let Some(ref bwrap) = self.project.sandbox.bwrap {
                Self::yaml_field(&mut y, "  bwrap", &bwrap.to_string_lossy());
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
        self.workspace.is_some() || self.session_mb.is_some() || self.allow.is_some()
    }
}

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

#[cfg(test)]
#[path = "cfg_ut.rs"]
mod cfg_tests;
