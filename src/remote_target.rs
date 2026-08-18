use crate::artifact::{ArtifactLimits, ArtifactPolicy};
use crate::cfg::{ProjectConfig, RemoteSection};
use crate::remote::{RemoteEnvironmentPolicy, RemoteToolPolicy};
use serde::de::{self, MapAccess, Visitor};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs::{self, File};
use std::marker::PhantomData;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub const CONFIG_VERSION: u64 = 1;
pub const CONFIG_FILE_NAME: &str = "remote-targets.yaml";
pub const CONFIG_DIRECTORY_NAME: &str = "bunkerbox";
pub const REMOTE_PROJECT_CONFIG_FILE_NAME: &str = "remote.conf";
pub const FIXED_WORKER_PATH: &str = "/usr/local/libexec/bunkerbox-worker";

const MAX_CONFIG_PATH_BYTES: usize = 4096;
const MAX_REMOTE_PATH_BYTES: usize = 4096;
const MAX_HOST_BYTES: usize = 253;
const MAX_USER_BYTES: usize = 64;
const MAX_NAME_BYTES: usize = 64;
const MAX_TOOL_PATH_BYTES: usize = 4096;
const MAX_ENV_NAME_BYTES: usize = 256;
const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;

/// Selects the host-side facility used for a project.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackendMode {
    Loopback,
    Ssh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    pub connect_timeout: Duration,
    pub sync_timeout: Duration,
    pub build_timeout: Duration,
    pub idle_output_timeout: Duration,
    pub cleanup_timeout: Duration,
    pub max_output_bytes: u64,
    pub max_active_builds: usize,
    pub artifact: ArtifactLimits,
    pub worker: WorkerStateLimits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerStateLimits {
    pub max_uploads: usize,
    pub max_upload_bytes: u64,
    pub max_jobs: usize,
    pub max_job_bytes: u64,
    pub max_artifact_spools: usize,
    pub max_artifact_spool_bytes: u64,
    pub max_state_entries: usize,
}

impl Default for WorkerStateLimits {
    fn default() -> Self {
        Self {
            max_uploads: 2,
            max_upload_bytes: 1024 * 1024 * 1024,
            max_jobs: 1,
            max_job_bytes: 512 * 1024 * 1024,
            max_artifact_spools: 1,
            max_artifact_spool_bytes: 512 * 1024 * 1024,
            max_state_entries: 20_000,
        }
    }
}

impl WorkerStateLimits {
    pub const MAX_COUNT: usize = 1_000;
    pub const MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;
    pub const MAX_ENTRIES: usize = 1_000_000;

    pub fn new(
        max_uploads: usize, max_upload_bytes: u64, max_jobs: usize, max_job_bytes: u64, max_artifact_spools: usize, max_artifact_spool_bytes: u64,
        max_state_entries: usize,
    ) -> Result<Self, String> {
        if max_uploads == 0 || max_jobs == 0 || max_artifact_spools == 0 {
            return Err("worker state counts must be positive".to_string());
        }
        if max_uploads > Self::MAX_COUNT || max_jobs > Self::MAX_COUNT || max_artifact_spools > Self::MAX_COUNT {
            return Err(format!("worker state counts must not exceed {}", Self::MAX_COUNT));
        }
        if max_upload_bytes == 0
            || max_job_bytes == 0
            || max_artifact_spool_bytes == 0
            || max_upload_bytes > Self::MAX_BYTES
            || max_job_bytes > Self::MAX_BYTES
            || max_artifact_spool_bytes > Self::MAX_BYTES
        {
            return Err(format!("worker state byte limits must be between 1 and {}", Self::MAX_BYTES));
        }
        if max_state_entries == 0 || max_state_entries > Self::MAX_ENTRIES {
            return Err(format!("worker state entry limit must be between 1 and {}", Self::MAX_ENTRIES));
        }
        Ok(Self { max_uploads, max_upload_bytes, max_jobs, max_job_bytes, max_artifact_spools, max_artifact_spool_bytes, max_state_entries })
    }
}

impl ResourceLimits {
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub fn sync_timeout(&self) -> Duration {
        self.sync_timeout
    }

    pub fn build_timeout(&self) -> Duration {
        self.build_timeout
    }

    pub fn idle_output_timeout(&self) -> Duration {
        self.idle_output_timeout
    }

    pub fn cleanup_timeout(&self) -> Duration {
        self.cleanup_timeout
    }

    pub fn max_output(&self) -> u64 {
        self.max_output_bytes
    }

    pub fn max_output_bytes(&self) -> u64 {
        self.max_output_bytes
    }

    pub fn max_active_builds(&self) -> usize {
        self.max_active_builds
    }

    pub fn artifact_limits(&self) -> ArtifactLimits {
        self.artifact
    }

    pub fn worker_state_limits(&self) -> WorkerStateLimits {
        self.worker
    }
}

/// An SSH target after all configuration and local-file checks have passed.
///
/// The identity and known-hosts files are intentionally represented only by
/// their paths. The files are never read by this module.
#[derive(Clone, PartialEq, Eq)]
pub struct SshTarget {
    name: String,
    host: String,
    port: u16,
    user: String,
    identity_file: PathBuf,
    known_hosts_file: PathBuf,
    worker_path: String,
    workspace_root: String,
    tools: BTreeMap<String, String>,
    environment: BTreeMap<String, String>,
    resources: ResourceLimits,
    compact: bool,
    port_explicit: bool,
}

/// Alias emphasizing that an `SshTarget` can only be obtained after validation.
pub type ValidatedSshTarget = SshTarget;

impl fmt::Debug for SshTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshTarget")
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("identity_file", &self.identity_file)
            .field("known_hosts_file", &self.known_hosts_file)
            .field("worker_path", &self.worker_path)
            .field("workspace_root", &self.workspace_root)
            .field("tools", &self.tools)
            .field("environment", &RedactedEnvironment(self.environment.len()))
            .field("resources", &self.resources)
            .finish()
    }
}

impl SshTarget {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn identity_file(&self) -> &Path {
        &self.identity_file
    }

    pub fn known_hosts_file(&self) -> &Path {
        &self.known_hosts_file
    }

    pub fn worker_path(&self) -> &str {
        &self.worker_path
    }

    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }

    pub fn tools(&self) -> &BTreeMap<String, String> {
        &self.tools
    }

    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub fn resources(&self) -> ResourceLimits {
        self.resources
    }

    pub fn compact_destination(&self) -> bool {
        self.compact
    }

    pub fn port_explicit(&self) -> bool {
        self.port_explicit
    }

    pub fn from_compact(name: String, destination: &str, workspace: String, resources: ResourceLimits) -> Result<Self, String> {
        let (user, host, port, port_explicit) = parse_ssh_destination(destination)?;
        validate_remote_path("workspace", &workspace, true)?;
        Ok(Self {
            name,
            host,
            port,
            user,
            identity_file: PathBuf::new(),
            known_hosts_file: PathBuf::new(),
            worker_path: FIXED_WORKER_PATH.to_string(),
            workspace_root: workspace,
            tools: BTreeMap::new(),
            environment: BTreeMap::new(),
            resources,
            compact: true,
            port_explicit,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectBinding {
    pub backend: BackendMode,
    pub target: Option<String>,
    pub artifacts: ArtifactPolicy,
}

impl ProjectBinding {
    pub fn backend(&self) -> BackendMode {
        self.backend
    }

    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    pub fn artifacts(&self) -> &ArtifactPolicy {
        &self.artifacts
    }
}

/// The result of resolving a canonical project path.
///
/// Loopback resolutions always contain `None` for `target`, even if a
/// loopback project entry happens to contain an unused target name.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedBackend {
    pub project_root: PathBuf,
    pub backend: BackendMode,
    pub target: Option<SshTarget>,
    pub artifacts: ArtifactPolicy,
}

impl fmt::Debug for ResolvedBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedBackend")
            .field("project_root", &self.project_root)
            .field("backend", &self.backend)
            .field("target", &self.target)
            .finish()
    }
}

impl ResolvedBackend {
    pub fn backend(&self) -> BackendMode {
        self.backend
    }

    pub fn mode(&self) -> BackendMode {
        self.backend
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn target(&self) -> Option<&SshTarget> {
        self.target.as_ref()
    }

    pub fn artifacts(&self) -> &ArtifactPolicy {
        &self.artifacts
    }
}

/// Configuration loaded from the host's remote-targets file.
pub struct RemoteTargetConfig {
    source_path: PathBuf,
    targets: BTreeMap<String, SshTarget>,
    projects: BTreeMap<PathBuf, ProjectBinding>,
}

pub type RemoteConfig = RemoteTargetConfig;

impl fmt::Debug for RemoteTargetConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteTargetConfig")
            .field("source_path", &self.source_path)
            .field("targets", &self.targets)
            .field("projects", &self.projects)
            .finish()
    }
}

impl RemoteTargetConfig {
    pub fn load_default() -> Result<Self, String> {
        let helper = ConfigPathHelper::from_environment()?;
        Self::load_default_with(&helper)
    }

    pub fn load_default_with(helper: &ConfigPathHelper) -> Result<Self, String> {
        Self::load_from(helper.config_path()?)
    }

    pub fn load_default_with_path_helper(helper: &ConfigPathHelper) -> Result<Self, String> {
        Self::load_default_with(helper)
    }

    pub fn load_default_with_paths(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Result<Self, String> {
        Self::load_default_with(&ConfigPathHelper::new(xdg_config_home, home))
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|error| format!("failed to read remote target config {}: {error}", path.display()))?;
        let raw: RawConfig =
            serde_yaml::from_str(&contents).map_err(|error| format!("failed to parse remote target config {}: {error}", path.display()))?;
        Self::from_raw(raw, path.to_path_buf())
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn targets(&self) -> &BTreeMap<String, SshTarget> {
        &self.targets
    }

    pub fn target(&self, name: &str) -> Option<&SshTarget> {
        self.targets.get(name)
    }

    pub fn ssh_target(&self, name: &str) -> Result<&SshTarget, String> {
        self.targets.get(name).ok_or_else(|| format!("unknown SSH target '{name}'"))
    }

    pub fn projects(&self) -> &BTreeMap<PathBuf, ProjectBinding> {
        &self.projects
    }

    pub fn binding_for_project(&self, project: impl AsRef<Path>) -> Result<&ProjectBinding, String> {
        let canonical = canonical_project_for_resolution(project.as_ref())?;
        self.projects.get(&canonical).ok_or_else(|| format!("project has no remote backend binding: {}", canonical.display()))
    }

    pub fn resolve_for_project(&self, project: impl AsRef<Path>) -> Result<ResolvedBackend, String> {
        let canonical = canonical_project_for_resolution(project.as_ref())?;
        let binding = self.projects.get(&canonical).ok_or_else(|| format!("project has no remote backend binding: {}", canonical.display()))?;

        match binding.backend {
            BackendMode::Loopback => {
                Ok(ResolvedBackend { project_root: canonical, backend: BackendMode::Loopback, target: None, artifacts: binding.artifacts.clone() })
            }
            BackendMode::Ssh => {
                let target_name = binding.target.as_deref().ok_or_else(|| "SSH project binding is missing a target".to_string())?;
                let target = self.targets.get(target_name).ok_or_else(|| format!("unknown SSH target '{target_name}'"))?;
                Ok(ResolvedBackend {
                    project_root: canonical,
                    backend: BackendMode::Ssh,
                    target: Some(target.clone()),
                    artifacts: binding.artifacts.clone(),
                })
            }
        }
    }

    fn from_raw(raw: RawConfig, source_path: PathBuf) -> Result<Self, String> {
        if raw.version != CONFIG_VERSION {
            return Err(format!("unsupported remote target config version {}; expected {}", raw.version, CONFIG_VERSION));
        }

        let mut targets = BTreeMap::new();
        for (name, target) in raw.targets.0 {
            validate_name("target name", &name)?;
            let validated = validate_target(name.clone(), target)?;
            if targets.insert(name.clone(), validated).is_some() {
                return Err(format!("duplicate target name '{name}'"));
            }
        }

        let mut projects = BTreeMap::new();
        for (project, binding) in raw.projects.0 {
            let canonical = validate_project_binding_path(&project)?;
            let artifacts = validate_project_binding(&binding)?;
            if binding.backend == BackendMode::Ssh {
                let Some(target_name) = binding.target.as_deref() else {
                    return Err("SSH project binding requires a target".to_string());
                };
                let target = targets.get(target_name).ok_or_else(|| format!("unknown SSH target '{target_name}'"))?;
                artifacts.validate_limits(target.resources().artifact_limits())?;
            } else {
                artifacts.validate_limits(ArtifactLimits::default())?;
            }
            if projects.insert(canonical, binding.into_public(artifacts)).is_some() {
                return Err("duplicate project binding after canonicalization".to_string());
            }
        }

        Ok(Self { source_path, targets, projects })
    }
}

/// Inputs used to resolve the host-level default configuration path.
///
/// Keeping environment lookup outside `config_path` makes default-path
/// behavior deterministic in callers and unit tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPathHelper {
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl ConfigPathHelper {
    pub fn new(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Self {
        Self { xdg_config_home: nonempty_path(xdg_config_home), home: nonempty_path(home) }
    }

    pub fn from_paths(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Self {
        Self::new(xdg_config_home.map(Path::to_path_buf), home.map(Path::to_path_buf))
    }

    pub fn from_environment() -> Result<Self, String> {
        Ok(Self::new(env::var_os("XDG_CONFIG_HOME").map(PathBuf::from), env::var_os("HOME").map(PathBuf::from)))
    }

    pub fn xdg_config_home(&self) -> Option<&Path> {
        self.xdg_config_home.as_deref()
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub fn config_path(&self) -> Result<PathBuf, String> {
        let base = match (&self.xdg_config_home, &self.home) {
            (Some(xdg), _) => xdg.clone(),
            (None, Some(home)) => home.join(".config"),
            (None, None) => return Err("cannot resolve remote target config path: HOME is not set".to_string()),
        };

        validate_config_base(&base)?;
        Ok(base.join(CONFIG_DIRECTORY_NAME).join(CONFIG_FILE_NAME))
    }

    pub fn default_path(&self) -> Result<PathBuf, String> {
        self.config_path()
    }
}

pub fn default_config_path() -> Result<PathBuf, String> {
    ConfigPathHelper::from_environment()?.config_path()
}

pub fn default_config_path_with(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, String> {
    ConfigPathHelper::from_paths(xdg_config_home, home).config_path()
}

#[derive(Debug)]
struct UniqueMap<K, V>(BTreeMap<K, V>);

impl<K, V> Default for UniqueMap<K, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<'de, K, V> Deserialize<'de> for UniqueMap<K, V>
where
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
    }
}

struct UniqueMapVisitor<K, V>(PhantomData<(K, V)>);

impl<'de, K, V> Visitor<'de> for UniqueMapVisitor<K, V>
where
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    type Value = UniqueMap<K, V>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a YAML mapping with unique keys")
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = BTreeMap::new();
        while let Some(key) = access.next_key::<K>()? {
            if entries.contains_key(&key) {
                return Err(de::Error::custom("duplicate YAML map key"));
            }
            let value = access.next_value::<V>()?;
            entries.insert(key, value);
        }
        Ok(UniqueMap(entries))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u64,
    targets: UniqueMap<String, RawTarget>,
    projects: UniqueMap<String, RawProjectBinding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    transport: String,
    host: String,
    port: u64,
    user: String,
    #[serde(rename = "identity-file", alias = "identity_file")]
    identity_file: String,
    #[serde(rename = "known-hosts-file", alias = "known_hosts_file")]
    known_hosts_file: String,
    #[serde(rename = "worker-path", alias = "worker_path")]
    worker_path: String,
    #[serde(rename = "workspace-root", alias = "workspace_root")]
    workspace_root: String,
    #[serde(default)]
    tools: UniqueMap<String, String>,
    #[serde(default)]
    environment: UniqueMap<String, String>,
    resources: RawResources,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectBinding {
    backend: BackendMode,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    artifacts: Option<RawArtifacts>,
}

impl RawProjectBinding {
    fn into_public(self, artifacts: ArtifactPolicy) -> ProjectBinding {
        ProjectBinding { backend: self.backend, target: self.target, artifacts }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArtifacts {
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResources {
    #[serde(
        rename = "connect-timeout-seconds",
        alias = "connect-timeout",
        alias = "connect_timeout_seconds",
        alias = "connect_timeout",
        alias = "connect"
    )]
    connect_timeout: RawQuantity,
    #[serde(rename = "sync-timeout-seconds", alias = "sync-timeout", alias = "sync_timeout_seconds", alias = "sync_timeout", alias = "sync")]
    sync_timeout: RawQuantity,
    #[serde(rename = "build-timeout-seconds", alias = "build-timeout", alias = "build_timeout_seconds", alias = "build_timeout", alias = "build")]
    build_timeout: RawQuantity,
    #[serde(rename = "max-output-bytes", alias = "max-output", alias = "max_output_bytes", alias = "max_output")]
    max_output: RawQuantity,
    #[serde(default, rename = "idle-output-timeout-seconds", alias = "idle-output-timeout", alias = "idle_output_timeout")]
    idle_output_timeout: Option<RawQuantity>,
    #[serde(default, rename = "cleanup-timeout-seconds", alias = "cleanup-timeout", alias = "cleanup_timeout")]
    cleanup_timeout: Option<RawQuantity>,
    #[serde(default, rename = "artifact-timeout-seconds", alias = "artifact-timeout", alias = "artifact_timeout")]
    artifact_timeout: Option<RawQuantity>,
    #[serde(default, rename = "max-artifact-bytes", alias = "max-artifact-bytes-per-file", alias = "max_artifact_bytes")]
    max_artifact_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-artifact-total-bytes", alias = "max_artifact_total_bytes")]
    max_artifact_total_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-artifact-entries", alias = "max_artifact_entries")]
    max_artifact_entries: Option<u64>,
    #[serde(default, rename = "max-worker-uploads", alias = "max_worker_uploads")]
    max_worker_uploads: Option<u64>,
    #[serde(default, rename = "max-worker-upload-bytes", alias = "max_worker_upload_bytes")]
    max_worker_upload_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-worker-jobs", alias = "max_worker_jobs")]
    max_worker_jobs: Option<u64>,
    #[serde(default, rename = "max-worker-job-bytes", alias = "max_worker_job_bytes")]
    max_worker_job_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-worker-artifact-spools", alias = "max_worker_artifact_spools")]
    max_worker_artifact_spools: Option<u64>,
    #[serde(default, rename = "max-worker-artifact-spool-bytes", alias = "max_worker_artifact_spool_bytes")]
    max_worker_artifact_spool_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-worker-state-entries", alias = "max_worker_state_entries")]
    max_worker_state_entries: Option<u64>,
    #[serde(default, rename = "max-active-builds", alias = "max_active_builds")]
    max_active_builds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
#[derive(Debug)]
enum RawQuantity {
    Integer(u64),
    Text(String),
}

fn validate_target(name: String, raw: RawTarget) -> Result<SshTarget, String> {
    if raw.transport != "ssh" {
        return Err("remote target transport must be 'ssh'".to_string());
    }

    validate_hostname(&raw.host)?;
    if raw.port == 0 || raw.port > u16::MAX as u64 {
        return Err("remote target port must be between 1 and 65535".to_string());
    }
    validate_username(&raw.user)?;

    let identity_file = validate_local_absolute_path("identity-file", &raw.identity_file)?;
    validate_identity_file(&identity_file)?;

    let known_hosts_file = validate_local_absolute_path("known-hosts-file", &raw.known_hosts_file)?;
    validate_known_hosts_file(&known_hosts_file)?;

    let worker_path = validate_remote_path("worker-path", &raw.worker_path, false)?;
    let workspace_root = validate_remote_path("workspace-root", &raw.workspace_root, true)?;

    let mut tools = BTreeMap::new();
    for (identity, path) in raw.tools.0 {
        validate_tool_identity(&identity)?;
        let path = validate_remote_tool_path(&path)?;
        if tools.insert(identity.clone(), path).is_some() {
            return Err(format!("duplicate tool identity '{identity}'"));
        }
    }

    let mut environment = BTreeMap::new();
    for (name, value) in raw.environment.0 {
        validate_environment_name(&name)?;
        validate_environment_value(&value)?;
        if environment.insert(name.clone(), value).is_some() {
            return Err(format!("duplicate environment name '{name}'"));
        }
    }

    let resources = validate_resources(raw.resources)?;

    Ok(SshTarget {
        name,
        host: raw.host,
        port: raw.port as u16,
        user: raw.user,
        identity_file,
        known_hosts_file,
        worker_path,
        workspace_root,
        tools,
        environment,
        resources,
        compact: false,
        port_explicit: true,
    })
}

fn validate_project_binding(binding: &RawProjectBinding) -> Result<ArtifactPolicy, String> {
    if let Some(target) = &binding.target {
        validate_name("project target name", target)?;
    }
    if binding.backend == BackendMode::Ssh && binding.target.is_none() {
        return Err("SSH project binding requires a target".to_string());
    }
    binding.artifacts.as_ref().map_or_else(|| Ok(ArtifactPolicy::default()), |artifacts| ArtifactPolicy::new(artifacts.paths.clone()))
}

fn validate_resources(raw: RawResources) -> Result<ResourceLimits, String> {
    let connect_timeout = parse_duration("connect-timeout", raw.connect_timeout)?;
    let sync_timeout = parse_duration("sync-timeout", raw.sync_timeout)?;
    let build_timeout = parse_duration("build-timeout", raw.build_timeout)?;
    validate_lifecycle_duration("connect-timeout", connect_timeout)?;
    validate_lifecycle_duration("sync-timeout", sync_timeout)?;
    validate_lifecycle_duration("build-timeout", build_timeout)?;
    let max_output_bytes = parse_size("max-output", raw.max_output)?;
    let max_active_builds = parse_count("max-active-builds", raw.max_active_builds, 1)?;
    if max_active_builds == 0 || max_active_builds > 64 {
        return Err("max-active-builds must be between 1 and 64".to_string());
    }
    let defaults = ArtifactLimits::default();
    let idle_output_timeout =
        raw.idle_output_timeout.map_or(Ok(Duration::from_secs(5 * 60)), |value| parse_duration("idle-output-timeout", value))?;
    let cleanup_timeout = raw.cleanup_timeout.map_or(Ok(Duration::from_secs(5)), |value| parse_duration("cleanup-timeout", value))?;
    validate_lifecycle_duration("idle-output-timeout", idle_output_timeout)?;
    validate_lifecycle_duration("cleanup-timeout", cleanup_timeout)?;
    let artifact_timeout = raw.artifact_timeout.map_or(Ok(defaults.timeout), |value| parse_duration("artifact-timeout", value))?;
    let max_artifact_bytes = raw.max_artifact_bytes.map_or(Ok(defaults.max_file_bytes), |value| parse_size("max-artifact-bytes", value))?;
    let max_artifact_total_bytes =
        raw.max_artifact_total_bytes.map_or(Ok(defaults.max_total_bytes), |value| parse_size("max-artifact-total-bytes", value))?;
    let max_artifact_entries = raw
        .max_artifact_entries
        .map_or(Ok(defaults.max_entries), |value| usize::try_from(value).map_err(|_| "max-artifact-entries is too large".to_string()))?;
    let artifact = ArtifactLimits::new(artifact_timeout, max_artifact_entries, max_artifact_bytes, max_artifact_total_bytes)?;
    let worker_defaults = WorkerStateLimits::default();
    let worker = WorkerStateLimits::new(
        parse_count("max-worker-uploads", raw.max_worker_uploads, worker_defaults.max_uploads)?,
        raw.max_worker_upload_bytes.map_or(Ok(worker_defaults.max_upload_bytes), |value| parse_size("max-worker-upload-bytes", value))?,
        parse_count("max-worker-jobs", raw.max_worker_jobs, worker_defaults.max_jobs)?,
        raw.max_worker_job_bytes.map_or(Ok(worker_defaults.max_job_bytes), |value| parse_size("max-worker-job-bytes", value))?,
        parse_count("max-worker-artifact-spools", raw.max_worker_artifact_spools, worker_defaults.max_artifact_spools)?,
        raw.max_worker_artifact_spool_bytes
            .map_or(Ok(worker_defaults.max_artifact_spool_bytes), |value| parse_size("max-worker-artifact-spool-bytes", value))?,
        parse_count("max-worker-state-entries", raw.max_worker_state_entries, worker_defaults.max_state_entries)?,
    )?;
    Ok(ResourceLimits {
        connect_timeout,
        sync_timeout,
        build_timeout,
        idle_output_timeout,
        cleanup_timeout,
        max_output_bytes,
        max_active_builds,
        artifact,
        worker,
    })
}

fn parse_count(field: &str, value: Option<u64>, default: usize) -> Result<usize, String> {
    match value {
        Some(value) => usize::try_from(value).map_err(|_| format!("{field} is too large")),
        None => Ok(default),
    }
}

fn validate_lifecycle_duration(field: &str, value: Duration) -> Result<(), String> {
    if value.is_zero() || value > Duration::from_secs(24 * 60 * 60) {
        return Err(format!("{field} must be between 1 second and 24 hours"));
    }
    Ok(())
}

fn parse_duration(field: &str, quantity: RawQuantity) -> Result<Duration, String> {
    let (number, suffix) = quantity_parts(field, quantity)?;
    let multiplier_nanos = match suffix.to_ascii_lowercase().as_str() {
        "" | "s" => 1_000_000_000u64,
        "ms" => 1_000_000,
        "us" => 1_000,
        "ns" => 1,
        "m" => 60 * 1_000_000_000,
        "h" => 60 * 60 * 1_000_000_000,
        "d" => 24 * 60 * 60 * 1_000_000_000,
        _ => return Err(format!("{field} has an unsupported duration unit")),
    };
    let nanos = number.checked_mul(multiplier_nanos).ok_or_else(|| format!("{field} is too large"))?;
    if nanos == 0 {
        return Err(format!("{field} must be positive"));
    }
    let seconds = nanos / 1_000_000_000;
    let subsecond_nanos = (nanos % 1_000_000_000) as u32;
    Ok(Duration::new(seconds, subsecond_nanos))
}

fn parse_size(field: &str, quantity: RawQuantity) -> Result<u64, String> {
    let (number, suffix) = quantity_parts(field, quantity)?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1u64,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("{field} has an unsupported size unit")),
    };
    let bytes = number.checked_mul(multiplier).ok_or_else(|| format!("{field} is too large"))?;
    if bytes == 0 {
        return Err(format!("{field} must be positive"));
    }
    Ok(bytes)
}

fn quantity_parts(field: &str, quantity: RawQuantity) -> Result<(u64, String), String> {
    let text = match quantity {
        RawQuantity::Integer(number) => return Ok((number, String::new())),
        RawQuantity::Text(text) => text,
    };
    let text = text.trim();
    let split = text.find(|character: char| !character.is_ascii_digit()).unwrap_or(text.len());
    if split == 0 {
        return Err(format!("{field} must start with a positive integer"));
    }
    let number = text[..split].parse::<u64>().map_err(|_| format!("{field} is too large"))?;
    Ok((number, text[split..].to_string()))
}

fn validate_name(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_NAME_BYTES || value == "." || value == ".." {
        return Err(format!("{field} is invalid"));
    }
    let mut characters = value.bytes();
    let Some(first) = characters.next() else {
        return Err(format!("{field} is invalid"));
    };
    if !first.is_ascii_alphanumeric() {
        return Err(format!("{field} is invalid"));
    }
    if !characters.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+')) {
        return Err(format!("{field} is invalid"));
    }
    Ok(())
}

fn validate_hostname(host: &str) -> Result<(), String> {
    if host.is_empty() || host.len() > MAX_HOST_BYTES || !host.is_ascii() || host.chars().any(char::is_whitespace) {
        return Err("remote target host has unsafe hostname syntax".to_string());
    }
    if IpAddr::from_str(host).is_ok() {
        return Ok(());
    }
    if host.ends_with('.') {
        return Err("remote target host has unsafe hostname syntax".to_string());
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("remote target host has unsafe hostname syntax".to_string());
        }
        let bytes = label.as_bytes();
        if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
            return Err("remote target host has unsafe hostname syntax".to_string());
        }
        if !bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-') {
            return Err("remote target host has unsafe hostname syntax".to_string());
        }
    }
    Ok(())
}

fn validate_username(user: &str) -> Result<(), String> {
    if user.is_empty() || user.len() > MAX_USER_BYTES || !user.is_ascii() {
        return Err("remote target username is invalid".to_string());
    }
    let bytes = user.as_bytes();
    if !bytes[0].is_ascii_alphabetic() && bytes[0] != b'_' {
        return Err("remote target username is invalid".to_string());
    }
    if !bytes[1..].iter().all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.')) {
        return Err("remote target username is invalid".to_string());
    }
    Ok(())
}

fn validate_local_absolute_path(field: &str, value: &str) -> Result<PathBuf, String> {
    if value.is_empty() || value.len() > MAX_CONFIG_PATH_BYTES || value.chars().any(char::is_control) {
        return Err(format!("{field} is invalid"));
    }
    let path = Path::new(value);
    validate_absolute_no_parent_path(field, path)?;
    Ok(path.to_path_buf())
}

fn validate_absolute_no_parent_path(field: &str, path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{field} must be absolute"));
    }
    if path.components().any(|component| matches!(component, Component::ParentDir | Component::CurDir)) {
        return Err(format!("{field} must not contain '.' or '..' path components"));
    }
    Ok(())
}

fn validate_remote_path(field: &str, value: &str, allow_root: bool) -> Result<String, String> {
    if value.is_empty() || value.len() > MAX_REMOTE_PATH_BYTES || !value.is_ascii() || value.chars().any(char::is_whitespace) {
        return Err(format!("{field} has invalid path syntax"));
    }
    if !value.starts_with('/') || value.contains("//") || (value != "/" && value.ends_with('/')) {
        return Err(format!("{field} must be an absolute normalized path"));
    }
    let path = Path::new(value);
    validate_absolute_no_parent_path(field, path)?;
    if !allow_root && value == "/" {
        return Err(format!("{field} must name a worker"));
    }
    if value.bytes().any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+' | b'@' | b'%' | b'~')) {
        return Err(format!("{field} has invalid path syntax"));
    }
    Ok(value.to_string())
}

fn validate_remote_tool_path(value: &str) -> Result<String, String> {
    if value.len() > MAX_TOOL_PATH_BYTES {
        return Err("tool path is too long".to_string());
    }
    validate_remote_path("tool path", value, false)
}

fn validate_tool_identity(identity: &str) -> Result<(), String> {
    validate_name("tool identity", identity)
}

fn validate_environment_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_ENV_NAME_BYTES {
        return Err("environment name is invalid".to_string());
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_alphabetic() && bytes[0] != b'_' {
        return Err("environment name is invalid".to_string());
    }
    if !bytes[1..].iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
        return Err("environment name is invalid".to_string());
    }
    Ok(())
}

fn validate_environment_value(value: &str) -> Result<(), String> {
    if value.len() > MAX_ENV_VALUE_BYTES || value.chars().any(char::is_control) {
        return Err("environment value is invalid".to_string());
    }
    Ok(())
}

fn validate_project_binding_path(value: &str) -> Result<PathBuf, String> {
    let path = validate_local_absolute_path("project binding", value)?;
    let canonical = fs::canonicalize(&path).map_err(|_| "project binding must refer to an existing canonical project path".to_string())?;
    if canonical != path {
        return Err("project binding must use the canonical project path".to_string());
    }
    let metadata = fs::metadata(&canonical).map_err(|_| "project binding cannot be inspected".to_string())?;
    if !metadata.is_dir() {
        return Err("project binding must refer to a directory".to_string());
    }
    Ok(canonical)
}

fn canonical_project_for_resolution(path: &Path) -> Result<PathBuf, String> {
    validate_absolute_no_parent_path("project path", path)?;
    let canonical = fs::canonicalize(path).map_err(|_| "project path cannot be canonicalized".to_string())?;
    let metadata = fs::metadata(&canonical).map_err(|_| "project path cannot be inspected".to_string())?;
    if !metadata.is_dir() {
        return Err("project path must refer to a directory".to_string());
    }
    Ok(canonical)
}

fn validate_identity_file(path: &Path) -> Result<(), String> {
    let metadata = regular_file_metadata(path, "identity-file")?;
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode();
        if mode & 0o7777 != 0o400 && mode & 0o7777 != 0o600 {
            return Err("identity-file has insecure permissions".to_string());
        }
    }
    File::open(path).map_err(|_| "identity-file is not readable".to_string())?;
    Ok(())
}

fn validate_known_hosts_file(path: &Path) -> Result<(), String> {
    let metadata = regular_file_metadata(path, "known-hosts-file")?;
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o444 == 0 {
        return Err("known-hosts-file is not readable".to_string());
    }
    File::open(path).map_err(|_| "known-hosts-file is not readable".to_string())?;
    Ok(())
}

fn regular_file_metadata(path: &Path, field: &str) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| format!("{field} does not exist"))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{field} must be a regular file"));
    }
    Ok(metadata)
}

fn validate_config_base(path: &Path) -> Result<(), String> {
    validate_absolute_no_parent_path("configuration directory", path)?;
    if path.as_os_str().len() > MAX_CONFIG_PATH_BYTES {
        return Err("configuration directory path is too long".to_string());
    }
    Ok(())
}

fn nonempty_path(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| !path.as_os_str().is_empty())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildTargetSummary {
    label: String,
    workspace: String,
    local: bool,
}

impl BuildTargetSummary {
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub fn is_local(&self) -> bool {
        self.local
    }
}

#[derive(Clone, Debug)]
pub struct RemoteBuildTarget {
    summary: BuildTargetSummary,
    target: SshTarget,
    project: RemoteSection,
    artifact_policy: ArtifactPolicy,
}

impl RemoteBuildTarget {
    pub fn summary(&self) -> &BuildTargetSummary {
        &self.summary
    }

    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    pub fn project(&self) -> &RemoteSection {
        &self.project
    }

    pub fn artifact_policy(&self) -> &ArtifactPolicy {
        &self.artifact_policy
    }

    pub fn tool_policies(&self) -> Vec<(String, RemoteToolPolicy)> {
        self.project
            .tools
            .iter()
            .map(|tool| {
                let command = tool.command.clone().unwrap_or_else(|| tool.name.clone());
                (tool.name.clone(), RemoteToolPolicy::new(tool.allow_args).with_command(command))
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct BuildTargetCatalog {
    project_root: PathBuf,
    base_project: ProjectConfig,
    summaries: Vec<BuildTargetSummary>,
    remotes: BTreeMap<String, RemoteBuildTarget>,
}

impl BuildTargetCatalog {
    pub fn localhost_only(project_root: PathBuf, base_project: ProjectConfig) -> Result<Self, String> {
        validate_remote_section(&base_project.project.remote)?;
        Ok(Self {
            summaries: vec![BuildTargetSummary { label: "localhost".to_string(), workspace: project_root.display().to_string(), local: true }],
            project_root,
            base_project,
            remotes: BTreeMap::new(),
        })
    }

    pub fn load_optional(project_root: &Path, base_project: &ProjectConfig) -> Result<Option<Self>, String> {
        let path = project_root.join(".bunkerbox").join(REMOTE_PROJECT_CONFIG_FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let raw: RawRemoteProjectConfig = serde_yaml::from_str(&contents).map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
        Self::from_raw(project_root.to_path_buf(), base_project.clone(), raw).map(Some)
    }

    fn from_raw(project_root: PathBuf, base_project: ProjectConfig, raw: RawRemoteProjectConfig) -> Result<Self, String> {
        validate_remote_section(&base_project.project.remote)?;
        let mut remotes = BTreeMap::new();
        let mut summaries = vec![BuildTargetSummary { label: "localhost".to_string(), workspace: project_root.display().to_string(), local: true }];
        for (label, target) in raw.targets.0 {
            validate_name("target label", &label)?;
            if label == "localhost" {
                return Err("remote target label 'localhost' is reserved".to_string());
            }
            let project = merge_remote_overlay(&base_project.project.remote, target.project.as_ref())?;
            validate_remote_section(&project)?;
            let resources = validate_compact_resources(target.resources)?;
            let ssh_target = SshTarget::from_compact(label.clone(), &target.ssh, target.workspace, resources)?;
            let artifact_policy = ArtifactPolicy::new(project.artifacts.clone())?;
            artifact_policy.validate_limits(resources.artifact_limits())?;
            let summary = BuildTargetSummary { label: label.clone(), workspace: ssh_target.workspace_root().to_string(), local: false };
            let remote = RemoteBuildTarget { summary: summary.clone(), target: ssh_target, project, artifact_policy };
            if remotes.insert(label.clone(), remote).is_some() {
                return Err(format!("duplicate target label: {label}"));
            }
            summaries.push(summary);
        }
        Ok(Self { project_root, base_project, summaries, remotes })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn base_project(&self) -> &ProjectConfig {
        &self.base_project
    }

    pub fn summaries(&self) -> &[BuildTargetSummary] {
        &self.summaries
    }

    pub fn remote(&self, label: &str) -> Option<&RemoteBuildTarget> {
        self.remotes.get(label)
    }

    pub fn remote_targets(&self) -> impl Iterator<Item = &RemoteBuildTarget> {
        self.remotes.values()
    }

    pub fn wrapper_names(&self) -> Vec<String> {
        let mut names = BTreeMap::new();
        for tool in &self.base_project.project.remote.tools {
            names.insert(tool.name.clone(), ());
        }
        for target in self.remotes.values() {
            for tool in &target.project.tools {
                names.insert(tool.name.clone(), ());
            }
        }
        names.into_keys().collect()
    }

    pub fn environment_names(&self) -> Vec<String> {
        let mut names = BTreeMap::new();
        for name in &self.base_project.project.remote.environment {
            names.insert(name.clone(), ());
        }
        for target in self.remotes.values() {
            for name in &target.project.environment {
                names.insert(name.clone(), ());
            }
        }
        names.into_keys().collect()
    }
}

#[derive(Clone)]
pub struct ActiveBuildTarget {
    selected: Arc<Mutex<String>>,
}

impl ActiveBuildTarget {
    pub fn new() -> Self {
        Self::with_label("localhost")
    }

    pub(crate) fn with_label(label: impl Into<String>) -> Self {
        Self { selected: Arc::new(Mutex::new(label.into())) }
    }

    pub fn current(&self) -> String {
        self.selected.lock().map(|value| value.clone()).unwrap_or_else(|_| "localhost".to_string())
    }

    pub fn select(&self, catalog: &BuildTargetCatalog, label: &str) -> Result<(), String> {
        if !catalog.summaries.iter().any(|summary| summary.label == label) {
            return Err(format!("unknown build target: {label}"));
        }
        let mut selected = self.selected.lock().map_err(|_| "active build target lock poisoned".to_string())?;
        *selected = label.to_string();
        Ok(())
    }
}

impl Default for ActiveBuildTarget {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRemoteProjectConfig {
    #[serde(default)]
    targets: UniqueMap<String, RawCompactTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompactTarget {
    ssh: String,
    workspace: String,
    #[serde(default)]
    project: Option<RawTargetProject>,
    #[serde(default)]
    resources: Option<RawTargetResources>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTargetProject {
    #[serde(default)]
    remote: Option<RawRemoteOverlay>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRemoteOverlay {
    #[serde(default)]
    exclude: Option<Vec<String>>,
    #[serde(default)]
    environment: Option<Vec<String>>,
    #[serde(default)]
    tools: Option<Vec<crate::cfg::RemoteToolSpec>>,
    #[serde(default)]
    artifacts: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTargetResources {
    #[serde(
        default,
        rename = "connect-timeout-seconds",
        alias = "connect-timeout",
        alias = "connect_timeout_seconds",
        alias = "connect_timeout",
        alias = "connect"
    )]
    connect_timeout: Option<RawQuantity>,
    #[serde(
        default,
        rename = "sync-timeout-seconds",
        alias = "sync-timeout",
        alias = "sync_timeout_seconds",
        alias = "sync_timeout",
        alias = "sync"
    )]
    sync_timeout: Option<RawQuantity>,
    #[serde(
        default,
        rename = "build-timeout-seconds",
        alias = "build-timeout",
        alias = "build_timeout_seconds",
        alias = "build_timeout",
        alias = "build"
    )]
    build_timeout: Option<RawQuantity>,
    #[serde(default, rename = "max-output-bytes", alias = "max-output", alias = "max_output_bytes", alias = "max_output")]
    max_output: Option<RawQuantity>,
    #[serde(default, rename = "idle-output-timeout-seconds", alias = "idle-output-timeout", alias = "idle_output_timeout")]
    idle_output_timeout: Option<RawQuantity>,
    #[serde(default, rename = "cleanup-timeout-seconds", alias = "cleanup-timeout", alias = "cleanup_timeout")]
    cleanup_timeout: Option<RawQuantity>,
    #[serde(default, rename = "artifact-timeout-seconds", alias = "artifact-timeout", alias = "artifact_timeout")]
    artifact_timeout: Option<RawQuantity>,
    #[serde(default, rename = "max-artifact-bytes", alias = "max-artifact-bytes-per-file", alias = "max_artifact_bytes")]
    max_artifact_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-artifact-total-bytes", alias = "max_artifact_total_bytes")]
    max_artifact_total_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-artifact-entries", alias = "max_artifact_entries")]
    max_artifact_entries: Option<u64>,
    #[serde(default, rename = "max-worker-uploads", alias = "max_worker_uploads")]
    max_worker_uploads: Option<u64>,
    #[serde(default, rename = "max-worker-upload-bytes", alias = "max_worker_upload_bytes")]
    max_worker_upload_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-worker-jobs", alias = "max_worker_jobs")]
    max_worker_jobs: Option<u64>,
    #[serde(default, rename = "max-worker-job-bytes", alias = "max_worker_job_bytes")]
    max_worker_job_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-worker-artifact-spools", alias = "max_worker_artifact_spools")]
    max_worker_artifact_spools: Option<u64>,
    #[serde(default, rename = "max-worker-artifact-spool-bytes", alias = "max_worker_artifact_spool_bytes")]
    max_worker_artifact_spool_bytes: Option<RawQuantity>,
    #[serde(default, rename = "max-worker-state-entries", alias = "max_worker_state_entries")]
    max_worker_state_entries: Option<u64>,
    #[serde(default, rename = "max-active-builds", alias = "max_active_builds")]
    max_active_builds: Option<u64>,
}

fn merge_remote_overlay(base: &RemoteSection, target: Option<&RawTargetProject>) -> Result<RemoteSection, String> {
    let Some(target) = target.and_then(|project| project.remote.as_ref()) else { return Ok(base.clone()) };
    let mut merged = base.clone();
    if let Some(exclude) = &target.exclude {
        merged.exclude = exclude.clone();
    }
    if let Some(environment) = &target.environment {
        merged.environment = environment.clone();
    }
    if let Some(tools) = &target.tools {
        merged.tools = tools.clone();
    }
    if let Some(artifacts) = &target.artifacts {
        merged.artifacts = artifacts.clone();
    }
    Ok(merged)
}

fn validate_remote_section(section: &RemoteSection) -> Result<(), String> {
    RemoteEnvironmentPolicy::from_names(section.environment.clone())?;
    crate::snapshot::SnapshotExclusionPolicy::from_patterns(section.exclude.clone())?;
    ArtifactPolicy::new(section.artifacts.clone())?;
    let mut names = BTreeMap::new();
    for tool in &section.tools {
        crate::remote::validate_remote_wrapper_name(tool.name.clone())?;
        if let Some(command) = &tool.command {
            crate::remote::validate_remote_wrapper_name(command.clone())?;
        }
        if names.insert(tool.name.clone(), ()).is_some() {
            return Err(format!("duplicate remote tool: {}", tool.name));
        }
    }
    Ok(())
}

fn validate_compact_resources(raw: Option<RawTargetResources>) -> Result<ResourceLimits, String> {
    let defaults = crate::remote::RemoteResourcePolicy::default();
    let artifact_defaults = ArtifactLimits::default();
    let worker_defaults = WorkerStateLimits::default();
    let raw = raw.unwrap_or_default();
    let connect_timeout = raw.connect_timeout.map_or(Ok(defaults.sync_timeout), |value| parse_duration("connect-timeout", value))?;
    let sync_timeout = raw.sync_timeout.map_or(Ok(defaults.sync_timeout), |value| parse_duration("sync-timeout", value))?;
    let build_timeout = raw.build_timeout.map_or(Ok(defaults.build_timeout), |value| parse_duration("build-timeout", value))?;
    let idle_output_timeout =
        raw.idle_output_timeout.map_or(Ok(defaults.idle_output_timeout), |value| parse_duration("idle-output-timeout", value))?;
    let cleanup_timeout = raw.cleanup_timeout.map_or(Ok(defaults.cleanup_timeout), |value| parse_duration("cleanup-timeout", value))?;
    validate_lifecycle_duration("connect-timeout", connect_timeout)?;
    validate_lifecycle_duration("sync-timeout", sync_timeout)?;
    validate_lifecycle_duration("build-timeout", build_timeout)?;
    validate_lifecycle_duration("idle-output-timeout", idle_output_timeout)?;
    validate_lifecycle_duration("cleanup-timeout", cleanup_timeout)?;
    let max_output_bytes = raw.max_output.map_or(Ok(defaults.max_output_bytes), |value| parse_size("max-output", value))?;
    let max_active_builds = parse_count("max-active-builds", raw.max_active_builds, 1)?;
    if max_active_builds == 0 || max_active_builds > 64 {
        return Err("max-active-builds must be between 1 and 64".to_string());
    }
    let artifact_timeout = raw.artifact_timeout.map_or(Ok(artifact_defaults.timeout), |value| parse_duration("artifact-timeout", value))?;
    let max_artifact_bytes = raw.max_artifact_bytes.map_or(Ok(artifact_defaults.max_file_bytes), |value| parse_size("max-artifact-bytes", value))?;
    let max_artifact_total_bytes =
        raw.max_artifact_total_bytes.map_or(Ok(artifact_defaults.max_total_bytes), |value| parse_size("max-artifact-total-bytes", value))?;
    let max_artifact_entries = raw
        .max_artifact_entries
        .map_or(Ok(artifact_defaults.max_entries), |value| usize::try_from(value).map_err(|_| "max-artifact-entries is too large".to_string()))?;
    let artifact = ArtifactLimits::new(artifact_timeout, max_artifact_entries, max_artifact_bytes, max_artifact_total_bytes)?;
    let worker = WorkerStateLimits::new(
        parse_count("max-worker-uploads", raw.max_worker_uploads, worker_defaults.max_uploads)?,
        raw.max_worker_upload_bytes.map_or(Ok(worker_defaults.max_upload_bytes), |value| parse_size("max-worker-upload-bytes", value))?,
        parse_count("max-worker-jobs", raw.max_worker_jobs, worker_defaults.max_jobs)?,
        raw.max_worker_job_bytes.map_or(Ok(worker_defaults.max_job_bytes), |value| parse_size("max-worker-job-bytes", value))?,
        parse_count("max-worker-artifact-spools", raw.max_worker_artifact_spools, worker_defaults.max_artifact_spools)?,
        raw.max_worker_artifact_spool_bytes
            .map_or(Ok(worker_defaults.max_artifact_spool_bytes), |value| parse_size("max-worker-artifact-spool-bytes", value))?,
        parse_count("max-worker-state-entries", raw.max_worker_state_entries, worker_defaults.max_state_entries)?,
    )?;
    Ok(ResourceLimits {
        connect_timeout,
        sync_timeout,
        build_timeout,
        idle_output_timeout,
        cleanup_timeout,
        max_output_bytes,
        max_active_builds,
        artifact,
        worker,
    })
}

fn parse_ssh_destination(value: &str) -> Result<(String, String, u16, bool), String> {
    if value.is_empty()
        || value.len() > MAX_HOST_BYTES
        || !value.is_ascii()
        || value.chars().any(char::is_control)
        || value.chars().any(char::is_whitespace)
    {
        return Err("SSH destination has invalid syntax".to_string());
    }
    if value.starts_with('-') || value.contains('/') || value.matches('@').count() > 1 {
        return Err("SSH destination has invalid syntax".to_string());
    }
    let (user, authority) = value.split_once('@').map_or((String::new(), value), |(user, authority)| (user.to_string(), authority));
    if !user.is_empty() {
        validate_username(&user)?;
    }
    let (host, port, port_explicit) = if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']').ok_or_else(|| "SSH destination has invalid bracketed host".to_string())?;
        let host = &rest[..close];
        let suffix = &rest[close + 1..];
        if suffix.is_empty() {
            (host.to_string(), 22, false)
        } else {
            let port = suffix
                .strip_prefix(':')
                .ok_or_else(|| "SSH destination has invalid port".to_string())?
                .parse::<u16>()
                .map_err(|_| "SSH destination has invalid port".to_string())?;
            if port == 0 {
                return Err("SSH destination port must be positive".to_string());
            }
            (host.to_string(), port, true)
        }
    } else if authority.matches(':').count() == 1 {
        let (host, port) = authority.split_once(':').unwrap();
        let port = port.parse::<u16>().map_err(|_| "SSH destination has invalid port".to_string())?;
        if port == 0 {
            return Err("SSH destination port must be positive".to_string());
        }
        (host.to_string(), port, true)
    } else if authority.contains(':') {
        return Err("SSH destination must use a bracketed IPv6 host".to_string());
    } else {
        (authority.to_string(), 22, false)
    };
    validate_destination_host(&host)?;
    Ok((user, host, port, port_explicit))
}

fn validate_destination_host(host: &str) -> Result<(), String> {
    if host.is_empty() || host.len() > MAX_HOST_BYTES || host.starts_with('-') || host.ends_with('-') || host.chars().any(char::is_control) {
        return Err("SSH destination host has invalid syntax".to_string());
    }
    if IpAddr::from_str(host).is_ok() {
        return Ok(());
    }
    let bytes = host.as_bytes();
    if !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || !bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'-' | b'_' | b'+'))
    {
        return Err("SSH destination host has invalid syntax".to_string());
    }
    Ok(())
}

struct RedactedEnvironment(usize);

impl fmt::Debug for RedactedEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RedactedEnvironment").field("entries", &self.0).finish()
    }
}

#[cfg(test)]
#[path = "remote_target_ut.rs"]
mod tests;
