use crate::runtime::RuntimeConfig;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

pub fn run(config: &RuntimeConfig, workspace: &Path, container_name: &str) -> Result<(), String> {
    if !config.oci.is_file() {
        return Err(format!("OCI archive not found: {}", config.oci.display()));
    }

    run_command("sudo", &["systemctl", "start", "containerd"])?;
    import_image(config)?;
    remove_stale_container(container_name)?;

    run_command(
        "sudo",
        &[
            "ctr",
            "run",
            "--runtime",
            "io.containerd.kata.v2",
            "--rm",
            "--tty",
            "--mount",
            &format!(
                "type=bind,src={},dst=/workspace,options=rbind:rw",
                workspace.display()
            ),
            &config.image,
            container_name,
        ],
    )
}

fn import_image(config: &RuntimeConfig) -> Result<(), String> {
    let output = command_output("sudo", &["ctr", "images", "ls", "-q"])?;

    if output.lines().any(|line| line.trim() == config.image) {
        return Ok(());
    }

    run_command(
        "sudo",
        &[
            "ctr",
            "images",
            "import",
            config
                .oci
                .to_str()
                .ok_or_else(|| format!("path is not valid UTF-8: {}", config.oci.display()))?,
        ],
    )
}

fn remove_stale_container(container_name: &str) -> Result<(), String> {
    run_command_allow_failure("sudo", &["ctr", "tasks", "kill", "--signal", "SIGKILL", container_name])?;
    kill_task_pid(container_name)?;
    kill_kata_shim(container_name)?;
    run_command_allow_failure("sudo", &["systemctl", "restart", "containerd"])?;

    let commands: &[&[&str]] = &[
        &["ctr", "tasks", "delete", "--force", container_name],
        &["ctr", "tasks", "rm", "--force", container_name],
        &["ctr", "containers", "rm", container_name],
        &["ctr", "snapshots", "rm", container_name],
    ];

    for args in commands {
        run_command_allow_failure("sudo", args)?;
    }

    Ok(())
}

fn kill_task_pid(container_name: &str) -> Result<(), String> {
    let output = command_output("sudo", &["ctr", "tasks", "ls"])?;

    for line in output.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        let Some(pid) = fields.next() else {
            continue;
        };

        if name == container_name && pid.chars().all(|ch| ch.is_ascii_digit()) {
            run_command_allow_failure("sudo", &["kill", "-9", pid])?;
        }
    }

    Ok(())
}

fn kill_kata_shim(container_name: &str) -> Result<(), String> {
    let output = command_output("ps", &["-eo", "pid=,args="])?;

    for line in output.lines() {
        if line.contains("containerd-shim-kata-v2") && line.contains("-id") && line.contains(container_name) {
            if let Some(pid) = line.split_whitespace().next() {
                if pid.chars().all(|ch| ch.is_ascii_digit()) {
                    run_command_allow_failure("sudo", &["kill", "-9", pid])?;
                }
            }
        }
    }

    Ok(())
}

fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run {program}: {err}"))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("failed to capture stderr for {program}"))?;
    let stderr_thread = thread::spawn(move || filter_stderr(stderr));

    let status = child
        .wait()
        .map_err(|err| format!("failed to wait for {program}: {err}"))?;
    stderr_thread
        .join()
        .map_err(|_| format!("stderr filter thread panicked for {program}"))??;

    if !status.success() {
        return Err(format!("command failed with status {status}: {program}"));
    }

    Ok(())
}

fn run_command_allow_failure(program: &str, args: &[&str]) -> Result<(), String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;

    Ok(())
}

fn command_output(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("failed to run {program}: {err}"))?;

    print_filtered_stderr(&output.stderr)?;

    if !output.status.success() {
        return Err(format!("command failed with status {}: {program}", output.status));
    }

    String::from_utf8(output.stdout).map_err(|err| format!("command output is not UTF-8: {err}"))
}

fn filter_stderr(stderr: impl std::io::Read) -> Result<(), String> {
    let reader = BufReader::new(stderr);

    for line in reader.lines() {
        let line = line.map_err(|err| format!("failed to read stderr: {err}"))?;
        if !is_filtered_warning(&line) {
            eprintln!("{line}");
        }
    }

    Ok(())
}

fn print_filtered_stderr(stderr: &[u8]) -> Result<(), String> {
    let stderr = String::from_utf8_lossy(stderr);

    for line in stderr.lines() {
        if !is_filtered_warning(line) {
            eprintln!("{line}");
        }
    }

    Ok(())
}

fn is_filtered_warning(line: &str) -> bool {
    line.contains("DEPRECATION: The support for cgroup v1 is deprecated since containerd v2.2")
}
