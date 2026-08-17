use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::{tempdir, TempDir};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    temp: TempDir,
    project: PathBuf,
    key: PathBuf,
    known_hosts: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();

        let key = temp.path().join("id_ed25519");
        fs::write(&key, b"not-read-by-this-module").unwrap();
        set_mode(&key, 0o600);

        let known_hosts = temp.path().join("known_hosts");
        fs::write(&known_hosts, b"example ssh-ed25519 AAAA\n").unwrap();
        set_mode(&known_hosts, 0o644);

        Self { config: temp.path().join("remote-targets.yaml"), temp, project, key, known_hosts }
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    #[cfg(not(unix))]
    let _ = (path, mode);
}

fn scalar(value: &str) -> String {
    serde_yaml::to_string(value).unwrap().trim().to_string()
}

fn valid_yaml(fixture: &Fixture, project: &Path, backend: &str, target: Option<&str>) -> String {
    let target_line = target.map(|target| format!("    target: {}\n", scalar(target))).unwrap_or_default();
    format!(
        r#"version: 1
targets:
  ssh-one:
    transport: ssh
    host: build.example.test
    port: 2222
    user: builder
    identity-file: {}
    known-hosts-file: {}
    worker-path: /usr/local/libexec/bunkerbox-worker
    workspace-root: /var/tmp/bunkerbox-workers
    tools:
      cargo: /usr/local/bin/cargo
      make: /usr/bin/make
    environment:
      CC: clang
      PRIVATE_VALUE: super-secret-value
    resources:
      connect-timeout-seconds: 5
      sync-timeout-seconds: 120
      build-timeout-seconds: 3600
      max-output-bytes: 67108864
projects:
  {}:
    backend: {}
{}"#,
        scalar(fixture.key.to_str().unwrap()),
        scalar(fixture.known_hosts.to_str().unwrap()),
        scalar(project.to_str().unwrap()),
        backend,
        target_line,
    )
}

fn write_config(fixture: &Fixture, yaml: &str) {
    fs::write(&fixture.config, yaml).unwrap();
}

#[test]
fn valid_config_loads_and_validates_an_ssh_target() {
    let fixture = Fixture::new();
    write_config(&fixture, &valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")));

    let config = RemoteTargetConfig::load_from(&fixture.config).unwrap();
    let target = config.ssh_target("ssh-one").unwrap();
    assert_eq!(target.name(), "ssh-one");
    assert_eq!(target.host(), "build.example.test");
    assert_eq!(target.port(), 2222);
    assert_eq!(target.user(), "builder");
    assert_eq!(target.identity_file(), fixture.key.as_path());
    assert_eq!(target.known_hosts_file(), fixture.known_hosts.as_path());
    assert_eq!(target.worker_path(), "/usr/local/libexec/bunkerbox-worker");
    assert_eq!(target.workspace_root(), "/var/tmp/bunkerbox-workers");
    assert_eq!(target.tools().get("cargo").map(String::as_str), Some("/usr/local/bin/cargo"));
    assert_eq!(target.environment().get("CC").map(String::as_str), Some("clang"));
    assert_eq!(target.resources().connect_timeout(), Duration::from_secs(5));
    assert_eq!(target.resources().sync_timeout(), Duration::from_secs(120));
    assert_eq!(target.resources().build_timeout(), Duration::from_secs(3600));
    assert_eq!(target.resources().max_output_bytes(), 64 * 1024 * 1024);

    let debug = format!("{target:?}");
    assert!(!debug.contains("super-secret-value"));
}

#[test]
fn ssh_and_loopback_projects_resolve_explicitly() {
    let fixture = Fixture::new();
    let second_project = fixture.temp.path().join("loopback-project");
    fs::create_dir(&second_project).unwrap();
    let ssh_yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one"));
    let loopback_key = scalar(second_project.to_str().unwrap());
    let yaml = format!("{ssh_yaml}  {loopback_key}:\n    backend: loopback\n    target: target-that-does-not-exist\n");
    write_config(&fixture, &yaml);

    let config = RemoteTargetConfig::load_from(&fixture.config).unwrap();
    let ssh = config.resolve_for_project(&fixture.project).unwrap();
    assert_eq!(ssh.backend(), BackendMode::Ssh);
    assert_eq!(ssh.target().unwrap().name(), "ssh-one");

    let loopback = config.resolve_for_project(&second_project).unwrap();
    assert_eq!(loopback.backend(), BackendMode::Loopback);
    assert!(loopback.target().is_none());
}

#[test]
fn missing_project_binding_is_an_error() {
    let fixture = Fixture::new();
    let other = fixture.temp.path().join("other-project");
    fs::create_dir(&other).unwrap();
    write_config(&fixture, &valid_yaml(&fixture, &other, "loopback", None));

    let config = RemoteTargetConfig::load_from(&fixture.config).unwrap();
    assert!(config.resolve_for_project(&fixture.project).is_err());
}

#[test]
fn ssh_requires_a_known_target_and_a_target_name() {
    let fixture = Fixture::new();
    write_config(&fixture, &valid_yaml(&fixture, &fixture.project, "ssh", Some("missing")));
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    write_config(&fixture, &valid_yaml(&fixture, &fixture.project, "ssh", None));
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
}

#[test]
fn duplicate_yaml_map_keys_are_rejected() {
    let fixture = Fixture::new();
    let project = scalar(fixture.project.to_str().unwrap());
    let binding = "    backend: loopback\n".to_string();
    let yaml = format!("version: 1\ntargets: {{}}\nprojects:\n  {project}:\n{binding}  {project}:\n{binding}");
    write_config(&fixture, &yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
}

#[test]
fn duplicate_canonical_project_bindings_are_rejected() {
    let fixture = Fixture::new();
    let alias = fixture.temp.path().join("project-alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&fixture.project, &alias).unwrap();
    #[cfg(not(unix))]
    fs::create_dir(&alias).unwrap();

    let project = scalar(fixture.project.to_str().unwrap());
    let alias = scalar(alias.to_str().unwrap());
    let yaml = format!("version: 1\ntargets: {{}}\nprojects:\n  {project}:\n    backend: loopback\n  {alias}:\n    backend: loopback\n");
    write_config(&fixture, &yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
}

#[test]
fn invalid_host_port_and_user_are_rejected() {
    let fixture = Fixture::new();
    for (host, port, user) in [
        ("bad;host", "22", "builder"),
        ("build.example.test", "0", "builder"),
        ("build.example.test", "65536", "builder"),
        ("build.example.test", "22", "bad user"),
    ] {
        let yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one"))
            .replace("build.example.test", &scalar(host))
            .replace("port: 2222", &format!("port: {port}"))
            .replace("user: builder", &format!("user: {}", scalar(user)));
        write_config(&fixture, &yaml);
        assert!(RemoteTargetConfig::load_from(&fixture.config).is_err(), "accepted invalid target fields");
    }
}

#[test]
fn missing_unreadable_and_insecure_identity_files_are_rejected() {
    let fixture = Fixture::new();
    let missing = fixture.temp.path().join("missing-key");
    let missing_yaml =
        valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace(fixture.key.to_str().unwrap(), missing.to_str().unwrap());
    write_config(&fixture, &missing_yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    set_mode(&fixture.key, 0o644);
    write_config(&fixture, &valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")));
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    set_mode(&fixture.key, 0o000);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
}

#[test]
fn known_hosts_must_be_a_readable_regular_file() {
    let fixture = Fixture::new();
    let missing = fixture.temp.path().join("missing-known-hosts");
    let missing_yaml =
        valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace(fixture.known_hosts.to_str().unwrap(), missing.to_str().unwrap());
    write_config(&fixture, &missing_yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    let directory = fixture.temp.path().join("known-hosts-directory");
    fs::create_dir(&directory).unwrap();
    let directory_yaml =
        valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace(fixture.known_hosts.to_str().unwrap(), directory.to_str().unwrap());
    write_config(&fixture, &directory_yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    set_mode(&fixture.known_hosts, 0o000);
    write_config(&fixture, &valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")));
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
}

#[test]
fn worker_root_tool_and_local_paths_are_strict() {
    let fixture = Fixture::new();
    for (worker, root) in [
        ("relative-worker", "/var/tmp/workers"),
        ("/usr/bin/worker/../bad", "/var/tmp/workers"),
        ("/usr/bin/worker", "relative-root"),
        ("/usr/bin/worker", "/var/tmp/workers/../escape"),
    ] {
        let yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one"))
            .replace("/usr/local/libexec/bunkerbox-worker", worker)
            .replace("/var/tmp/bunkerbox-workers", root);
        write_config(&fixture, &yaml);
        assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
    }

    let tool_path_yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace("/usr/local/bin/cargo", "relative-cargo");
    write_config(&fixture, &tool_path_yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    let tool_identity_yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace("      cargo:", "      bad/tool:");
    write_config(&fixture, &tool_identity_yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    let local_path_yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace(fixture.key.to_str().unwrap(), "relative-key");
    write_config(&fixture, &local_path_yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
}

#[test]
fn environment_names_values_and_resource_bounds_are_validated() {
    let fixture = Fixture::new();
    for replacement in [
        ("      CC: clang", "      BAD-NAME: clang"),
        ("      CC: clang", "      CC: bad\nvalue"),
        ("      connect-timeout-seconds: 5", "      connect-timeout-seconds: 0"),
        ("      max-output-bytes: 67108864", "      max-output-bytes: 0"),
    ] {
        let yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace(replacement.0, replacement.1);
        write_config(&fixture, &yaml);
        assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
    }
}

#[test]
fn project_binding_paths_must_be_canonical_and_resolution_rejects_traversal() {
    let fixture = Fixture::new();
    let injected = fixture.temp.path().join("project").join("..").join("project");
    let yaml = valid_yaml(&fixture, &injected, "loopback", None);
    write_config(&fixture, &yaml);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    write_config(&fixture, &valid_yaml(&fixture, &fixture.project, "loopback", None));
    let config = RemoteTargetConfig::load_from(&fixture.config).unwrap();
    assert!(config.resolve_for_project(&injected).is_err());
}

#[test]
fn default_path_resolution_is_injectable_and_uses_xdg_then_home_fallback() {
    let fixture = Fixture::new();
    let xdg = fixture.temp.path().join("xdg");
    let home = fixture.temp.path().join("home");
    let helper = ConfigPathHelper::new(Some(xdg.clone()), Some(home.clone()));
    assert_eq!(helper.config_path().unwrap(), xdg.join("bunkerbox").join(CONFIG_FILE_NAME));

    let fallback = ConfigPathHelper::new(None, Some(home.clone()));
    assert_eq!(fallback.config_path().unwrap(), home.join(".config").join("bunkerbox").join(CONFIG_FILE_NAME));

    fs::create_dir_all(xdg.join("bunkerbox")).unwrap();
    let config_path = helper.config_path().unwrap();
    fs::write(&config_path, valid_yaml(&fixture, &fixture.project, "loopback", None)).unwrap();
    assert_eq!(RemoteTargetConfig::load_default_with_path_helper(&helper).unwrap().source_path(), config_path.as_path());
}

#[test]
fn config_version_and_transport_are_strict() {
    let fixture = Fixture::new();
    let version = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace("version: 1", "version: 2");
    write_config(&fixture, &version);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());

    let transport = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace("transport: ssh", "transport: loopback");
    write_config(&fixture, &transport);
    assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
}

#[test]
fn project_artifact_policy_and_limits_are_loaded_from_trusted_binding() {
    let fixture = Fixture::new();
    let marker = format!("  {}:\n    backend: ssh\n    target: ssh-one\n", scalar(fixture.project.to_str().unwrap()));
    let yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one"))
        .replace(
            "      max-output-bytes: 67108864\n",
            "      max-output-bytes: 67108864\n      artifact-timeout-seconds: 7\n      max-artifact-bytes: 1M\n      max-artifact-total-bytes: 2M\n      max-artifact-entries: 3\n",
        )
        .replace(
            &marker,
            &format!(
                "{marker}    artifacts:\n      paths:\n        - target/result\n        - dist/package.tar.gz\n"
            ),
        );
    write_config(&fixture, &yaml);

    let config = RemoteTargetConfig::load_from(&fixture.config).unwrap();
    let resolved = config.resolve_for_project(&fixture.project).unwrap();
    assert_eq!(resolved.artifacts().paths(), ["target/result", "dist/package.tar.gz"]);
    let limits = resolved.target().unwrap().resources().artifact_limits();
    assert_eq!(limits.timeout, Duration::from_secs(7));
    assert_eq!(limits.max_entries, 3);
    assert_eq!(limits.max_file_bytes, 1024 * 1024);
    assert_eq!(limits.max_total_bytes, 2 * 1024 * 1024);
}

#[test]
fn project_artifact_policy_must_fit_target_limits() {
    let fixture = Fixture::new();
    let marker = format!("  {}:\n    backend: ssh\n    target: ssh-one\n", scalar(fixture.project.to_str().unwrap()));
    let yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one"))
        .replace("      max-output-bytes: 67108864\n", "      max-output-bytes: 67108864\n      max-artifact-entries: 1\n")
        .replace(&marker, &format!("{marker}    artifacts:\n      paths:\n        - target/result\n        - dist/package.tar.gz\n"));
    write_config(&fixture, &yaml);

    let error = RemoteTargetConfig::load_from(&fixture.config).unwrap_err();
    assert!(error.contains("artifact policy exceeds configured entry count 1"));
}

#[test]
fn project_artifact_paths_reject_traversal_globs_and_duplicates() {
    let fixture = Fixture::new();
    let marker = format!("  {}:\n    backend: loopback\n", scalar(fixture.project.to_str().unwrap()));
    for paths in ["        - /absolute\n", "        - ../escape\n", "        - dist/*\n", "        - result\n        - result\n"] {
        let yaml =
            valid_yaml(&fixture, &fixture.project, "loopback", None).replace(&marker, &format!("{marker}    artifacts:\n      paths:\n{paths}"));
        write_config(&fixture, &yaml);
        assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
    }
}

#[test]
fn lifecycle_admission_and_worker_limits_are_loaded_with_safe_defaults() {
    let fixture = Fixture::new();
    let yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace(
        "      max-output-bytes: 67108864\n",
        "      max-output-bytes: 67108864\n      idle-output-timeout-seconds: 7\n      cleanup-timeout-seconds: 3\n      max-active-builds: 2\n      max-worker-uploads: 3\n      max-worker-upload-bytes: 2M\n      max-worker-jobs: 2\n      max-worker-job-bytes: 4M\n      max-worker-artifact-spools: 2\n      max-worker-artifact-spool-bytes: 5M\n      max-worker-state-entries: 99\n",
    );
    write_config(&fixture, &yaml);
    let target = RemoteTargetConfig::load_from(&fixture.config).unwrap().ssh_target("ssh-one").unwrap().resources();
    assert_eq!(target.idle_output_timeout(), Duration::from_secs(7));
    assert_eq!(target.cleanup_timeout(), Duration::from_secs(3));
    assert_eq!(target.max_active_builds(), 2);
    assert_eq!(target.worker_state_limits().max_uploads, 3);
    assert_eq!(target.worker_state_limits().max_upload_bytes, 2 * 1024 * 1024);
    assert_eq!(target.worker_state_limits().max_state_entries, 99);
}

#[test]
fn lifecycle_and_admission_limits_reject_zero_or_excessive_values() {
    let fixture = Fixture::new();
    for replacement in [
        ("      connect-timeout-seconds: 5", "      connect-timeout-seconds: 0"),
        ("      max-output-bytes: 67108864", "      max-active-builds: 65\n      max-output-bytes: 67108864"),
        ("      max-output-bytes: 67108864", "      cleanup-timeout-seconds: 0\n      max-output-bytes: 67108864"),
    ] {
        let yaml = valid_yaml(&fixture, &fixture.project, "ssh", Some("ssh-one")).replace(replacement.0, replacement.1);
        write_config(&fixture, &yaml);
        assert!(RemoteTargetConfig::load_from(&fixture.config).is_err());
    }
}
