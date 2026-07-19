use std::path::Path;

use super::PassthroughMode;

pub struct Cargo;

impl super::BuildSystem for Cargo {
    fn name(&self) -> &'static str {
        "cargo"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("Cargo.toml").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, _root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["cargo *".into()],
            PassthroughMode::Paranoid => vec![
                "cargo check".into(),
                "cargo build".into(),
                "cargo test".into(),
                "cargo clippy".into(),
                "cargo fmt".into(),
                "cargo doc".into(),
                "cargo run".into(),
            ],
        }
    }
}
