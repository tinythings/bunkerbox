use crate::runtime::{HomeMode, NetworkMode, RuntimeConfig};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use aes_gcm::aead::Aead;
use aes_gcm::aead::consts::U12;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::Rng;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

const BRIDGE_SUBNET: &str = "10.247.0.0/24";
const BRIDGE_NAME: &str = "bunkerbox0";

pub fn run(config: &RuntimeConfig, workspace: &Path, container_name: &str, _share_dir: &Path, app_name: &str) -> Result<(), String> {
    if !config.oci.is_file() {
        return Err(format!("OCI archive not found: {}", config.oci.display()));
    }

    check_containerd_version()?;

    let home_path: Option<PathBuf> = if config.home == Some(HomeMode::Persist) {
        Some(config.home_path.clone().unwrap_or_else(|| default_home_path(app_name)))
    } else {
        None
    };

    let encrypt_patterns: &[String] = config.encrypt.as_deref().unwrap_or(&[]);
    let passphrase = if !encrypt_patterns.is_empty() {
        if let Ok(key) = std::env::var("BUNKERBOX_ENCRYPT_KEY") {
            key
        } else {
            read_passphrase()?
        }
    } else {
        String::new()
    };

    if !passphrase.is_empty() {
        if let Some(ref hp) = home_path {
            if hp.is_dir() {
                unseal_home(hp, &passphrase)?;
            }
        }
    }

    let user = current_user_spec()?;
    let Some((uid, gid)) = user.split_once(':') else {
        return Err(format!("invalid current user spec: {user}"));
    };

    let session_mb = config.session_mb();
    let session_dir: Option<PathBuf> = if session_mb > 0 {
        if let Some(ref hp) = home_path {
            Some(setup_session(hp, session_mb, container_name, uid, gid)?)
        } else {
            None
        }
    } else {
        None
    };

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
    let mut container_env = Vec::new();
    let home_mount = if let Some(ref hp) = home_path {
        let src = if let Some(ref sd) = session_dir {
            sd.display().to_string()
        } else {
            run_command(
                "sudo",
                &["install", "-d", "-m", "0755", "-o", uid, "-g", gid, hp.to_str().ok_or_else(|| "home path is not valid UTF-8".to_string())?],
            )?;
            hp.display().to_string()
        };

        container_env.push("BUNKERBOX_PERSIST_HOME=/bunkerbox-persist-home".to_string());
        Some(format!("type=bind,src={},dst=/bunkerbox-persist-home,options=rbind:rw", src))
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

    let term_env;
    if let Ok(term) = std::env::var("TERM") {
        term_env = format!("TERM={term}");
        args.push("--env");
        args.push(&term_env);
    }

    let colorterm_env;
    if let Ok(ct) = std::env::var("COLORTERM") {
        colorterm_env = format!("COLORTERM={ct}");
        args.push("--env");
        args.push(&colorterm_env);
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

    if let Some(ref sd) = session_dir {
        if let Some(ref hp) = home_path {
            teardown_session(hp, sd);
        }
    }

    if !passphrase.is_empty() {
        if let Some(ref hp) = home_path {
            if hp.is_dir() {
                seal_home(hp, encrypt_patterns, &passphrase)?;
            }
        }
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
    run_command("sudo", &["modprobe", "br_netfilter"])?;
    run_command("sudo", &["sysctl", "-w", "net.bridge.bridge-nf-call-iptables=1"])?;
    run_command_allow_failure("sudo", &["iptables", "-N", "BUNKERBOX-EGRESS"])?;
    run_command("sudo", &["iptables", "-F", "BUNKERBOX-EGRESS"])?;
    run_command("sudo", &["iptables", "-I", "FORWARD", "1", "-s", BRIDGE_SUBNET, "-j", "BUNKERBOX-EGRESS"])?;
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
            .args(["iptables", "-D", "FORWARD", "-s", BRIDGE_SUBNET, "-j", "BUNKERBOX-EGRESS"])
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

    let config = format!(
        r#"{{
  "cniVersion": "0.4.0",
  "name": "bunkerbox",
  "plugins": [
    {{
      "type": "bridge",
      "bridge": "{bridge}",
      "isGateway": true,
      "ipMasq": true,
      "hairpinMode": true,
      "ipam": {{
        "type": "host-local",
        "ranges": [
          [
            {{ "subnet": "{subnet}" }}
          ]
        ],
        "routes": [
          {{ "dst": "0.0.0.0/0" }}
        ]
      }}
    }}
  ]
}}
"#,
        bridge = BRIDGE_NAME,
        subnet = BRIDGE_SUBNET,
    );

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
    let _ = run_command_allow_failure("sudo", &["ctr", "tasks", "kill", "--signal", "SIGKILL", container_name]);
    let _ = run_command_allow_failure("sudo", &["ctr", "tasks", "delete", "--force", container_name]);
    let _ = run_command_allow_failure("sudo", &["ctr", "containers", "rm", container_name]);
    let _ = run_command_allow_failure(
        "sudo",
        &["rm", "-f", &format!("/var/lib/cni/networks/{BRIDGE_NAME}/{container_name}")],
    );
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

fn confirm(prompt: &str) -> bool {
    use std::io::{self, Read, Write};
    let mut stdout = io::stdout();
    let _ = write!(stdout, "{prompt} [y/N] ");
    let _ = stdout.flush();

    let mut buf = [0u8; 1];
    if io::stdin().read_exact(&mut buf).is_err() {
        return false;
    }

    buf[0] == b'y' || buf[0] == b'Y'
}

fn read_passphrase() -> Result<String, String> {
    rpassword::prompt_password("Bunkerbox passphrase: ").map_err(|err| format!("failed to read passphrase: {err}"))
}

fn derive_key(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, 100_000, &mut key);
    key
}

fn encrypt_to_vec(passphrase: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut rng = rand::thread_rng();
    let mut salt = [0u8; 16];
    rng.fill(&mut salt);
    let mut nonce_bytes = [0u8; 12];
    rng.fill(&mut nonce_bytes);

    let key = derive_key(passphrase, &salt);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("cipher init: {e}"))?;
    let nonce = Nonce::<U12>::from_slice(&nonce_bytes);

    let ciphertext = cipher.encrypt(nonce, plaintext).map_err(|_| "encryption failed".to_string())?;

    let mut result = Vec::with_capacity(28 + ciphertext.len());
    result.extend_from_slice(&salt);
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(&result).into_bytes())
}

fn decrypt_from_slice(passphrase: &str, encoded: &[u8]) -> Result<Vec<u8>, String> {
    let data = BASE64.decode(encoded).map_err(|_| "base64 decode failed".to_string())?;

    if data.len() < 28 {
        return Err("encrypted data too short".to_string());
    }

    let salt = &data[..16];
    let nonce_bytes = &data[16..28];
    let ciphertext = &data[28..];

    let key = derive_key(passphrase, salt);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("cipher init: {e}"))?;
    let nonce = Nonce::<U12>::from_slice(nonce_bytes);

    cipher.decrypt(nonce, ciphertext).map_err(|_| "Failed to open credentials vault".to_string())
}

fn walk_files(base: &Path, dir: &Path, f: &mut dyn FnMut(&Path, &Path) -> Result<(), String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            walk_files(base, &path, f)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            f(&path, rel)?;
        }
    }
    Ok(())
}

fn unseal_home(home: &Path, passphrase: &str) -> Result<(), String> {
    walk_files(home, home, &mut |path, _rel| {
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            return Ok(());
        };
        if !filename.ends_with(".enc-cipher") {
            return Ok(());
        }

        let encrypted = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let plaintext = match decrypt_from_slice(passphrase, &encrypted) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                if confirm("Reset current authentication to default?") {
                    fs::remove_file(path).map_err(|err| format!("remove {}: {err}", path.display()))?;
                    return Ok(());
                }
                return Err(e);
            }
        };

        let path_str = path.to_string_lossy();
        let target = PathBuf::from(path_str.strip_suffix(".enc-cipher").unwrap_or(&path_str));

        fs::write(&target, &plaintext).map_err(|e| format!("write {}: {e}", target.display()))?;
        fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))?;

        Ok(())
    })
}

fn seal_home(home: &Path, patterns: &[String], passphrase: &str) -> Result<(), String> {
    let compiled: Vec<glob::Pattern> =
        patterns.iter().map(|p| glob::Pattern::new(p)).collect::<Result<Vec<_>, _>>().map_err(|e| format!("invalid glob pattern: {e}"))?;

    walk_files(home, home, &mut |path, rel| {
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            return Ok(());
        };
        if filename.ends_with(".enc-cipher") {
            return Ok(());
        }

        let rel_str = rel.to_string_lossy();
        if !compiled.iter().any(|p| p.matches(&rel_str)) {
            return Ok(());
        }

        let plaintext = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let encrypted = encrypt_to_vec(passphrase, &plaintext)?;

        let enc_path = PathBuf::from(format!("{}.enc-cipher", path.display()));
        fs::write(&enc_path, &encrypted).map_err(|e| format!("write {}: {e}", enc_path.display()))?;
        fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))?;

        Ok(())
    })
}

fn setup_session(home_path: &Path, session_mb: u32, container_name: &str, uid: &str, gid: &str) -> Result<PathBuf, String> {
    let bunker_dir = home_path.join(".bunker");
    let session_img = bunker_dir.join("session.img");
    let session_dir = PathBuf::from(format!("/tmp/bunkerbox-session-{}", container_name));

    if session_img.exists() {
        eprintln!("bunkerbox: found leftover session.img, recovering...");
        let _ = run_command_allow_failure("sudo", &["mkdir", "-p", session_dir.to_str().unwrap()]);

        let _ = run_command("sudo", &["e2fsck", "-p", &format!("{}", session_img.display())]);

        if run_command("sudo", &["mount", "-o", "loop", &format!("{}", session_img.display()), &format!("{}", session_dir.display())]).is_ok() {
            let _ = run_command_allow_failure("sudo", &["chown", &format!("{}:{}", uid, gid), &format!("{}", session_dir.display())]);
            let _ = run_command("cp", &["-Rup", &format!("{}/.", session_dir.display()), &format!("{}", home_path.display())]);
            let _ = run_command_allow_failure("sudo", &["umount", &format!("{}", session_dir.display())]);
        } else {
            eprintln!("bunkerbox: warning: could not mount leftover session.img, discarding");
        }

        let _ = run_command_allow_failure("sudo", &["rm", "-rf", &format!("{}", session_dir.display())]);
        let _ = run_command_allow_failure("rm", &["-f", &format!("{}", session_img.display())]);
    }

    run_command_allow_failure("mkdir", &["-p", &format!("{}", bunker_dir.display())])?;
    run_command("dd", &["if=/dev/zero", &format!("of={}", session_img.display()), "bs=1M", &format!("count={}", session_mb)])?;
    run_command("mke2fs", &["-F", "-t", "ext4", &format!("{}", session_img.display())])?;
    run_command_allow_failure("sudo", &["mkdir", "-p", &format!("{}", session_dir.display())])?;
    run_command("sudo", &["mount", "-o", "loop", &format!("{}", session_img.display()), &format!("{}", session_dir.display())])?;
    run_command("sudo", &["chown", &format!("{}:{}", uid, gid), &format!("{}", session_dir.display())])?;
    let _ = run_command("sudo", &["rm", "-rf", &format!("{}/lost+found", session_dir.display())]);

    let _ = run_command("cp", &["-a", &format!("{}/.", home_path.display()), &format!("{}/", session_dir.display())]);

    Ok(session_dir)
}

fn teardown_session(home_path: &Path, session_dir: &Path) {
    let _ = run_command("cp", &["-Rup", &format!("{}/.", session_dir.display()), &format!("{}", home_path.display())]);

    let _ = run_command_allow_failure("sudo", &["umount", &format!("{}", session_dir.display())]);

    let session_img = home_path.join(".bunker").join("session.img");
    let _ = run_command_allow_failure("rm", &["-f", &format!("{}", session_img.display())]);
    let _ = run_command_allow_failure("sudo", &["rmdir", &format!("{}", session_dir.display())]);
}

fn check_containerd_version() -> Result<(), String> {
    if std::env::var("BUNKERBOX_SKIP_CONTAINERD_CHECK").is_ok() {
        return Ok(());
    }

    let output = Command::new("containerd")
        .arg("--version")
        .output()
        .map_err(|err| format!("failed to run containerd --version: {err}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    for token in stdout.split_whitespace() {
        let stripped = token.strip_prefix('v').unwrap_or(token);
        let parts: Vec<&str> = stripped.split('.').collect();
        if parts.len() != 3 {
            continue;
        }
        let Ok(major): Result<u32, _> = parts[0].parse() else { continue; };
        let Ok(minor): Result<u32, _> = parts[1].parse() else { continue; };
        let Ok(patch): Result<u32, _> = parts[2].parse() else { continue; };

        if (major, minor, patch) < (2, 2, 5) {
            return Err(format!(
                "containerd {major}.{minor}.{patch} is too old. Version >= 2.2.5 required for Kata networking.\n\
                 Install from: https://github.com/containerd/containerd/releases\n\
                 To skip: export BUNKERBOX_SKIP_CONTAINERD_CHECK=1",
            ));
        }
        return Ok(());
    }

    eprintln!("WARNING: could not parse containerd version. Proceeding anyway.");
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
