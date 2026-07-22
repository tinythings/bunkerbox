use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    #[serde(alias = "binaries")]
    pub bin: BTreeMap<String, PathBuf>,
    #[serde(default, alias = "ro_dirs")]
    pub ro: Vec<String>,
    #[serde(default, alias = "rw_dirs")]
    pub rw: Vec<String>,
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
    pub bin: BTreeMap<String, PathBuf>,
    pub ro: Vec<String>,
    pub rw: Vec<String>,
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
            for (k, v) in &p.bin {
                merged.bin.entry(k.clone()).or_insert_with(|| v.clone());
            }
            for d in &p.ro {
                let expanded = expand_vars(d);
                if !merged.ro.contains(&expanded) {
                    merged.ro.push(expanded);
                }
            }
            for d in &p.rw {
                let expanded = expand_vars(d);
                if !merged.rw.contains(&expanded) {
                    merged.rw.push(expanded);
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
        let contents = std::fs::read_to_string(name_or_path).map_err(|e| format!("failed to read profile {}: {e}", name_or_path))?;
        return parse_profile_yaml(&contents);
    }

    let share_path = share_dir.join("profiles").join(format!("{name_or_path}.yaml"));
    if share_path.exists() {
        let contents = std::fs::read_to_string(&share_path).map_err(|e| format!("failed to read profile {}: {e}", share_path.display()))?;
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
bin:
  ls: /usr/bin/ls
ro:
  - /lib
rw:
  - "${HOME}/.cache"
env:
  FOO: bar
network: none
shell: /bin/sh
"#;
        let profile = parse_profile_yaml(yaml).unwrap();
        assert_eq!(profile.name, "test");
        assert_eq!(profile.bin.get("ls").unwrap(), &std::path::PathBuf::from("/usr/bin/ls"));
        assert_eq!(profile.ro.len(), 1);
        assert_eq!(profile.rw.len(), 1);
        assert!(matches!(profile.network, NetworkMode::None));
    }

    #[test]
    fn test_merge_profiles() {
        let p1 = Profile {
            name: "a".into(),
            bin: {
                let mut m = BTreeMap::new();
                m.insert("cmd1".into(), "/usr/bin/cmd1".into());
                m
            },
            ro: vec!["/lib".into()],
            rw: vec!["/cache".into()],
            env: {
                let mut m = BTreeMap::new();
                m.insert("A".into(), "1".into());
                m
            },
            network: NetworkMode::None,
            shell: "/bin/sh".into(),
        };
        let p2 = Profile {
            name: "b".into(),
            bin: {
                let mut m = BTreeMap::new();
                m.insert("cmd2".into(), "/usr/bin/cmd2".into());
                m
            },
            ro: vec!["/usr/lib".into()],
            rw: vec!["/other".into()],
            env: {
                let mut m = BTreeMap::new();
                m.insert("B".into(), "2".into());
                m
            },
            network: NetworkMode::None,
            shell: "/bin/dash".into(),
        };
        let merged = MergedProfile::from_profiles(&[p1, p2]);
        assert_eq!(merged.bin.len(), 2);
        assert_eq!(merged.ro.len(), 2);
        assert_eq!(merged.rw.len(), 2);
        assert_eq!(merged.env.len(), 2);
        assert_eq!(merged.shell, PathBuf::from("/bin/dash"));
        assert_eq!(merged.name, "a+b");
    }

    #[test]
    fn test_resolve_builtin() {
        let profile = resolve_profile("make", std::path::Path::new("/nonexistent")).unwrap();
        assert_eq!(profile.name, "make");
        assert!(profile.bin.contains_key("make"));
        assert!(profile.bin.contains_key("gcc"));
    }
}
