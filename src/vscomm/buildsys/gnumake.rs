use std::path::Path;

pub struct GnuMake;

impl super::BuildSystem for GnuMake {
    fn name(&self) -> &'static str {
        "make"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("Makefile").exists() || root.join("makefile").exists() || root.join("GNUmakefile").exists()
    }
    fn passthrough(&self) -> Vec<String> {
        vec!["make *".into()]
    }
}
