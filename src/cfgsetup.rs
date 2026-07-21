use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};

use crate::cfg::{EnvMode, ImageOverrides, ProjectConfig, ProjectSection, RuntimeConfig, WorkspaceMode};
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
    let detected = if env_mode == EnvMode::Paranoid { buildsys::scan(&repo_root, buildsys::PassthroughMode::Paranoid) } else { detected_relaxed.clone() };
    let passthrough = build_passthrough(detected, env_mode)?;
    let profiles = pick_profiles(&detected_relaxed)?;
    let overrides = pick_overrides(runtime)?;

    let cfg = ProjectConfig { project: ProjectSection { env: env_mode, quota: Some(quota), exclude: Vec::new(), passthrough }, image: overrides, profiles };

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
    println!("    [r] relaxed           (default)");
    println!("    [p] paranoid");
    println!();

    let key = key_press("  Pick [r]", &['r', 'p'], 'r')?;
    Ok(match key {
        'p' => EnvMode::Paranoid,
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

fn pick_profiles(detected: &[String]) -> Result<Vec<String>, String> {
    let suggested: BTreeSet<String> = detected
        .iter()
        .filter_map(|e| {
            let cmd = e.strip_suffix(" *").unwrap_or(e);
            system_to_profile(cmd)
        })
        .map(String::from)
        .collect();

    if suggested.is_empty() {
        println!();
        let key = key_press("  Sandbox profiles: none detected. Add one? [y/N]", &['y', 'n'], 'n')?;
        if key == 'n' {
            return Ok(Vec::new());
        }
        return add_profile_loop(Vec::new());
    }

    println!();
    println!("  Sandbox profiles (suggested):");
    let sugg_list: Vec<&String> = suggested.iter().collect();
    for (i, p) in sugg_list.iter().enumerate() {
        println!("    [{}] {p}", i + 1);
    }
    println!();

    let mut chosen: Vec<String> = sugg_list.iter().map(|s| s.to_string()).collect();

    loop {
        println!("    [a] accept suggestions  (default)");
        println!("    [n] none");
        println!("    [+] add another profile");
        println!("    [-] remove one");
        println!();
        let key = key_press("  Pick [a]", &['a', 'n', '+', '-'], 'a')?;
        match key {
            'a' => break,
            'n' => {
                chosen.clear();
                break;
            }
            '+' => {
                chosen = add_profile_loop(chosen)?;
                continue;
            }
            '-' => {
                if chosen.is_empty() {
                    continue;
                }
                println!("    Remove which?");
                for (i, p) in chosen.iter().enumerate() {
                    println!("      [{i}] {p}");
                }
                let idx = pick_num("  Pick [skip]", 0, chosen.len() - 1)?;
                chosen.remove(idx);
            }
            _ => unreachable!(),
        }
    }

    Ok(chosen)
}

fn system_to_profile(name: &str) -> Option<&'static str> {
    match name {
        "cargo" => Some("rust"),
        "make" => Some("make"),
        "npm" => Some("node"),
        "go" => Some("go"),
        "python" => Some("python"),
        _ => None,
    }
}

fn add_profile_loop(current: Vec<String>) -> Result<Vec<String>, String> {
    let mut chosen = current;
    let builtins = ["rust", "make", "node", "go", "python"];
    loop {
        println!();
        println!("    Available built-in profiles:");
        for (i, name) in builtins.iter().enumerate() {
            let already = if chosen.contains(&name.to_string()) { " (selected)" } else { "" };
            println!("      [{i}] {name}{already}");
        }
        println!("      [c] custom path");
        println!("      [d] done");
        println!();
        let key = key_press("  Pick [d]", &['0', '1', '2', '3', '4', 'c', 'd'], 'd')?;
        match key {
            'd' => break,
            'c' => {
                print!("  Path: ");
                io::stdout().flush().map_err(|e| format!("flush: {e}"))?;
                let path = read_line()?;
                if !path.is_empty() && !chosen.contains(&path) {
                    chosen.push(path);
                }
            }
            _ => {
                let idx = key.to_digit(10).unwrap() as usize;
                let name = builtins[idx].to_string();
                if chosen.contains(&name) {
                    chosen.retain(|p| p != &name);
                } else {
                    chosen.push(name);
                }
            }
        }
    }
    Ok(chosen)
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
    let current = runtime.map(|r| r.session_mb()).unwrap_or(512);
    println!("  Session size (current: {current} MB):");
    println!("    [1] 512 MB (default)");
    println!("    [2] 1024 MB");
    println!("    [3] 2048 MB");
    println!("    [c] custom");
    println!();

    let key = key_press("  Pick [skip]", &['1', '2', '3', 'c'], 's')?;
    Ok(match key {
        '1' => Some(512),
        '2' => Some(1024),
        '3' => Some(2048),
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
