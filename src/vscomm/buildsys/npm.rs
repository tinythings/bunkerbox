use std::path::Path;

pub struct Npm;

impl super::BuildSystem for Npm {
    fn name(&self) -> &'static str {
        "npm"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("package.json").exists()
    }
    fn passthrough(&self) -> Vec<String> {
        vec!["npm *".into(), "npx *".into()]
    }
}
