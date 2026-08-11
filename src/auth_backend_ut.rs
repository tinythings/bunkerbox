use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[test]
fn find_on_path_finds_binary_in_path_dir() {
    let dir = TempDir::with_prefix("bb-test-auth-path").unwrap();
    let exe = dir.path().join("bunkerbox-auth-backend");
    {
        let mut f = std::fs::File::create(&exe).unwrap();
        f.write_all(b"#!/bin/sh\necho ok\n").unwrap();
    }
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

    unsafe { std::env::set_var("PATH", dir.path().as_os_str()); }
    let found = find_on_path("bunkerbox-auth-backend");
    unsafe { std::env::remove_var("PATH"); }

    assert!(found.is_some());
}

#[test]
fn find_on_path_returns_none_for_missing() {
    let dir = TempDir::with_prefix("bb-test-empty-path").unwrap();
    unsafe { std::env::set_var("PATH", dir.path().as_os_str()); }
    let found = find_on_path("nonexistent-foobar-xyz");
    unsafe { std::env::remove_var("PATH"); }
    assert!(found.is_none());
}

#[test]
fn load_named_config_reads_valid_yaml() {
    let dir = TempDir::with_prefix("bb-test-cfg").unwrap();
    let path = dir.path().join("test-provider.conf");
    std::fs::write(
        &path,
        "host: https://example.com\nauth:\n  type: env\n  variable: TEST_KEY\nrequest_headers: {}\n",
    )
    .unwrap();

    let cfg = load_named_config("test-provider", dir.path()).unwrap();
    match cfg {
        AuthBackendConfig::Declarative(ref d) => {
            assert_eq!(d.host, "https://example.com");
            assert!(matches!(d.auth, crate::cfg::AuthFlow::Env { .. }));
        }
        _ => panic!("expected declarative"),
    }
}

#[test]
fn load_named_config_rejects_missing_file() {
    let dir = TempDir::with_prefix("bb-test-no-cfg").unwrap();
    let result = load_named_config("nonexistent", dir.path());
    assert!(result.is_err());
}

#[test]
fn load_named_config_rejects_invalid_yaml() {
    let dir = TempDir::with_prefix("bb-test-bad-yaml").unwrap();
    let path = dir.path().join("borked.conf");
    std::fs::write(&path, "{{{[[[\n").unwrap();
    let result = load_named_config("borked", dir.path());
    assert!(result.is_err());
}

#[test]
fn discover_auth_plugin_errors_for_unknown_name() {
    let dir = TempDir::with_prefix("bb-test-plugin-empty").unwrap();
    unsafe { std::env::set_var("PATH", dir.path().as_os_str()); }
    let result = discover_auth_plugin("nonexistent-plugin-xyz");
    unsafe { std::env::remove_var("PATH"); }
    assert!(result.is_err());
}
