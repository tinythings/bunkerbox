use std::fs;
use std::path::Path;

#[cfg(test)]
#[path = "envconf_ut.rs"]
mod envconf_tests;

/// Default directories excluded from copy-on-write workspace snapshots.
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

/// Minimum quota floor (1 GiB) used when computing auto quota.
const MIN_QUOTA: u64 = 1024 * 1024 * 1024;

/// Persistent per-repository configuration stored in `.bunkerbox/env.conf`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct EnvConfig {
    #[serde(default)]
    pub quota: Option<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl EnvConfig {
    /// Relative filesystem path to the environment config file.
    pub const PATH: &str = ".bunkerbox/env.conf";

    /// Loads the environment config from `.bunkerbox/env.conf`, or creates a
    /// default config file and returns `auto`-quota defaults if it does not exist.
    pub fn load_or_create(repo_root: &Path) -> Result<Self, String> {
        let env_path = repo_root.join(Self::PATH);
        if env_path.exists() {
            serde_yaml::from_str(&fs::read_to_string(&env_path).map_err(|e| format!("failed to read {}: {e}", env_path.display()))?)
                .map_err(|e| format!("failed to parse {}: {e}", env_path.display()))
        } else {
            let default_config = EnvConfig { quota: Some("auto".to_string()), exclude: Vec::new() };
            fs::create_dir_all(env_path.parent().unwrap()).map_err(|e| format!("failed to create {}: {e}", env_path.parent().unwrap().display()))?;
            fs::write(&env_path, serde_yaml::to_string(&default_config).map_err(|e| format!("failed to serialize default config: {e}"))?)
                .map_err(|e| format!("failed to write {}: {e}", env_path.display()))?;
            Ok(default_config)
        }
    }

    /// Returns the effective quota in bytes, resolving `"auto"` or `None` by
    /// walking the repository tree (skipping excluded directories) and applying
    /// a 10% buffer with a 1 GiB floor.
    pub fn quota_bytes(&self, _runtime_default: u64, repo_root: &Path, runtime_exclude: Option<&[String]>) -> Result<u64, String> {
        match &self.quota {
            None => compute_auto_quota(repo_root, &self.effective_exclude(runtime_exclude)),
            Some(s) if s == "auto" => compute_auto_quota(repo_root, &self.effective_exclude(runtime_exclude)),
            Some(s) => crate::runtime::parse_size(s).ok_or_else(|| format!("invalid quota: {s}")),
        }
    }

    /// Returns the strict cow quota in bytes. In strict cow mode every writable path
    /// must be backed by the bounded image, so auto sizing walks the full project tree.
    pub fn strict_cow_quota_bytes(&self, repo_root: &Path) -> Result<u64, String> {
        match &self.quota {
            None => compute_auto_quota(repo_root, &[]),
            Some(s) if s == "auto" => compute_auto_quota(repo_root, &[]),
            Some(s) => crate::runtime::parse_size(s).ok_or_else(|| format!("invalid quota: {s}")),
        }
    }

    /// Merges the default exclude list, config-specified excludes, and runtime
    /// excludes, deduplicating entries (comparison is done without trailing slashes).
    pub fn effective_exclude(&self, runtime_exclude: Option<&[String]>) -> Vec<String> {
        let mut exclude: Vec<String> = DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect();
        for pat in &self.exclude {
            if !exclude.iter().any(|e| e.trim_end_matches('/') == pat.trim_end_matches('/')) {
                exclude.push(pat.clone());
            }
        }
        if let Some(runtime) = runtime_exclude {
            for pat in runtime {
                if !exclude.iter().any(|e| e.trim_end_matches('/') == pat.trim_end_matches('/')) {
                    exclude.push(pat.clone());
                }
            }
        }
        exclude
    }
}

/// Walks the repository to compute its total size (excluding specified
/// directories) and returns the auto quota: size + 10%, floored at 1 GiB.
fn compute_auto_quota(repo_root: &Path, exclude: &[String]) -> Result<u64, String> {
    Ok(walk_repo_size(repo_root, exclude)?.saturating_mul(11).saturating_div(10).max(MIN_QUOTA))
}

/// Recursively walks a directory tree, summing file sizes while skipping
/// entries whose file name matches the exclude list.
fn walk_repo_size(dir: &Path, exclude: &[String]) -> Result<u64, String> {
    let mut total: u64 = 0;
    for entry in fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))? {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
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

/// Returns `true` if `name` is one of the well-known excluded directories
/// (`.bunker`, `.bunkerbox`, `.git`) or matches any pattern in `exclude`
/// (comparison is done without trailing slashes).
fn is_excluded(name: &str, exclude: &[String]) -> bool {
    name == ".bunker" || name == ".bunkerbox" || name == ".git" || exclude.iter().any(|p| name == p.trim_end_matches('/'))
}
