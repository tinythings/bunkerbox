use crate::cfg::{ProjectConfig, WorkspaceMode};
use crate::overlay::CowWorkspace;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

pub fn prepare(reset: bool) -> Result<(), String> {
    let workspace = prepare_workspace(reset, false)?;
    println!("{}", workspace.display());
    Ok(())
}

pub enum WorkspaceHandle {
    Cow { inner: CowWorkspace },
    Direct { path: PathBuf },
    Isolated { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCwd {
    host: PathBuf,
    relative: PathBuf,
}

impl WorkspaceCwd {
    pub fn resolve(workspace: &Path, guest_cwd: &Path) -> Result<Self, String> {
        if !guest_cwd.is_absolute() {
            return Err(format!("working directory must be absolute: {}", guest_cwd.display()));
        }

        let relative = guest_cwd
            .strip_prefix(Path::new("/workspace"))
            .map_err(|_| format!("working directory must be under /workspace: {}", guest_cwd.display()))?
            .components()
            .try_fold(PathBuf::new(), |mut relative, component| match component {
                Component::CurDir => Ok(relative),
                Component::Normal(name) => {
                    relative.push(name);
                    Ok(relative)
                }
                Component::ParentDir => Err(format!("working directory contains '..': {}", guest_cwd.display())),
                Component::RootDir | Component::Prefix(_) => Err(format!("invalid workspace path: {}", guest_cwd.display())),
            })?;

        let canonical_workspace = fs::canonicalize(workspace).map_err(|err| format!("failed to resolve workspace {}: {err}", workspace.display()))?;
        let candidate = canonical_workspace.join(&relative);
        let canonical_host =
            fs::canonicalize(&candidate).map_err(|err| format!("failed to resolve working directory {}: {err}", guest_cwd.display()))?;

        canonical_host.strip_prefix(&canonical_workspace).map_err(|_| format!("working directory escapes workspace: {}", guest_cwd.display()))?;

        if !fs::metadata(&canonical_host).map(|metadata| metadata.is_dir()).unwrap_or(false) {
            return Err(format!("working directory is not a directory: {}", guest_cwd.display()));
        }

        Ok(Self { host: canonical_host, relative })
    }

    pub fn host_path(&self) -> &Path {
        &self.host
    }

    pub fn relative_path(&self) -> &Path {
        &self.relative
    }

    pub fn guest_path(&self) -> PathBuf {
        if self.relative.as_os_str().is_empty() {
            PathBuf::from("/workspace")
        } else {
            Path::new("/workspace").join(&self.relative)
        }
    }
}

impl WorkspaceHandle {
    pub fn path(&self) -> &Path {
        match self {
            WorkspaceHandle::Cow { inner } => &inner.mount_point,
            WorkspaceHandle::Direct { path } => path,
            WorkspaceHandle::Isolated { path } => path,
        }
    }
}

pub fn resolve(mode: WorkspaceMode, quota_bytes: u64, runtime_exclude: Option<&[String]>, app_name: &str) -> Result<WorkspaceHandle, String> {
    match mode {
        WorkspaceMode::Cow => {
            let repo_root = project_root()?;
            let env = ProjectConfig::load_or_create(&repo_root)?;
            let cow = CowWorkspace::setup(&repo_root, &env, quota_bytes, runtime_exclude, app_name)?;
            Ok(WorkspaceHandle::Cow { inner: cow })
        }
        WorkspaceMode::Direct => {
            Ok(WorkspaceHandle::Direct { path: std::env::current_dir().map_err(|e| format!("failed to get current directory: {e}"))? })
        }
        WorkspaceMode::Isolated => Ok(WorkspaceHandle::Isolated { path: prepare_workspace(false, true)? }),
    }
}

fn prepare_workspace(reset: bool, reuse_existing: bool) -> Result<PathBuf, String> {
    let source = project_root()?;
    let bunker_dir = source.join(".bunker");
    let workspace = bunker_dir.join("workspace");

    if workspace.exists() {
        if reuse_existing {
            return Ok(workspace);
        }

        if reset {
            fs::remove_dir_all(&workspace).map_err(|err| format!("failed to remove {}: {err}", workspace.display()))?;
        } else {
            return Err(format!("workspace already exists: {}", workspace.display()));
        }
    }

    fs::create_dir_all(&bunker_dir).map_err(|err| format!("failed to create {}: {err}", bunker_dir.display()))?;

    if try_git_worktree(&source, &workspace)? {
        return Ok(workspace);
    }

    copy_workspace(&source, &workspace)?;
    Ok(workspace)
}

pub fn project_root() -> Result<PathBuf, String> {
    if let Some(root) = git_root()? {
        return Ok(root);
    }

    std::env::current_dir().map_err(|err| format!("failed to get current directory: {err}"))
}

fn git_root() -> Result<Option<PathBuf>, String> {
    let output = Command::new("git").args(["rev-parse", "--show-toplevel"]).stderr(Stdio::null()).output();

    let Ok(output) = output else {
        return Ok(None);
    };

    if !output.status.success() {
        return Ok(None);
    }

    let root = String::from_utf8(output.stdout).map_err(|err| format!("git output is not UTF-8: {err}"))?.trim().to_string();

    if root.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(root)))
    }
}

fn try_git_worktree(source: &Path, workspace: &Path) -> Result<bool, String> {
    if git_root()?.is_none() {
        return Ok(false);
    }

    let status = Command::new("git")
        .current_dir(source)
        .args(["worktree", "add", "--detach"])
        .arg(workspace)
        .arg("HEAD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| format!("failed to run git worktree: {err}"))?;

    Ok(status.success())
}

fn copy_workspace(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|err| format!("failed to create {}: {err}", destination.display()))?;

    copy_dir(source, destination)
}

fn copy_dir(source: &Path, destination: &Path) -> Result<(), String> {
    for entry in fs::read_dir(source).map_err(|err| format!("failed to read {}: {err}", source.display()))? {
        let entry = entry.map_err(|err| format!("failed to read directory entry: {err}"))?;
        let path = entry.path();
        let name = entry.file_name();

        if should_skip(&name) {
            continue;
        }

        let target = destination.join(&name);
        let metadata = entry.metadata().map_err(|err| format!("failed to stat {}: {err}", path.display()))?;

        if metadata.is_dir() {
            fs::create_dir_all(&target).map_err(|err| format!("failed to create {}: {err}", target.display()))?;
            copy_dir(&path, &target)?;
        } else if metadata.is_file() {
            fs::copy(&path, &target).map_err(|err| format!("failed to copy {} to {}: {err}", path.display(), target.display()))?;
            fs::set_permissions(&target, fs::Permissions::from_mode(metadata.permissions().mode()))
                .map_err(|err| format!("failed to chmod {}: {err}", target.display()))?;
        }
    }

    Ok(())
}

fn should_skip(name: &OsStr) -> bool {
    matches!(name.to_str(), Some(".bunker") | Some(".bunkerbox") | Some(".git") | Some("target"))
}

#[cfg(test)]
#[path = "workspace_ut.rs"]
mod tests;
