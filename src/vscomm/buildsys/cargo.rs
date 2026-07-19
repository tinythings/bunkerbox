use std::path::Path;

pub struct Cargo;

impl super::BuildSystem for Cargo {
    fn name(&self) -> &'static str {
        "cargo"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("Cargo.toml").exists()
    }
    fn passthrough(&self) -> Vec<String> {
        vec!["cargo *".into()]
    }
}
