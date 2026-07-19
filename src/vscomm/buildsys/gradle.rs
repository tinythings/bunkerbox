use std::path::Path;

use super::PassthroughMode;

pub struct Gradle;

impl super::BuildSystem for Gradle {
    fn name(&self) -> &'static str {
        "gradle"
    }
    fn detect(&self, root: &Path) -> bool {
        root.join("build.gradle").exists()
            || root.join("build.gradle.kts").exists()
            || root.join("settings.gradle").exists()
            || root.join("settings.gradle.kts").exists()
            || root.join("gradlew").exists()
    }
    fn passthrough(&self, mode: PassthroughMode, _root: &Path) -> Vec<String> {
        match mode {
            PassthroughMode::Relaxed => vec!["./gradlew *".into(), "gradle *".into()],
            PassthroughMode::Paranoid => {
                vec!["./gradlew build".into(), "./gradlew test".into(), "gradle build".into(), "gradle test".into()]
            }
        }
    }
}
