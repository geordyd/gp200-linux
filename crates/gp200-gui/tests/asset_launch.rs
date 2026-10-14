use gp200_gui::LaunchOptions;
#[test]
fn ir_metadata_startup_read_needs_explicit_backend_and_never_enables_a_write() {
    assert!(LaunchOptions::parse(["--read-ir-metadata".into()]).is_err());
    for backend in ["--demo", "--connect"] {
        let options = LaunchOptions::parse([backend.into(), "--read-ir-metadata".into()]).unwrap();
        assert!(options.read_ir_metadata);
        assert_eq!(options.connect, backend == "--connect");
        assert_eq!(options.demo, backend == "--demo");
        assert!(!options.scan_bank && !options.read_globals);
    }
    assert!(
        LaunchOptions::parse([
            "--demo".into(),
            "--connect".into(),
            "--read-ir-metadata".into()
        ])
        .is_err()
    );
    let passive = LaunchOptions::parse(["--view".into(), "assets".into()]).unwrap();
    assert!(!passive.read_ir_metadata && !passive.connect && !passive.demo);
}
