use std::path::Path;

use super::PassthroughMode;

pub struct Npm;

impl super::BuildSystem for Npm {
    fn name(&self) -> &'static str {
        "npm"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("package.json").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, _root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["npm *".into(), "npx *".into()],
            PassthroughMode::Paranoid => vec!["npm install".into(), "npm test".into(), "npm run build".into(), "npm run lint".into()],
        }
    }
}
