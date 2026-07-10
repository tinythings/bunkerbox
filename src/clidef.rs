use clap::builder::styling;
use clap::{Arg, ArgAction, Command};
use colored::Colorize;

pub static APPNAME: &str = "bunkerbox";

pub fn cli(version: &'static str) -> Command {
    let styles = styling::Styles::styled()
        .header(styling::AnsiColor::Yellow.on_default())
        .usage(styling::AnsiColor::Yellow.on_default())
        .literal(styling::AnsiColor::BrightGreen.on_default())
        .placeholder(styling::AnsiColor::BrightMagenta.on_default());

    Command::new(APPNAME)
        .version(version)
        .about(format!("{} - {}", APPNAME.bright_magenta().bold(), "run AI agents inside a Kata bunker"))
        .override_usage(format!("{APPNAME} <COMMAND>"))
        .subcommand(Command::new("setup").about("IT: setup host runtime").styles(styles.clone()).disable_help_flag(true).arg(help_arg()))
        .subcommand(
            Command::new("install-image").about("IT: import prepared OCI image").styles(styles.clone()).disable_help_flag(true).arg(help_arg()),
        )
        .subcommand(
            Command::new("prepare")
                .about("Create disposable bunker workspace")
                .styles(styles.clone())
                .disable_help_flag(true)
                .arg(Arg::new("reset").long("reset").action(ArgAction::SetTrue).help("Recreate workspace if it already exists"))
                .arg(help_arg()),
        )
        .subcommand(
            Command::new("run")
                .about("Run named embedded YAML sequence")
                .styles(styles.clone())
                .disable_help_flag(true)
                .arg(Arg::new("name").help("Sequence name").required(true).index(1))
                .arg(help_arg()),
        )
        .subcommand(Command::new("list").about("List embedded YAML sequences").styles(styles.clone()).disable_help_flag(true).arg(help_arg()))
        .next_help_heading("Other")
        .arg(Arg::new("share").long("share").help("Override bunkerbox share directory"))
        .arg(Arg::new("workspace").long("workspace").value_parser(["share", "clone"]).help("Override workspace mode: share or clone"))
        .arg(help_arg())
        .arg(Arg::new("version").short('v').long("version").action(ArgAction::SetTrue).help("Get the current version."))
        .disable_help_flag(true)
        .disable_version_flag(true)
        .disable_colored_help(false)
        .styles(styles)
        .after_help("NOTE: bunkerbox is a proof of concept.\n".bright_yellow().to_string())
}

fn help_arg() -> Arg {
    Arg::new("help").short('h').long("help").action(ArgAction::SetTrue).help("Display help")
}
