use std::path::Path;

pub struct CMake;

impl super::BuildSystem for CMake {
    fn name(&self) -> &'static str {
        "cmake"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("CMakeLists.txt").exists()
    }
    fn passthrough(&self) -> Vec<String> {
        vec!["cmake *".into(), "make *".into()]
    }
}
