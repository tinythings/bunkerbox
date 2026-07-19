use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use tokio::process::Command;

use crate::cfg::EnvMode;

pub struct Sandbox {
    pub bwrap: PathBuf,
    pub workspace: PathBuf,
}

const MIN_BWRAP_VERSION: (u32, u32, u32) = (0, 10, 0);
const BWRAP_BUILD_URL: &str = "https://github.com/containers/bubblewrap";

/// Resolve the bubblewrap binary from an optional absolute path or PATH.
/// Validates that it exists, is executable, is at least version 0.10.0,
/// and can perform a basic sandboxed no-op run.
pub fn resolve_bwrap(configured: Option<&Path>) -> Result<PathBuf, String> {
    let path = match configured {
        Some(p) => p.to_path_buf(),
        None => find_on_path("bwrap")?,
    };

    validate_path(&path)?;

    let version = get_bwrap_version(&path)?;
    if version < MIN_BWRAP_VERSION {
        return Err(format!(
            "bubblewrap {} is too old. Minimum required version is {}.\n\
             Build from source: {BWRAP_BUILD_URL}",
            format_version(version),
            format_version(MIN_BWRAP_VERSION)
        ));
    }

    smoke_test(&path)?;
    Ok(path)
}

impl Sandbox {
    pub fn new(bwrap: PathBuf, workspace: PathBuf) -> Self {
        Self { bwrap, workspace }
    }

    /// Wrap a host command in a bubblewrap sandbox.
    ///
    /// The sandbox drops network access, limits filesystem access to read-only
    /// system directories plus the workspace, and provides a scratch HOME.
    pub fn wrap(&self, program: &str, args: &[String], env_mode: EnvMode, guest_env: &[(String, String)]) -> Result<Command, String> {
        let mut cmd = Command::new(&self.bwrap);
        // Keep the bwrap process itself from inheriting sensitive host env.
        cmd.env_clear();
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("HOME", "/home/bunkerbox");

        cmd.arg("--unshare-all");
        cmd.arg("--die-with-parent");
        cmd.arg("--proc").arg("/proc");
        cmd.arg("--dev").arg("/dev");
        cmd.arg("--tmpfs").arg("/tmp");

        for (src, dst) in system_dirs_to_bind()? {
            cmd.arg("--ro-bind").arg(src).arg(dst);
        }

        cmd.arg("--bind").arg(&self.workspace).arg("/workspace");
        cmd.arg("--chdir").arg("/workspace");

        cmd.arg("--tmpfs").arg("/home/bunkerbox");
        cmd.arg("--setenv").arg("HOME").arg("/home/bunkerbox");
        cmd.arg("--setenv").arg("PATH").arg("/usr/bin:/bin");

        if env_mode == EnvMode::Relaxed {
            for (key, val) in guest_env {
                if key == "PATH" || key == "HOME" || key == "VSOCK_CID" || key.starts_with("BUNKERBOX_") || key.starts_with("XDG_") {
                    continue;
                }
                cmd.arg("--setenv").arg(key).arg(val);
            }
        }

        // Re-assert HOME and PATH after any guest-provided values.
        cmd.arg("--setenv").arg("HOME").arg("/home/bunkerbox");
        cmd.arg("--setenv").arg("PATH").arg("/usr/bin:/bin");

        cmd.arg("--");
        cmd.arg(program);
        for a in args {
            cmd.arg(a);
        }

        Ok(cmd)
    }
}

fn validate_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("sandbox.bwrap must be an absolute path, got: {}", path.display()));
    }
    if !path.is_file() {
        return Err(format!("sandbox.bwrap is not a file: {}", path.display()));
    }
    let metadata = std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!("sandbox.bwrap is not executable: {}", path.display()));
    }
    Ok(())
}

fn find_on_path(name: &str) -> Result<PathBuf, String> {
    let output =
        StdCommand::new("sh").arg("-c").arg(format!("command -v {name}")).output().map_err(|e| format!("failed to search PATH for {name}: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "{name} not found on PATH. Install bubblewrap >= {} or set sandbox.bwrap to an absolute path.",
            format_version(MIN_BWRAP_VERSION)
        ));
    }

    let s = String::from_utf8(output.stdout).map_err(|e| format!("invalid UTF-8 from command -v: {e}"))?;
    let s = s.trim();
    if s.is_empty() {
        return Err(format!("{name} not found on PATH"));
    }
    Ok(PathBuf::from(s))
}

fn get_bwrap_version(path: &Path) -> Result<(u32, u32, u32), String> {
    let output = StdCommand::new(path).arg("--version").output().map_err(|e| format!("failed to run {} --version: {e}", path.display()))?;

    if !output.status.success() {
        return Err(format!("{} --version failed with status {}", path.display(), output.status));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    parse_bwrap_version(&text).ok_or_else(|| format!("cannot parse bubblewrap version from: {text}"))
}

fn parse_bwrap_version(text: &str) -> Option<(u32, u32, u32)> {
    // Typical output: "bubblewrap 0.11.0" or "bwrap 0.10.0"
    for token in text.split_whitespace() {
        let cleaned = token.strip_prefix('v').unwrap_or(token);
        let parts: Vec<&str> = cleaned.split('.').collect();
        if parts.len() == 3 {
            if let (Ok(major), Ok(minor), Ok(patch)) = (parts[0].parse(), parts[1].parse(), parts[2].parse()) {
                return Some((major, minor, patch));
            }
        }
    }
    None
}

fn format_version((a, b, c): (u32, u32, u32)) -> String {
    format!("{a}.{b}.{c}")
}

fn smoke_test(path: &Path) -> Result<(), String> {
    let mut cmd = StdCommand::new(path);
    cmd.args(["--unshare-all", "--die-with-parent", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);

    for (src, dst) in system_dirs_to_bind()? {
        cmd.arg("--ro-bind").arg(src).arg(dst);
    }

    cmd.args(["--setenv", "PATH", "/usr/bin:/bin", "true"]);

    let status = cmd.status().map_err(|e| format!("failed to run bubblewrap smoke test ({}): {e}", path.display()))?;

    if !status.success() {
        return Err(format!("bubblewrap smoke test failed for {}. Is unprivileged user namespace support enabled?", path.display()));
    }
    Ok(())
}

fn system_dirs_to_bind() -> Result<Vec<(String, String)>, String> {
    let dirs = ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"];
    let mut out = Vec::new();
    for d in dirs {
        if Path::new(d).is_dir() {
            out.push((d.to_string(), d.to_string()));
        }
    }
    if out.is_empty() {
        return Err("no essential system directories found to bind into sandbox".to_string());
    }
    Ok(out)
}

#[cfg(test)]
#[path = "sandbox_ut.rs"]
mod tests;
