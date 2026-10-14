use std::{env, path::PathBuf, process::Command};

fn run(mut command: Command) {
    let status = command
        .status()
        .expect("Cannot execute calibration build compiler");
    assert!(status.success(), "Offline calibration helper build failed");
}
fn main() {
    println!("cargo:rerun-if-changed=native");
    println!("cargo:rerun-if-env-changed=CXX");
    println!("cargo:rerun-if-env-changed=CC");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
        || env::var("HOST").ok() != env::var("TARGET").ok()
    {
        // This product is Linux-native; unavailable helper stays explicit elsewhere.
        std::fs::write(out.join("gp200-calibration"), []).expect("Write unavailable marker");
        return;
    }
    let mut cc = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()));
    cc.args(["-O2", "-c", "native/ooura/fft4g.c", "-o"])
        .arg(out.join("fft4g.o"));
    run(cc);
    let mut cxx = Command::new(env::var_os("CXX").unwrap_or_else(|| "c++".into()));
    cxx.args([
        "-std=c++20",
        "-O2",
        "-ffp-contract=off",
        "-include",
        "cstdint",
        "-Inative/r8brain",
        "native/helper.cpp",
        "native/clo_refiner.cpp",
        "native/r8brain/r8bbase.cpp",
    ])
    .arg(out.join("fft4g.o"))
    .args(["-o"])
    .arg(out.join("gp200-calibration"));
    run(cxx);
    let executable = std::fs::read(out.join("gp200-calibration")).expect("Read built helper");
    assert!(
        executable.len() > 20
            && &executable[..4] == b"\x7fELF"
            && executable[4] == 2
            && executable[5] == 1
            && u16::from_le_bytes([executable[18], executable[19]]) == 62,
        "Calibration compiler produced an incompatible ELF architecture"
    );
}
