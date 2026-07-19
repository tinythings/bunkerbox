use std::io::{self, BufRead, Write};

use crate::cfg::{EnvMode, ProjectConfig, ProjectSection};
use crate::vscomm::buildsys;
use crate::workspace;

pub fn run() -> Result<(), String> {
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

    let cfg =
        ProjectConfig { project: ProjectSection { env: env_mode, quota: Some(quota), exclude: Vec::new(), passthrough }, image: Default::default() };

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
