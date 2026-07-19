use std::path::Path;

pub struct Meson;

impl super::BuildSystem for Meson {
    fn name(&self) -> &'static str {
        "meson"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("meson.build").exists()
    }
    fn passthrough(&self) -> Vec<String> {
        vec!["meson *".into(), "ninja *".into()]
    }
}
