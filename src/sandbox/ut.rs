use super::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
