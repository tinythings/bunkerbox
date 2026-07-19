use std::fs;
use std::path::Path;

use super::PassthroughMode;

pub struct GnuMake;

impl super::BuildSystem for GnuMake {
    fn name(&self) -> &'static str {
        "make"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("Makefile").exists() || root.join("makefile").exists() || root.join("GNUmakefile").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["make *".into()],
            PassthroughMode::Paranoid => {
                let phony = Self::read_phony_targets(root);
                if !phony.is_empty() {
                    return phony.into_iter().map(|t| format!("make {t}")).collect();
                }
                vec!["make build".into(), "make test".into(), "make clean".into(), "make check".into(), "make install".into()]
            }
        }
    }
}

impl GnuMake {
    fn read_phony_targets(root: &Path) -> Vec<String> {
        for name in &["Makefile", "makefile", "GNUmakefile"] {
            let path = root.join(name);
            if let Ok(contents) = fs::read_to_string(&path) {
                return Self::parse_phony(&contents);
            }
        }
        Vec::new()
    }

    fn parse_phony(contents: &str) -> Vec<String> {
        let mut targets = Vec::new();
        for line in contents.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix(".PHONY:").or_else(|| trimmed.strip_prefix(".PHONY ")) {
                for t in rest.split_whitespace() {
                    let t = t.trim().trim_matches(':');
                    if !t.is_empty() {
                        targets.push(t.to_string());
                    }
                }
            }
        }
        targets
    }
}
