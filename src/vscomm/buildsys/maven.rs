use std::path::Path;

use super::PassthroughMode;

pub struct Maven;

impl super::BuildSystem for Maven {
    fn name(&self) -> &'static str {
        "maven"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("pom.xml").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, _root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["mvn *".into()],
            PassthroughMode::Paranoid => vec!["mvn compile".into(), "mvn test".into(), "mvn package".into()],
        }
    }
}
