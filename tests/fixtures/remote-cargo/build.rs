use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let cargo = env::var_os("CARGO").expect("Cargo must provide CARGO to build scripts");
    let cargo_path = PathBuf::from(&cargo);
    assert_ne!(cargo_path.file_name().and_then(|name| name.to_str()), Some("bunkerbox-remote"));
    assert_ne!(cargo_path, PathBuf::from("/usr/local/bunkerbox/bin/cargo"));
    let version = Command::new(&cargo).arg("--version").output().expect("target Cargo must execute nested --version");
    assert!(version.status.success(), "target Cargo --version failed");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR"));
    fs::write(out_dir.join("build_marker.txt"), "bunkerbox-cargo-fixture-build-script\n").expect("write Cargo fixture build marker");
    let artifact = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"))
        .join("target/debug/bunkerbox-cargo-fixture-artifact.txt");
    fs::write(artifact, "bunkerbox-cargo-fixture-artifact\n").expect("write Cargo fixture artifact");
    println!("cargo:warning=bunkerbox-cargo-fixture-build-script");
}
