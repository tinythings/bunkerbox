use std::path::Path;

use super::PassthroughMode;

pub struct CMake;

impl super::BuildSystem for CMake {
    fn name(&self) -> &'static str {
        "cmake"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("CMakeLists.txt").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, _root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["cmake *".into(), "make *".into()],
            PassthroughMode::Paranoid => vec!["cmake".into(), "make build".into(), "make test".into(), "make install".into()],
        }
    }
}
