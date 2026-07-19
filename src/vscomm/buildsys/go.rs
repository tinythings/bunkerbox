use std::path::Path;

pub struct Go;

impl super::BuildSystem for Go {
    fn name(&self) -> &'static str {
        "go"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("go.mod").exists()
    }
    fn passthrough(&self) -> Vec<String> {
        vec!["go *".into()]
    }
}
