use crate::runtime::{HomeMode, NetworkMode, RuntimeConfig};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

pub fn run(config: &RuntimeConfig, workspace: &Path, container_name: &str, _share_dir: &Path, app_name: &str) -> Result<(), String> {
    if !config.oci.is_file() {
        return Err(format!("OCI archive not found: {}", config.oci.display()));
    }

    run_command("sudo", &["systemctl", "start", "containerd"])?;
    let bridge_firewall = config.network == Some(NetworkMode::Bridge) && config.allow.is_some();
    if config.network == Some(NetworkMode::Bridge) {
        ensure_bridge_cni_config()?;
    }
    remove_stale_container(container_name)?;
    import_image(config)?;

    let resolv_conf = if config.network.is_some() { Some(write_resolv_conf()?) } else { None };
    if bridge_firewall {
        ensure_bridge_egress_firewall(config, resolv_conf.as_deref())?;
    }
    let resolv_conf_mount = resolv_conf.as_ref().map(|path| format!("type=bind,src={},dst=/etc/resolv.conf,options=rbind:ro", path.display()));
    let workspace_mount = format!("type=bind,src={},dst=/workspace,options=rbind:rw", workspace.display());
    let user = current_user_spec()?;
    let Some((uid, gid)) = user.split_once(':') else {
        return Err(format!("invalid current user spec: {user}"));
    };
    let mut container_env = Vec::new();
    let home_mount = if config.home == Some(HomeMode::Persist) {
        let home_path = config.home_path.clone().unwrap_or_else(|| default_home_path(app_name));
        run_command(
            "sudo",
            &[
                "install",
                "-d",
                "-m",
                "0755",
                "-o",
                uid,
                "-g",
                gid,
                home_path.to_str().ok_or_else(|| "home path is not valid UTF-8".to_string())?,
            ],
        )?;

        container_env.push("BUNKERBOX_PERSIST_HOME=/bunkerbox-persist-home".to_string());
        Some(format!("type=bind,src={},dst=/bunkerbox-persist-home,options=rbind:rw", home_path.display()))
    } else {
        None
    };
    let mut args = Vec::new();

    if config.network == Some(NetworkMode::Bridge) {
        args.push("env");
        args.push("CNI_PATH=/usr/lib/cni");
    }

    args.extend(["ctr", "run", "--runtime", "io.containerd.kata.v2", "--rm", "--tty", "--user", user.as_str()]);

    for value in &container_env {
        args.push("--env");
        args.push(value.as_str());
    }

    if let Some(mount) = &resolv_conf_mount {
        args.push("--mount");
        args.push(mount.as_str());
    }

    args.push("--mount");
    args.push(workspace_mount.as_str());

    if let Some(mount) = &home_mount {
        args.push("--mount");
        args.push(mount.as_str());
    }

    match config.network {
        Some(NetworkMode::Bridge) => args.push("--cni"),
        Some(NetworkMode::Host) => args.push("--net-host"),
        None => {}
    }

    args.push(&config.image);
    args.push(container_name);

    let result = run_command("sudo", &args);

    if bridge_firewall {
        let _ = remove_bridge_egress_firewall();
    }

    if let Some(path) = resolv_conf {
        let _ = fs::remove_file(path);
    }

    result
}

fn default_home_path(app_name: &str) -> PathBuf {
    user_data_dir().join("bunkerbox").join(app_name).join("home")
}

fn user_data_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home).join(".local").join("share");
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(".local").join("share")
}

fn write_resolv_conf() -> Result<PathBuf, String> {
    let host = fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let mut lines = Vec::new();
    let mut nameserver_count = 0;

    for line in host.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("nameserver ") {
            let server = trimmed.split_whitespace().nth(1).unwrap_or_default();
            if server.starts_with("127.") || server == "::1" || server.eq_ignore_ascii_case("localhost") {
                continue;
            }
            nameserver_count += 1;
            lines.push(trimmed.to_string());
            continue;
        }

        if trimmed.starts_with("search ") || trimmed.starts_with("domain ") || trimmed.starts_with("options ") {
            lines.push(trimmed.to_string());
        }
    }

    if nameserver_count == 0 {
        lines.push("nameserver 1.1.1.1".to_string());
        lines.push("nameserver 8.8.8.8".to_string());
    }

    let path = std::env::temp_dir().join(format!("bunkerbox-resolv-{}.conf", std::process::id()));
    fs::write(&path, format!("{}\n", lines.join("\n"))).map_err(|err| format!("failed to write {}: {err}", path.display()))?;

    Ok(path)
}

fn ensure_bridge_egress_firewall(config: &RuntimeConfig, resolv_conf: Option<&Path>) -> Result<(), String> {
    let allow = config.allow.as_ref().ok_or_else(|| "bridge egress firewall requires allow list".to_string())?;

    remove_bridge_egress_firewall()?;
    run_command_allow_failure("sudo", &["iptables", "-N", "BUNKERBOX-EGRESS"])?;
    run_command("sudo", &["iptables", "-F", "BUNKERBOX-EGRESS"])?;
    run_command("sudo", &["iptables", "-I", "FORWARD", "1", "-i", "bunkerbox0", "-j", "BUNKERBOX-EGRESS"])?;
    run_command("sudo", &["iptables", "-A", "BUNKERBOX-EGRESS", "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"])?;

    for server in resolv_conf.map(dns_servers).transpose()?.unwrap_or_default() {
        add_bridge_allow_rule(&server.to_string(), Some("udp"), Some("53"))?;
        add_bridge_allow_rule(&server.to_string(), Some("tcp"), Some("53"))?;
    }

    for destination in resolve_allow_list(allow)? {
        add_bridge_allow_rule(&destination, None, None)?;
    }

    run_command("sudo", &["iptables", "-A", "BUNKERBOX-EGRESS", "-j", "REJECT"])
}

fn remove_bridge_egress_firewall() -> Result<(), String> {
    loop {
        let output = Command::new("sudo")
            .args(["iptables", "-D", "FORWARD", "-i", "bunkerbox0", "-j", "BUNKERBOX-EGRESS"])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .map_err(|err| format!("failed to run sudo: {err}"))?;

        if !output.success() {
            break;
        }
    }

    run_command_allow_failure("sudo", &["iptables", "-F", "BUNKERBOX-EGRESS"])?;
    run_command_allow_failure("sudo", &["iptables", "-X", "BUNKERBOX-EGRESS"])
}

fn add_bridge_allow_rule(destination: &str, protocol: Option<&str>, port: Option<&str>) -> Result<(), String> {
    let mut args = vec!["iptables", "-A", "BUNKERBOX-EGRESS", "-d", destination];

    if let Some(protocol) = protocol {
        args.push("-p");
        args.push(protocol);
    }

    if let Some(port) = port {
        args.push("--dport");
        args.push(port);
    }

    args.push("-j");
    args.push("ACCEPT");
    run_command("sudo", &args)
}

fn dns_servers(path: &Path) -> Result<Vec<IpAddr>, String> {
    let contents = fs::read_to_string(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let mut servers = Vec::new();

    for line in contents.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("nameserver ") {
            continue;
        }

        let server = trimmed.split_whitespace().nth(1).unwrap_or_default();
        if let Ok(ip) = server.parse::<IpAddr>() {
            servers.push(ip);
        }
    }

    Ok(servers)
}

fn resolve_allow_list(allow: &[String]) -> Result<Vec<String>, String> {
    let mut destinations = BTreeSet::new();

    for entry in allow {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        if entry.parse::<IpAddr>().is_ok() || is_ipv4_cidr(entry) {
            destinations.insert(entry.to_string());
            continue;
        }

        if entry.contains(':') {
            return Err(format!("IPv6 allow entries are not supported by iptables firewall yet: {entry}"));
        }

        let lookup = format!("{entry}:443");
        for addr in lookup.to_socket_addrs().map_err(|err| format!("failed to resolve allowed host {entry}: {err}"))? {
            if let IpAddr::V4(ip) = addr.ip() {
                destinations.insert(ip.to_string());
            }
        }
    }

    Ok(destinations.into_iter().collect())
}

fn is_ipv4_cidr(entry: &str) -> bool {
    let Some((addr, prefix)) = entry.split_once('/') else {
        return false;
    };

    addr.parse::<std::net::Ipv4Addr>().is_ok() && prefix.parse::<u8>().is_ok_and(|prefix| prefix <= 32)
}

fn ensure_bridge_cni_config() -> Result<(), String> {
    let path = Path::new("/etc/cni/net.d/10-bunkerbox.conflist");

    if let Ok(contents) = fs::read_to_string(path) {
        if !contents.contains("\"name\": \"bunkerbox\"") {
            return Err(format!("refusing to overwrite unrelated CNI config: {}", path.display()));
        }
    }

    let config = r#"{
  "cniVersion": "0.4.0",
  "name": "bunkerbox",
  "plugins": [
    {
      "type": "bridge",
      "bridge": "bunkerbox0",
      "isGateway": true,
      "ipMasq": true,
      "hairpinMode": true,
      "ipam": {
        "type": "host-local",
        "ranges": [
          [
            { "subnet": "10.247.0.0/24" }
          ]
        ],
        "routes": [
          { "dst": "0.0.0.0/0" }
        ]
      }
    }
  ]
}
"#;

    let temp = std::env::temp_dir().join(format!("bunkerbox-cni-{}.conflist", std::process::id()));
    fs::write(&temp, config).map_err(|err| format!("failed to write {}: {err}", temp.display()))?;

    let result = run_command(
        "sudo",
        &[
            "install",
            "-D",
            "-m",
            "0644",
            temp.to_str().ok_or_else(|| format!("path is not valid UTF-8: {}", temp.display()))?,
            path.to_str().ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?,
        ],
    );

    let _ = fs::remove_file(&temp);
    result
}

fn import_image(config: &RuntimeConfig) -> Result<(), String> {
    run_command_allow_failure("sudo", &["ctr", "images", "rm", &config.image])?;

    run_command(
        "sudo",
        &["ctr", "images", "import", config.oci.to_str().ok_or_else(|| format!("path is not valid UTF-8: {}", config.oci.display()))?],
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

fn current_user_spec() -> Result<String, String> {
    let uid = command_output("id", &["-u"])?;
    let gid = command_output("id", &["-g"])?;
    let uid = uid.trim();
    let gid = gid.trim();

    if uid.is_empty() || gid.is_empty() || !uid.chars().all(|ch| ch.is_ascii_digit()) || !gid.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("failed to determine current uid/gid: {uid}:{gid}"));
    }

    Ok(format!("{uid}:{gid}"))
}

fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run {program}: {err}"))?;

    let stderr = child.stderr.take().ok_or_else(|| format!("failed to capture stderr for {program}"))?;
    let stderr_thread = thread::spawn(move || filter_stderr(stderr));

    let status = child.wait().map_err(|err| format!("failed to wait for {program}: {err}"))?;
    stderr_thread.join().map_err(|_| format!("stderr filter thread panicked for {program}"))??;

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
    let output = Command::new(program).args(args).stderr(Stdio::piped()).output().map_err(|err| format!("failed to run {program}: {err}"))?;

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
