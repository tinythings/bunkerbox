use super::*;

fn config() -> ImageConfig {
    ImageConfig {
        name: "test".into(),
        image: "test:latest".into(),
        output: "test.oci".into(),
        command: Vec::new(),
        overwrite: false,
        build_args: BTreeMap::new(),
        hooks: ImageHooks::default(),
        files: Vec::new(),
        runtime: None,
        containerfile: "FROM scratch".into(),
    }
}

#[test]
fn image_entrypoint_installs_remote_make_before_local_vscomm_links() {
    let script = build_entrypoint(&config()).unwrap();
    let remote = script.find("bunkerbox-remote\" install").unwrap();
    let vscomm = script.find("bunkerbox-vscomm\" install").unwrap();
    assert!(remote < vscomm);
}
