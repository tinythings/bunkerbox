use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub binaries: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub ro_dirs: Vec<String>,
    #[serde(default)]
    pub rw_dirs: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub network: NetworkMode,
    #[serde(default = "default_shell")]
    pub shell: PathBuf,
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
    pub binaries: BTreeMap<String, PathBuf>,
    pub ro_dirs: Vec<String>,
    pub rw_dirs: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub network: NetworkMode,
    pub shell: PathBuf,
}

impl MergedProfile {
    pub fn from_profiles(profiles: &[Profile]) -> Self {
        let mut merged = MergedProfile::default();
        if profiles.is_empty() {
            merged.name = "default".into();
            merged.shell = PathBuf::from("/bin/sh");
            return merged;
        }
        merged.name = profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join("+");
        for p in profiles {
            for (k, v) in &p.binaries {
                merged.binaries.entry(k.clone()).or_insert_with(|| v.clone());
            }
            for d in &p.ro_dirs {
                let expanded = expand_vars(d);
                if !merged.ro_dirs.contains(&expanded) {
                    merged.ro_dirs.push(expanded);
                }
            }
            for d in &p.rw_dirs {
                let expanded = expand_vars(d);
                if !merged.rw_dirs.contains(&expanded) {
                    merged.rw_dirs.push(expanded);
                }
            }
            for (k, v) in &p.env {
                merged.env.entry(k.clone()).or_insert_with(|| expand_vars(v));
            }
            merged.network = p.network;
            merged.shell = p.shell.clone();
        }
        merged
    }
}

pub fn expand_vars(s: &str) -> String {
    let mut result = s.to_string();
    if let Ok(home) = std::env::var("HOME") {
        result = result.replace("${HOME}", &home);
    }
    if let Ok(user) = std::env::var("USER") {
        result = result.replace("${USER}", &user);
    }
    if let Ok(term) = std::env::var("TERM") {
        result = result.replace("${TERM}", &term);
    }
    result
}

pub fn parse_profile_yaml(yaml: &str) -> Result<Profile, String> {
    serde_yaml::from_str::<Profile>(yaml).map_err(|e| format!("failed to parse profile: {e}"))
}

pub fn resolve_profile(name_or_path: &str, share_dir: &std::path::Path) -> Result<Profile, String> {
    if name_or_path.starts_with('/') {
        let contents = std::fs::read_to_string(name_or_path)
            .map_err(|e| format!("failed to read profile {}: {e}", name_or_path))?;
        return parse_profile_yaml(&contents);
    }

    let share_path = share_dir.join("profiles").join(format!("{name_or_path}.yaml"));
    if share_path.exists() {
        let contents = std::fs::read_to_string(&share_path)
            .map_err(|e| format!("failed to read profile {}: {e}", share_path.display()))?;
        return parse_profile_yaml(&contents);
    }

    let builtin = get_builtin_profile(name_or_path)?;
    parse_profile_yaml(builtin)
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
mod tests {
    use super::*;

    #[test]
    fn test_parse_profile() {
        let yaml = r#"
name: test
binaries:
  ls: /usr/bin/ls
ro_dirs:
  - /lib
rw_dirs:
  - "${HOME}/.cache"
env:
  FOO: bar
network: none
shell: /bin/sh
"#;
        let profile = parse_profile_yaml(yaml).unwrap();
        assert_eq!(profile.name, "test");
        assert_eq!(profile.binaries.get("ls").unwrap(), &std::path::PathBuf::from("/usr/bin/ls"));
        assert_eq!(profile.ro_dirs.len(), 1);
        assert_eq!(profile.rw_dirs.len(), 1);
        assert!(matches!(profile.network, NetworkMode::None));
    }

    #[test]
    fn test_merge_profiles() {
        let p1 = Profile {
            name: "a".into(),
            binaries: {
                let mut m = BTreeMap::new();
                m.insert("cmd1".into(), "/usr/bin/cmd1".into());
                m
            },
            ro_dirs: vec!["/lib".into()],
            rw_dirs: vec!["/cache".into()],
            env: { let mut m = BTreeMap::new(); m.insert("A".into(), "1".into()); m },
            network: NetworkMode::None,
            shell: "/bin/sh".into(),
        };
        let p2 = Profile {
            name: "b".into(),
            binaries: {
                let mut m = BTreeMap::new();
                m.insert("cmd2".into(), "/usr/bin/cmd2".into());
                m
            },
            ro_dirs: vec!["/usr/lib".into()],
            rw_dirs: vec!["/other".into()],
            env: { let mut m = BTreeMap::new(); m.insert("B".into(), "2".into()); m },
            network: NetworkMode::None,
            shell: "/bin/dash".into(),
        };
        let merged = MergedProfile::from_profiles(&[p1, p2]);
        assert_eq!(merged.binaries.len(), 2);
        assert_eq!(merged.ro_dirs.len(), 2);
        assert_eq!(merged.rw_dirs.len(), 2);
        assert_eq!(merged.env.len(), 2);
        assert_eq!(merged.shell, PathBuf::from("/bin/dash"));
        assert_eq!(merged.name, "a+b");
    }

    #[test]
    fn test_resolve_builtin() {
        let profile = resolve_profile("make", std::path::Path::new("/nonexistent")).unwrap();
        assert_eq!(profile.name, "make");
        assert!(profile.binaries.contains_key("make"));
        assert!(profile.binaries.contains_key("gcc"));
    }
}
