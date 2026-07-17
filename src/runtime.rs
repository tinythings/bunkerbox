use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_SHARE_DIR: &str = "/usr/share/bunkerbox";

#[derive(Debug, Deserialize)]
pub struct RuntimeConfig {
    pub oci: PathBuf,
    pub image: String,
    pub network: Option<NetworkMode>,
    pub allow: Option<Vec<String>>,
    pub workspace: Option<WorkspaceMode>,
    pub workspace_quota: Option<String>,
    pub home: Option<HomeMode>,
    pub home_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    Bridge,
    Host,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum WorkspaceMode {
    #[serde(alias = "share")]
    #[default]
    Cow,
    Direct,
    #[serde(alias = "clone")]
    Isolated,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HomeMode {
    Persist,
    Temporary,
}

pub fn invoked_name() -> Result<String, String> {
    let arg0 = env::args_os().next().ok_or_else(|| "missing argv[0]".to_string())?;

    let name = Path::new(&arg0).file_name().and_then(|name| name.to_str()).ok_or_else(|| "invalid argv[0]".to_string())?.to_string();

    Ok(name)
}

pub fn load_for_invoked_name(share_dir: &Path) -> Result<Option<RuntimeConfig>, String> {
    let name = invoked_name()?;

    if name == "bunkerbox" {
        return Ok(None);
    }

    let path = share_dir.join(format!("{name}.conf"));
    let contents = fs::read_to_string(&path).map_err(|err| format!("failed to read runtime config {}: {err}", path.display()))?;
    let config = serde_yaml::from_str(&contents).map_err(|err| format!("failed to parse runtime config {}: {err}", path.display()))?;

    Ok(Some(config))
}

impl RuntimeConfig {
    pub fn workspace_quota_bytes(&self) -> u64 {
        let default = 10 * 1024 * 1024 * 1024;
        let raw = match &self.workspace_quota {
            Some(s) => s.as_str(),
            None => return default,
        };
        parse_size(raw).unwrap_or(default)
    }
}

pub(crate) fn parse_size(raw: &str) -> Option<u64> {
    let raw = raw.trim().to_uppercase();
    let (num_str, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let num: u64 = num_str.parse().ok()?;
    match unit {
        "" | "B" => Some(num),
        "K" | "KB" => Some(num * 1024),
        "M" | "MB" => Some(num * 1024 * 1024),
        "G" | "GB" => Some(num * 1024 * 1024 * 1024),
        _ => None,
    }
}
