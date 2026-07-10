use serde::Deserialize;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

const COMMANDS_YAML: &str = include_str!("commands.yaml");

#[derive(Debug, Deserialize)]
struct Script {
    commands: Vec<String>,
}

pub fn run_sequence(name: &str) -> Result<(), String> {
    let scripts: BTreeMap<String, Script> =
        serde_yaml::from_str(COMMANDS_YAML).map_err(|err| format!("failed to parse embedded commands.yaml: {err}"))?;

    let script = scripts.get(name).ok_or_else(|| format!("unknown sequence: {name}"))?;

    for command in &script.commands {
        run_shell(command)?;
    }

    Ok(())
}

pub fn sequence_names() -> Result<Vec<String>, String> {
    let scripts: BTreeMap<String, Script> =
        serde_yaml::from_str(COMMANDS_YAML).map_err(|err| format!("failed to parse embedded commands.yaml: {err}"))?;

    Ok(scripts.keys().cloned().collect())
}

fn run_shell(command: &str) -> Result<(), String> {
    let status = Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| format!("failed to run shell command: {err}"))?;

    if !status.success() {
        return Err(format!("shell command failed with status {status}"));
    }

    Ok(())
}
