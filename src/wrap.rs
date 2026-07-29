/// Generates a POSIX shell init script that wraps each command
/// with `bunkerbox-status` progress reporting and runs them sequentially.
/// Falls through to an interactive shell after the command list.
pub fn generate_init_script(commands: &[String]) -> String {
    let mut script = String::from(
        "#!/bin/sh\n\
         export PATH=/usr/local/bunkerbox/bin:/bunkerbox-tools:$PATH\n\
         \n",
    );

    script.push_str("bunkerbox-vscomm install 2>/dev/null\n");
    script.push_str("cd /workspace\n\n");

    let count = commands.len();
    for (i, cmd) in commands.iter().enumerate() {
        let label = truncate_label(cmd, 60);
        script.push_str(&format!("bunkerbox-status status set \"Running: {label}\"\n"));
        if i == count.saturating_sub(1) {
            script.push_str("bunkerbox-status popup hide \"SEC_2\"\n");
        }
        script.push_str(&format!("{cmd}\n"));
        script.push_str(&format!("bunkerbox-status status set \"Done: {label}\"\n\n"));
    }

    script
}

fn truncate_label(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
