use std::fs;
use std::path::Path;

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

const MIN_QUOTA: u64 = 1024 * 1024 * 1024;

#[derive(Debug, serde::Deserialize)]
pub struct EnvConfig {
    #[serde(default)]
    pub quota: Option<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl EnvConfig {
    pub const PATH: &str = ".bunkerbox/env.conf";

    pub fn load_or_create(repo_root: &Path) -> Result<Self, String> {
        let env_path = repo_root.join(Self::PATH);
        if env_path.exists() {
            let contents = fs::read_to_string(&env_path).map_err(|e| format!("failed to read {}: {e}", env_path.display()))?;
            serde_yaml::from_str(&contents).map_err(|e| format!("failed to parse {}: {e}", env_path.display()))
        } else {
            let bunkerbox_dir = env_path.parent().unwrap();
            fs::create_dir_all(bunkerbox_dir).map_err(|e| format!("failed to create {}: {e}", bunkerbox_dir.display()))?;

            let mut yaml = String::from(
                "# Bunkerbox workspace configuration\n\
                 # Edit this file to customize behavior.\n\n\
                 # Quota for copy-on-write workspace. \"auto\" = walk repo (skipping excluded dirs), +10%, floor 1G.\n\
                 # Use \"10G\", \"500M\", etc. for an explicit size.\n\
                 quota: auto\n\n\
                 # Directories excluded from copy-on-write (stored on host disk instead of in the capped loopback).\n\
                 # Patterns match directory names relative to the repository root.\n\
                 exclude:\n",
            );
            for pat in DEFAULT_EXCLUDE {
                yaml.push_str(&format!("  - {pat}\n"));
            }

            fs::write(&env_path, &yaml).map_err(|e| format!("failed to write {}: {e}", env_path.display()))?;

            Ok(EnvConfig { quota: Some("auto".to_string()), exclude: Vec::new() })
        }
    }

    pub fn quota_bytes(&self, _runtime_default: u64, repo_root: &Path, runtime_exclude: Option<&[String]>) -> Result<u64, String> {
        match &self.quota {
            None => compute_auto_quota(repo_root, &self.effective_exclude(runtime_exclude)),
            Some(s) if s == "auto" => compute_auto_quota(repo_root, &self.effective_exclude(runtime_exclude)),
            Some(s) => crate::runtime::parse_size(s).ok_or_else(|| format!("invalid quota: {s}")),
        }
    }

    pub fn effective_exclude(&self, runtime_exclude: Option<&[String]>) -> Vec<String> {
        let mut exclude: Vec<String> = DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect();
        for pat in &self.exclude {
            let p = pat.trim_end_matches('/');
            if !exclude.iter().any(|e| e.trim_end_matches('/') == p) {
                exclude.push(pat.clone());
            }
        }
        if let Some(runtime) = runtime_exclude {
            for pat in runtime {
                let p = pat.trim_end_matches('/');
                if !exclude.iter().any(|e| e.trim_end_matches('/') == p) {
                    exclude.push(pat.clone());
                }
            }
        }
        exclude
    }
}

fn compute_auto_quota(repo_root: &Path, exclude: &[String]) -> Result<u64, String> {
    let total = walk_repo_size(repo_root, exclude)?;
    let quota = total.saturating_mul(11).saturating_div(10);
    Ok(quota.max(MIN_QUOTA))
}

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

fn is_excluded(name: &str, exclude: &[String]) -> bool {
    if name == ".bunker" || name == ".bunkerbox" || name == ".git" {
        return true;
    }
    for pattern in exclude {
        let pat = pattern.trim_end_matches('/');
        if name == pat {
            return true;
        }
    }
    false
}
