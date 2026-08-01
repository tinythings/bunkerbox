use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::vscomm::{validate_env_key, validate_process_path, validate_process_string};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    #[serde(alias = "binaries")]
    pub bin: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub paths: Vec<ProfilePath>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub network: NetworkMode,
    #[serde(default = "default_shell")]
    pub shell: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePath {
    pub src: PathBuf,
    #[serde(default)]
    pub dst: Option<PathBuf>,
}

fn default_shell() -> PathBuf {
    PathBuf::from("/bin/sh")
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    #[default]
    None,
}

#[derive(Debug, Clone, Default)]
pub struct MergedProfile {
    pub name: String,
    pub bin: BTreeMap<String, PathBuf>,
    pub paths: Vec<ResolvedPath>,
    pub env: BTreeMap<String, String>,
    pub network: NetworkMode,
    pub shell: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub writable: bool,
}

pub fn validate_profile(profile: &Profile, source: &str) -> Result<(), String> {
    validate_process_string(&format!("profile {source} name"), &profile.name)?;

    for (name, path) in &profile.bin {
        validate_process_string(&format!("profile {source} binary name"), name)?;
        validate_process_path(&format!("profile {source} binary '{name}' path"), path)?;
    }

    for (index, path) in profile.paths.iter().enumerate() {
        validate_process_path(&format!("profile {source} path {index} source"), &path.src)?;
        if let Some(destination) = &path.dst {
            validate_process_path(&format!("profile {source} path {index} destination"), destination)?;
            if !destination.is_absolute() {
                return Err(format!("profile {source} path {index} destination must be absolute"));
            }
        }
        if path.src.as_os_str().is_empty() {
            return Err(format!("profile {source} path {index} source is empty"));
        }
        if path.src.components().any(|component| matches!(component, Component::ParentDir)) {
            return Err(format!("profile {source} path {index} source contains '..'"));
        }
    }

    for (key, value) in &profile.env {
        validate_env_key(&format!("profile {source} environment key"), key)?;
        validate_process_string(&format!("profile {source} environment value for '{key}'"), value)?;
    }

    validate_process_path(&format!("profile {source} shell path"), &profile.shell)?;
    Ok(())
}

impl MergedProfile {
    pub fn from_profiles(profiles: &[Profile], host_home: Option<&Path>) -> Result<Self, String> {
        let mut merged = MergedProfile::default();
        if profiles.is_empty() {
            merged.name = "default".into();
            merged.shell = PathBuf::from("/bin/sh");
            return Ok(merged);
        }
        merged.name = profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join("+");
        for p in profiles {
            validate_profile(p, &p.name)?;
            for (k, v) in &p.bin {
                merged.bin.entry(k.clone()).or_insert_with(|| v.clone());
            }
            for (index, path) in p.paths.iter().enumerate() {
                let resolved = resolve_path(path, host_home).map_err(|err| format!("profile {} path {index}: {err}", p.name))?;
                if !merged.paths.contains(&resolved) {
                    merged.paths.push(resolved);
                }
            }
            for (k, v) in &p.env {
                let expanded = expand_vars(v);
                validate_process_string(&format!("profile {} environment value for '{k}'", p.name), &expanded)?;
                merged.env.entry(k.clone()).or_insert(expanded);
            }
            merged.network = p.network;
            merged.shell = p.shell.clone();
        }
        Ok(merged)
    }
}

pub fn expand_vars(s: &str) -> String {
    let mut result = s.to_string();
    result = result.replace("${HOME}", "/home");
    if let Ok(user) = std::env::var("USER") {
        result = result.replace("${USER}", &user);
    }
    if let Ok(term) = std::env::var("TERM") {
        result = result.replace("${TERM}", &term);
    }
    result
}

fn resolve_path(path: &ProfilePath, host_home: Option<&Path>) -> Result<ResolvedPath, String> {
    let (source, relative_to_home, writable) = if path.src.is_absolute() {
        let source = path.src.clone();
        let relative = match host_home {
            Some(home) => source.strip_prefix(home).ok().filter(|relative| !relative.as_os_str().is_empty()).map(PathBuf::from),
            None => None,
        };
        let writable = relative.is_some();
        (source, relative, writable)
    } else {
        let home = host_home.ok_or_else(|| "relative source requires host HOME".to_string())?;
        (home.join(&path.src), Some(path.src.clone()), true)
    };

    let destination = if let Some(destination) = &path.dst {
        destination.clone()
    } else if let Some(relative) = relative_to_home {
        Path::new("/home").join(relative)
    } else {
        source.clone()
    };

    Ok(ResolvedPath { source, destination, writable })
}

pub fn parse_profile_yaml(yaml: &str) -> Result<Profile, String> {
    parse_profile_yaml_with_source(yaml, "configuration")
}

fn parse_profile_yaml_with_source(yaml: &str, source: &str) -> Result<Profile, String> {
    let profile = serde_yaml::from_str::<Profile>(yaml).map_err(|e| format!("failed to parse profile: {e}"))?;
    validate_profile(&profile, source)?;
    Ok(profile)
}

pub fn resolve_profile(name_or_path: &str, share_dir: &std::path::Path) -> Result<Profile, String> {
    if name_or_path.starts_with('/') {
        let contents = std::fs::read_to_string(name_or_path).map_err(|e| format!("failed to read profile {}: {e}", name_or_path))?;
        return parse_profile_yaml_with_source(&contents, name_or_path);
    }

    let share_path = share_dir.join("profiles").join(format!("{name_or_path}.yaml"));
    if share_path.exists() {
        let contents = std::fs::read_to_string(&share_path).map_err(|e| format!("failed to read profile {}: {e}", share_path.display()))?;
        return parse_profile_yaml_with_source(&contents, &share_path.display().to_string());
    }

    let builtin = get_builtin_profile(name_or_path)?;
    parse_profile_yaml_with_source(builtin, &format!("built-in '{name_or_path}'"))
}

fn get_builtin_profile(name: &str) -> Result<&str, String> {
    match name {
        "rust" => Ok(include_str!("../../profiles/rust.yaml")),
        "node" => Ok(include_str!("../../profiles/node.yaml")),
        "go" => Ok(include_str!("../../profiles/go.yaml")),
        "python" => Ok(include_str!("../../profiles/python.yaml")),
        "make" => Ok(include_str!("../../profiles/make.yaml")),
        _ => Err(format!("unknown built-in profile: {name}")),
    }
}

#[cfg(test)]
#[path = "ut.rs"]
mod sandbox_tests;
