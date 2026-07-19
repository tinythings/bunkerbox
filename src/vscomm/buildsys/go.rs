use std::path::Path;

use super::PassthroughMode;

pub struct Go;

impl super::BuildSystem for Go {
    fn name(&self) -> &'static str {
        "go"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("go.mod").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, _root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["go *".into()],
            PassthroughMode::Paranoid => vec!["go build".into(), "go test".into(), "go vet".into(), "go fmt".into()],
        }
    }
}
