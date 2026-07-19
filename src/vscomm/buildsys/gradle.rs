use std::path::Path;

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
    fn passthrough(&self) -> Vec<String> {
        vec!["./gradlew *".into(), "gradle *".into()]
    }
}
