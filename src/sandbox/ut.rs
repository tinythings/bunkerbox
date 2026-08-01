use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[test]
fn test_parse_profile() {
    let yaml = r#"
name: test
bin:
  ls: /usr/bin/ls
paths:
  - src: /lib
  - src: .cache
env:
  FOO: bar
network: none
shell: /bin/sh
"#;
    let profile = parse_profile_yaml(yaml).unwrap();
    assert_eq!(profile.name, "test");
    assert_eq!(profile.bin.get("ls").unwrap(), &std::path::PathBuf::from("/usr/bin/ls"));
    assert_eq!(profile.paths.len(), 2);
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
        paths: vec![ProfilePath { src: "/lib".into(), dst: None }, ProfilePath { src: "cache".into(), dst: None }],
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
        paths: vec![ProfilePath { src: "/usr/lib".into(), dst: None }, ProfilePath { src: "other".into(), dst: None }],
        env: {
            let mut m = BTreeMap::new();
            m.insert("B".into(), "2".into());
            m
        },
        network: NetworkMode::None,
        shell: "/bin/dash".into(),
    };
    let merged = MergedProfile::from_profiles(&[p1, p2], Some(Path::new("/home/test"))).unwrap();
    assert_eq!(merged.bin.len(), 2);
    assert_eq!(merged.paths.len(), 4);
    assert!(!merged.paths[0].writable);
    assert_eq!(merged.paths[1].destination, PathBuf::from("/home/cache"));
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

#[test]
fn reject_nul_in_profile_environment() {
    let yaml = r#"
name: test
env:
  TOOLCHAIN: "bad\0value"
"#;

    let err = parse_profile_yaml(yaml).unwrap_err();
    assert!(err.contains("environment value"));
    assert!(err.contains("NUL"));
}

#[test]
fn reject_invalid_profile_environment_key() {
    let yaml = r#"
name: test
env:
  BAD=KEY: value
"#;

    let err = parse_profile_yaml(yaml).unwrap_err();
    assert!(err.contains("environment key"));
    assert!(err.contains("'='"));
}

#[test]
fn reject_nul_in_profile_environment_key_without_returning_nul() {
    let profile = Profile {
        name: "test".into(),
        bin: Default::default(),
        paths: Vec::new(),
        env: [("BAD\0KEY".into(), "value".into())].into_iter().collect(),
        network: NetworkMode::None,
        shell: "/bin/sh".into(),
    };

    let err = validate_profile(&profile, "test").unwrap_err();
    assert!(err.contains("environment key"));
    assert!(err.contains("NUL"));
    assert!(!err.contains('\0'));
}

#[test]
fn resolve_paths_with_standard_and_explicit_destinations() {
    let profile = Profile {
        name: "paths".into(),
        bin: Default::default(),
        paths: vec![
            ProfilePath { src: ".cargo".into(), dst: None },
            ProfilePath { src: "/home/bo/.rustup".into(), dst: None },
            ProfilePath { src: "/opt/sdk/include".into(), dst: None },
            ProfilePath { src: "/opt/sdk/lib".into(), dst: Some("/toolchain/lib".into()) },
        ],
        env: Default::default(),
        network: NetworkMode::None,
        shell: "/bin/sh".into(),
    };

    let merged = MergedProfile::from_profiles(&[profile], Some(Path::new("/home/bo"))).unwrap();

    assert_eq!(merged.paths[0].source, PathBuf::from("/home/bo/.cargo"));
    assert_eq!(merged.paths[0].destination, PathBuf::from("/home/.cargo"));
    assert!(merged.paths[0].writable);
    assert_eq!(merged.paths[1].destination, PathBuf::from("/home/.rustup"));
    assert!(merged.paths[1].writable);
    assert_eq!(merged.paths[2].destination, PathBuf::from("/opt/sdk/include"));
    assert!(!merged.paths[2].writable);
    assert_eq!(merged.paths[3].destination, PathBuf::from("/toolchain/lib"));
    assert!(!merged.paths[3].writable);
}

#[test]
fn all_builtin_profiles_use_paths() {
    for name in ["rust", "make", "node", "go", "python"] {
        let profile = resolve_profile(name, Path::new("/nonexistent")).unwrap();
        assert!(!profile.paths.is_empty(), "{name} has no paths");
    }
}
