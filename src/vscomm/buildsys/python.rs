use std::path::Path;

pub struct Python;

impl super::BuildSystem for Python {
    fn name(&self) -> &'static str {
        "python"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("pyproject.toml").exists() || root.join("setup.py").exists() || root.join("setup.cfg").exists()
    }
    fn passthrough(&self) -> Vec<String> {
        vec!["python *".into(), "pip *".into()]
    }
}
