use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=res/riv.rc");
    println!("cargo:rerun-if-changed=res/resource.h");
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

    // C/C++ fallback 코덱 정적 링크 (PORTING_PLAN §5·§6.2) —
    // deps/build_deps.sh 산출물(버전 접미사 포함)을 전부 링크한다
    let manifest_directory = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let codec_library_directory = manifest_directory.join("deps/prefix/lib");
    assert!(
        codec_library_directory.join("riv_exr_shim.lib").exists(),
        "fallback codec libraries missing - run deps/build_deps.sh first"
    );
    println!(
        "cargo:rerun-if-changed={}",
        codec_library_directory.display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        codec_library_directory.display()
    );
    for entry in std::fs::read_dir(&codec_library_directory)
        .expect("codec library directory readable")
        .flatten()
    {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if let Some(library_name) = file_name.strip_suffix(".lib") {
            println!("cargo:rustc-link-lib=static={library_name}");
        }
    }
    // C++ 코덱(libheif·OpenEXR)용 MSVC 정적 C++ 런타임
    println!("cargo:rustc-link-lib=libcpmt");
}
