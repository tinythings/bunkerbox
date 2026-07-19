use std::path::Path;

/// A build system that can be auto-detected in a project.
pub trait BuildSystem {
    fn name(&self) -> &'static str;
    fn detect(&self, root: &Path) -> bool;
    fn passthrough(&self) -> Vec<String>;
}

/// Scan a project root for known build systems and return passthrough entries.
pub fn scan(root: &Path) -> Vec<String> {
    let detectors: Vec<Box<dyn BuildSystem>> = vec![
        Box::new(gnumake::GnuMake),
        Box::new(cargo::Cargo),
        Box::new(npm::Npm),
        Box::new(python::Python),
        Box::new(go::Go),
        Box::new(cmake::CMake),
        Box::new(gradle::Gradle),
        Box::new(maven::Maven),
        Box::new(meson::Meson),
    ];

    let mut entries = Vec::new();
    for detector in detectors {
        if detector.detect(root) {
            entries.extend(detector.passthrough());
        }
    }
    entries
}

mod cargo;
mod cmake;
mod gnumake;
mod go;
mod gradle;
mod maven;
mod meson;
mod npm;
mod python;
