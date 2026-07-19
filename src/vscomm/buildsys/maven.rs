use std::path::Path;

pub struct Maven;

impl super::BuildSystem for Maven {
    fn name(&self) -> &'static str {
        "maven"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("pom.xml").exists()
    }
    fn passthrough(&self) -> Vec<String> {
        vec!["mvn *".into()]
    }
}
