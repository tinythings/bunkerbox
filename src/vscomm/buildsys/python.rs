use std::path::Path;

use super::PassthroughMode;

pub struct Python;

impl super::BuildSystem for Python {
    fn name(&self) -> &'static str {
        "python"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("pyproject.toml").exists() || root.join("setup.py").exists() || root.join("setup.cfg").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, _root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["python *".into(), "pip *".into()],
            PassthroughMode::Paranoid => vec!["python".into()],
        }
    }
}
