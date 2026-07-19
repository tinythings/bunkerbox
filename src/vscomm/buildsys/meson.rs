use std::path::Path;

use super::PassthroughMode;

pub struct Meson;

impl super::BuildSystem for Meson {
    fn name(&self) -> &'static str {
        "meson"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("meson.build").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, _root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["meson *".into(), "ninja *".into()],
            PassthroughMode::Paranoid => vec!["meson setup".into(), "meson compile".into(), "meson test".into(), "ninja".into()],
        }
    }
}
