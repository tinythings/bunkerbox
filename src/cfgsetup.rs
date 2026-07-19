use std::io::{self, BufRead, Write};
use std::os::unix::fs::PermissionsExt;

use crate::cfg::{EnvMode, ImageOverrides, ProjectConfig, ProjectSection, RuntimeConfig, SandboxConfig, WorkspaceMode};
use crate::vscomm::buildsys;
use crate::workspace;

pub fn run(runtime: Option<&RuntimeConfig>) -> Result<(), String> {
    println!();
    println!("  Bunkerbox project setup");
    println!("  ========================");
    println!();

    let repo_root = workspace::project_root()?;
    let detected_relaxed = buildsys::scan(&repo_root, buildsys::PassthroughMode::Relaxed);

    if !detected_relaxed.is_empty() {
        print!("  Detected build systems: ");
        let names: Vec<&str> = detected_relaxed.iter().map(|e| e.trim_end_matches(" *")).collect();
        println!("{}", names.join(", "));
        println!();
    }

    let quota = pick_quota()?;
    let env_mode = pick_env_mode()?;
    let detected = if env_mode == EnvMode::Paranoid { buildsys::scan(&repo_root, buildsys::PassthroughMode::Paranoid) } else { detected_relaxed };
    let passthrough = build_passthrough(detected, env_mode)?;
    let overrides = pick_overrides(runtime)?;

    let sandbox = if env_mode == EnvMode::Dangerous { SandboxConfig::default() } else { pick_bwrap_path()? };

    let cfg =
        ProjectConfig { project: ProjectSection { env: env_mode, quota: Some(quota), exclude: Vec::new(), passthrough, sandbox }, image: overrides };

    let path = repo_root.join(ProjectConfig::PATH);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("failed to create {}: {e}", path.parent().unwrap().display()))?;
    std::fs::write(&path, cfg.to_yaml()).map_err(|e| format!("failed to write {}: {e}", path.display()))?;

    println!();
    println!("  Wrote {}", path.display());
    Ok(())
}

fn pick_quota() -> Result<String, String> {
    let options = ["auto", "5G", "10G", "20G", "custom"];

    println!("  Quota (loopback image size):");
    for (i, label) in options.iter().enumerate() {
        let note = if i == 0 { " (default)" } else { "" };
        println!("    [{i}] {label}{note}");
    }
    println!();

    let idx = pick_num("  Pick [auto]", 0, options.len() - 1)?;

    if options[idx] == "custom" {
        print!("  Size (e.g. 20G, 500M): ");
        io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
        let line = read_line()?;
        Ok(if line.is_empty() { "auto".into() } else { line })
    } else {
        Ok(options[idx].into())
    }
}

fn pick_env_mode() -> Result<EnvMode, String> {
    println!("  Env mode:");
    println!("    [r] relaxed            (default) - sandboxed, globs allowed");
    println!("    [p] paranoid           - sandboxed, exact commands only");
    println!("    [d] dangerous          - no sandbox, use at your own risk");
    println!();

    let key = key_press("  Pick [r]", &['r', 'p', 'd'], 'r')?;
    Ok(match key {
        'p' => EnvMode::Paranoid,
        'd' => EnvMode::Dangerous,
        _ => EnvMode::Relaxed,
    })
}

fn build_passthrough(detected: Vec<String>, env_mode: EnvMode) -> Result<Vec<String>, String> {
    let mut entries: Vec<String> =
        detected.into_iter().map(|e| if env_mode == EnvMode::Paranoid { e.trim_end_matches(" *").to_string() } else { e }).collect();

    if !entries.is_empty() {
        println!("  Passthrough (detected):");
        for entry in &entries {
            println!("    - {entry}");
        }
        println!();
    }

    loop {
        println!("    [a] add a command");
        println!("    [d] done");
        println!();
        let key = key_press("  Pick [d]", &['a', 'd'], 'd')?;
        if key == 'd' {
            break;
        }
        print!("  Command: ");
        io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
        let line = read_line()?;
        if !line.is_empty() {
            let trimmed = line.trim();
            if env_mode == EnvMode::Paranoid && trimmed.contains('*') {
                println!("  ! glob patterns not allowed in paranoid mode");
                continue;
            }
            let entry = trimmed.to_string();
            entries.push(entry);
        }
    }

    Ok(entries)
}

fn pick_overrides(runtime: Option<&RuntimeConfig>) -> Result<ImageOverrides, String> {
    println!();
    println!("  ══ Runtime overrides (optional) ═══");
    println!();
    println!("  Change how the container runs for this project.");
    println!("  Press Enter on any option to keep the current value.");
    println!();

    let key = key_press("  Configure overrides? [y/N]", &['y', 'n'], 'n')?;
    if key == 'n' {
        return Ok(ImageOverrides::default());
    }
    println!();

    let workspace = pick_workspace_override(runtime)?;
    let session_mb = pick_session_override(runtime)?;
    let allow = pick_allow_override(runtime)?;

    Ok(ImageOverrides { workspace, session_mb, allow })
}

fn pick_workspace_override(runtime: Option<&RuntimeConfig>) -> Result<Option<WorkspaceMode>, String> {
    let current = runtime.and_then(|r| r.workspace).unwrap_or_default();
    println!("  Workspace mode (current: {}):", workspace_label(current));
    println!("    [1] cow         copy-on-write, quota-limited (recommended)");
    println!("    [2] direct      mount repo directly, no guardrails");
    println!("    [3] isolated    disposable clone via git worktree");
    println!();
    let idx = pick_num_skip("  Pick [skip]", 1, 3)?;
    Ok(match idx {
        1 => Some(WorkspaceMode::Cow),
        2 => Some(WorkspaceMode::Direct),
        3 => Some(WorkspaceMode::Isolated),
        _ => None,
    })
}

fn pick_session_override(runtime: Option<&RuntimeConfig>) -> Result<Option<u32>, String> {
    let current = runtime.map(|r| r.session_mb()).unwrap_or(50);
    println!("  Session size (current: {current} MB):");
    println!("    [1] 50 MB");
    println!("    [2] 100 MB");
    println!("    [3] 200 MB");
    println!("    [4] 500 MB");
    println!("    [c] custom");
    println!();

    let key = key_press("  Pick [skip]", &['1', '2', '3', '4', 'c'], 's')?;
    Ok(match key {
        '1' => Some(50),
        '2' => Some(100),
        '3' => Some(200),
        '4' => Some(500),
        'c' => {
            print!("  Size in MB: ");
            io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
            read_line()?.parse::<u32>().ok()
        }
        _ => None,
    })
}

fn pick_allow_override(runtime: Option<&RuntimeConfig>) -> Result<Option<Vec<String>>, String> {
    let base_hosts = runtime.and_then(|r| r.allow.as_deref()).unwrap_or(&[]);
    if base_hosts.is_empty() {
        println!("  No hosts found in runtime config.");
        println!("  Pass --share <dir> to auto-detect from packaged config.");
    } else {
        println!("  Currently allowed hosts (from runtime config):");
        for host in base_hosts {
            println!("    - {host}");
        }
    }
    println!();

    let mut extras: Vec<String> = Vec::new();
    loop {
        println!("    [a] add a host");
        println!("    [d] done");
        println!();
        let key = key_press("  Pick [d]", &['a', 'd'], 'd')?;
        if key == 'd' {
            break;
        }
        print!("  Host: ");
        io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
        let line = read_line()?;
        if !line.is_empty() {
            extras.push(line.trim().to_string());
        }
    }
    Ok(if extras.is_empty() { None } else { Some(extras) })
}

fn workspace_label(mode: WorkspaceMode) -> &'static str {
    match mode {
        WorkspaceMode::Cow => "cow",
        WorkspaceMode::Direct => "direct",
        WorkspaceMode::Isolated => "isolated",
    }
}

fn pick_num_skip(prompt: &str, min: usize, max: usize) -> Result<usize, String> {
    loop {
        print!("{prompt}: ");
        io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
        let line = read_line()?;
        if line.is_empty() {
            return Ok(0);
        }
        match line.parse::<usize>() {
            Ok(n) if n >= min && n <= max => return Ok(n),
            _ => {}
        }
    }
}

fn pick_num(prompt: &str, min: usize, max: usize) -> Result<usize, String> {
    loop {
        print!("{prompt}: ");
        io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
        let line = read_line()?;
        if line.is_empty() {
            return Ok(min);
        }
        match line.parse::<usize>() {
            Ok(n) if n >= min && n <= max => return Ok(n),
            _ => {}
        }
    }
}

fn key_press(prompt: &str, valid: &[char], default: char) -> Result<char, String> {
    loop {
        print!("{prompt}: ");
        io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
        let line = read_line()?;
        if line.is_empty() {
            return Ok(default);
        }
        let c = line.chars().next().unwrap_or('\0');
        if valid.contains(&c) {
            return Ok(c);
        }
    }
}

fn read_line() -> Result<String, String> {
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).map_err(|e| format!("read: {e}"))?;
    Ok(line.trim().to_string())
}

fn pick_bwrap_path() -> Result<SandboxConfig, String> {
    println!();
    println!("  Bubblewrap sandbox binary:");
    println!("    Enter an absolute path to a custom bwrap binary, or");
    println!("    press Enter to use the system bwrap from PATH.");
    println!();

    loop {
        print!("  bwrap path [system]: ");
        io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
        let line = read_line()?;
        if line.is_empty() {
            return Ok(SandboxConfig::default());
        }
        let path = std::path::PathBuf::from(&line);
        if !path.is_absolute() {
            println!("  ! bwrap path must be absolute");
            continue;
        }
        if !path.is_file() {
            println!("  ! not a file: {}", path.display());
            continue;
        }
        let metadata = std::fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if metadata.permissions().mode() & 0o111 == 0 {
            println!("  ! not executable: {}", path.display());
            continue;
        }
        return Ok(SandboxConfig { bwrap: Some(path) });
    }
}
