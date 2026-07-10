use clap::builder::styling;
use clap::{Arg, ArgAction, Command};
use colored::Colorize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};

const APPNAME: &str = "bunkerbox-image";
const DEFAULT_OPENCODE_VERSION: &str = "1.17.18";

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

    match matches.subcommand() {
        Some(("opencode", submatches)) => {
            if submatches.get_flag("help") {
                print_subcommand_help("opencode")?;
                return Ok(());
            }

            let version = submatches.get_one::<String>("opencode-version").map(String::as_str).unwrap_or(DEFAULT_OPENCODE_VERSION);
            let output = submatches
                .get_one::<String>("output")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("bunkerbox-opencode-{version}.oci")));
            let tag = submatches.get_one::<String>("tag").cloned().unwrap_or_else(|| format!("localhost/bunkerbox-opencode:{version}"));

            build_opencode(version, &tag, &output)
        }
        Some((name, _)) => Err(format!("unknown command: {name}")),
        None => {
            cli.print_help().map_err(|err| err.to_string())?;
            println!();
            Ok(())
        }
    }
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
        .override_usage(format!("{APPNAME} <COMMAND> [OPTIONS]"))
        .subcommand(
            Command::new("opencode")
                .about("Build OpenCode OCI archive")
                .styles(styles.clone())
                .disable_help_flag(true)
                .arg(Arg::new("opencode-version").long("opencode-version").help("OpenCode version").default_value(DEFAULT_OPENCODE_VERSION))
                .arg(Arg::new("output").short('o').long("output").help("Output OCI archive path"))
                .arg(Arg::new("tag").short('t').long("tag").help("Local image tag"))
                .arg(help_arg()),
        )
        .next_help_heading("Other")
        .arg(help_arg())
        .arg(Arg::new("version").short('v').long("version").action(ArgAction::SetTrue).help("Get the current version."))
        .disable_help_flag(true)
        .disable_version_flag(true)
        .disable_colored_help(false)
        .styles(styles)
        .after_help("NOTE: output archives are consumed by bunkerbox install-image.\n".bright_yellow().to_string())
}

fn help_arg() -> Arg {
    Arg::new("help").short('h').long("help").action(ArgAction::SetTrue).help("Display help")
}

fn print_subcommand_help(name: &str) -> Result<(), String> {
    let mut cli = cli(env!("CARGO_PKG_VERSION"));
    let subcommand = cli.find_subcommand_mut(name).ok_or_else(|| format!("unknown command: {name}"))?;

    subcommand.print_help().map_err(|err| err.to_string())?;
    println!();
    Ok(())
}

fn build_opencode(version: &str, tag: &str, output: &Path) -> Result<(), String> {
    require_program("podman")?;

    if output.exists() {
        return Err(format!("output already exists: {}", output.display()));
    }

    let build_dir = env::temp_dir().join(format!("bunkerbox-image-{}", std::process::id()));
    fs::create_dir_all(&build_dir).map_err(|err| format!("failed to create build dir {}: {err}", build_dir.display()))?;

    let result = (|| {
        write_opencode_files(&build_dir)?;

        run_command(
            "podman",
            &[
                "build",
                "--no-cache",
                "--build-arg",
                &format!("OPENCODE_VERSION={version}"),
                "-t",
                tag,
                "-f",
                path_str(&build_dir.join("Containerfile"))?,
                path_str(&build_dir)?,
            ],
        )?;

        run_command("podman", &["save", "--format", "oci-archive", "-o", path_str(output)?, tag])?;

        println!("{}", output.display());
        Ok(())
    })();

    let _ = fs::remove_dir_all(&build_dir);
    result
}

fn write_opencode_files(build_dir: &Path) -> Result<(), String> {
    fs::write(
        build_dir.join("bunker-entrypoint"),
        r#"#!/bin/sh
set -eu
exec opencode
"#,
    )
    .map_err(|err| format!("failed to write entrypoint: {err}"))?;

    fs::write(
        build_dir.join("Containerfile"),
        r#"FROM docker.io/library/alpine:3.22

ARG OPENCODE_VERSION

RUN apk add --no-cache \
      bash \
      ca-certificates \
      curl \
      git \
      libstdc++ \
      openssh-client \
      ripgrep \
    && curl -fsSL \
      "https://github.com/anomalyco/opencode/releases/download/v${OPENCODE_VERSION}/opencode-linux-x64-baseline-musl.tar.gz" \
      -o /tmp/opencode.tar.gz \
    && tar -xzf /tmp/opencode.tar.gz -C /usr/local/bin opencode \
    && chmod 0755 /usr/local/bin/opencode \
    && rm -f /tmp/opencode.tar.gz \
    && opencode --version

RUN addgroup -g 1000 opencode \
    && adduser -D -u 1000 -G opencode -s /bin/bash opencode \
    && mkdir -p /workspace /home/opencode \
    && chown -R opencode:opencode /workspace /home/opencode

COPY bunker-entrypoint /usr/local/bin/bunker-entrypoint
RUN chmod 0755 /usr/local/bin/bunker-entrypoint

ENV HOME=/home/opencode \
    XDG_CONFIG_HOME=/home/opencode/.config \
    XDG_DATA_HOME=/home/opencode/.local/share \
    XDG_STATE_HOME=/home/opencode/.local/state \
    XDG_CACHE_HOME=/home/opencode/.cache

USER opencode
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/bunker-entrypoint"]
"#,
    )
    .map_err(|err| format!("failed to write Containerfile: {err}"))?;

    Ok(())
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

fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
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

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}
