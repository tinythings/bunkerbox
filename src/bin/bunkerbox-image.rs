use clap::builder::styling;
use clap::{Arg, ArgAction, Command};
use colored::Colorize;
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};

const APPNAME: &str = "bunkerbox-image";

#[derive(Debug, Deserialize)]
struct ImageConfig {
    name: String,
    image: String,
    output: PathBuf,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    build_args: BTreeMap<String, String>,
    #[serde(default)]
    files: Vec<BuildFile>,
    containerfile: String,
}

#[derive(Debug, Deserialize)]
struct BuildFile {
    path: PathBuf,
    #[serde(default = "default_file_mode", deserialize_with = "deserialize_mode")]
    mode: u32,
    content: String,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{APPNAME}: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut cli = cli(env!("CARGO_PKG_VERSION"));
    let matches = cli.clone().get_matches();

    if matches.get_flag("help") {
        cli.print_help().map_err(|err| err.to_string())?;
        println!();
        return Ok(());
    }

    if matches.get_flag("version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let Some(config_path) = matches.get_one::<String>("config") else {
        cli.print_help().map_err(|err| err.to_string())?;
        println!();
        return Ok(());
    };

    let config = load_config(Path::new(config_path))?;
    build_image(&config)
}

fn cli(version: &'static str) -> Command {
    let styles = styling::Styles::styled()
        .header(styling::AnsiColor::Yellow.on_default())
        .usage(styling::AnsiColor::Yellow.on_default())
        .literal(styling::AnsiColor::BrightGreen.on_default())
        .placeholder(styling::AnsiColor::BrightMagenta.on_default());

    Command::new(APPNAME)
        .version(version)
        .about(format!("{} - {}", APPNAME.bright_magenta().bold(), "build prepared bunker agent OCI images"))
        .override_usage(format!("{APPNAME} <CONFIG>"))
        .arg(Arg::new("config").help("Image YAML config file").required(false).index(1))
        .next_help_heading("Other")
        .arg(help_arg())
        .arg(Arg::new("version").short('v').long("version").action(ArgAction::SetTrue).help("Get the current version."))
        .disable_help_flag(true)
        .disable_version_flag(true)
        .disable_colored_help(false)
        .styles(styles)
        .after_help("Example: bunkerbox-image images/opencode.conf\n".bright_yellow().to_string())
}

fn help_arg() -> Arg {
    Arg::new("help").short('h').long("help").action(ArgAction::SetTrue).help("Display help")
}

fn load_config(path: &Path) -> Result<ImageConfig, String> {
    let contents = fs::read_to_string(path).map_err(|err| format!("failed to read config {}: {err}", path.display()))?;

    serde_yaml::from_str(&contents).map_err(|err| format!("failed to parse config {}: {err}", path.display()))
}

fn build_image(config: &ImageConfig) -> Result<(), String> {
    require_program("podman")?;

    if config.name.trim().is_empty() {
        return Err("image config name is required".to_string());
    }

    if config.output.exists() {
        if config.overwrite {
            fs::remove_file(&config.output).map_err(|err| format!("failed to remove {}: {err}", config.output.display()))?;
        } else {
            return Err(format!("output already exists: {}", config.output.display()));
        }
    }

    let build_dir = env::temp_dir().join(format!("bunkerbox-image-{}", std::process::id()));
    fs::create_dir_all(&build_dir).map_err(|err| format!("failed to create build dir {}: {err}", build_dir.display()))?;

    let result = (|| {
        write_build_context(config, &build_dir)?;
        podman_build(config, &build_dir)?;
        podman_save(config)?;
        podman_remove_image(config)?;
        println!("{}", config.output.display());
        Ok(())
    })();

    let _ = fs::remove_dir_all(&build_dir);
    result
}

fn write_build_context(config: &ImageConfig, build_dir: &Path) -> Result<(), String> {
    fs::write(build_dir.join("Containerfile"), &config.containerfile).map_err(|err| format!("failed to write Containerfile: {err}"))?;

    for file in &config.files {
        if file.path.is_absolute() || file.path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
            return Err(format!("unsafe build file path: {}", file.path.display()));
        }

        let full_path = build_dir.join(&file.path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }

        fs::write(&full_path, &file.content).map_err(|err| format!("failed to write {}: {err}", full_path.display()))?;
        fs::set_permissions(&full_path, fs::Permissions::from_mode(file.mode))
            .map_err(|err| format!("failed to chmod {}: {err}", full_path.display()))?;
    }

    Ok(())
}

fn podman_build(config: &ImageConfig, build_dir: &Path) -> Result<(), String> {
    let mut args = vec!["build".to_string(), "--no-cache".to_string()];

    for (name, value) in &config.build_args {
        args.push("--build-arg".to_string());
        args.push(format!("{name}={value}"));
    }

    args.push("-t".to_string());
    args.push(config.image.clone());
    args.push("-f".to_string());
    args.push(build_dir.join("Containerfile").display().to_string());
    args.push(build_dir.display().to_string());

    run_command("podman", &args)
}

fn podman_save(config: &ImageConfig) -> Result<(), String> {
    run_command(
        "podman",
        &[
            "save".to_string(),
            "--format".to_string(),
            "oci-archive".to_string(),
            "-o".to_string(),
            config.output.display().to_string(),
            config.image.clone(),
        ],
    )
}

fn podman_remove_image(config: &ImageConfig) -> Result<(), String> {
    let containers = command_output(
        "podman",
        &[
            "ps".to_string(),
            "-a".to_string(),
            "--filter".to_string(),
            format!("ancestor={}", config.image),
            "--format".to_string(),
            "{{.ID}}".to_string(),
        ],
    )?;

    for container in containers.lines().filter(|line| !line.trim().is_empty()) {
        run_command("podman", &["rm".to_string(), "-f".to_string(), container.to_string()])?;
    }

    run_command("podman", &["image".to_string(), "rm".to_string(), "-f".to_string(), config.image.clone()])
}

fn require_program(name: &str) -> Result<(), String> {
    let status = ProcCommand::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map_err(|err| format!("failed to check {name}: {err}"))?;

    if !status.success() {
        return Err(format!("missing required program: {name}"));
    }

    Ok(())
}

fn run_command(program: &str, args: &[String]) -> Result<(), String> {
    let status = ProcCommand::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;

    if !status.success() {
        return Err(format!("command failed with status {status}: {program}"));
    }

    Ok(())
}

fn command_output(program: &str, args: &[String]) -> Result<String, String> {
    let output = ProcCommand::new(program).args(args).stderr(Stdio::inherit()).output().map_err(|err| format!("failed to run {program}: {err}"))?;

    if !output.status.success() {
        return Err(format!("command failed with status {}: {program}", output.status));
    }

    String::from_utf8(output.stdout).map_err(|err| format!("command output is not UTF-8: {err}"))
}

fn default_file_mode() -> u32 {
    0o644
}

fn deserialize_mode<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Mode {
        Int(u32),
        String(String),
    }

    match Mode::deserialize(deserializer)? {
        Mode::Int(value) => Ok(value),
        Mode::String(value) => {
            let trimmed = value.trim();
            let radix = if trimmed.starts_with('0') { 8 } else { 10 };
            u32::from_str_radix(trimmed, radix).map_err(serde::de::Error::custom)
        }
    }
}
