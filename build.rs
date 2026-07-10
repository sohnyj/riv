use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=res/riv.rc");
    println!("cargo:rerun-if-changed=res/riv.manifest");
    println!("cargo:rerun-if-changed=res/riv.ico");

    let output_directory = PathBuf::from(env::var("OUT_DIR").unwrap());
    let compiled_resource = output_directory.join("riv.res");

    let status = Command::new("llvm-rc")
        .args(["/I", "res", "/FO"])
        .arg(&compiled_resource)
        .arg("res/riv.rc")
        .status()
        .expect("failed to run llvm-rc");
    assert!(status.success(), "llvm-rc failed with {status}");

    println!("cargo:rustc-link-arg-bins={}", compiled_resource.display());
}
