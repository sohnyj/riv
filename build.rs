//! Build script: blue-noise table, HLSL blobs, resources, and the static codec links.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "res/shaders/blue_noise.rs"]
mod blue_noise;
#[path = "res/shaders/compile.rs"]
mod shaders;

/// xwin splat layout, shared by the crate link search and the shader compiler build.
const XWIN_LIBRARY_DIRECTORIES: [&str; 3] =
    ["crt/lib/x86_64", "sdk/lib/um/x86_64", "sdk/lib/ucrt/x86_64"];

/// True when the output is missing or older than an input it is generated from.
fn is_stale(output: &Path, inputs: &[&Path]) -> bool {
    let Ok(output_time) = std::fs::metadata(output).and_then(|metadata| metadata.modified()) else {
        return true;
    };
    inputs.iter().any(|input| {
        std::fs::metadata(input)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|input_time| input_time > output_time)
    })
}

fn main() {
    println!("cargo:rerun-if-changed=res/shaders");

    let output_directory = PathBuf::from(env::var("OUT_DIR").unwrap());
    // The texels are a pure function of the generator, so an up-to-date table is kept.
    let blue_noise_table = output_directory.join("blue_noise.bin");
    if is_stale(&blue_noise_table, &[Path::new("res/shaders/blue_noise.rs")]) {
        blue_noise::write_table(&blue_noise_table);
    }

    // xwin CRT/SDK import libraries; override the splat location with XWIN_ROOT.
    println!("cargo:rerun-if-env-changed=XWIN_ROOT");
    let xwin_root = env::var("XWIN_ROOT").unwrap_or_else(|_| {
        let home = env::var("HOME").expect("HOME set");
        format!("{home}/.xwin")
    });
    shaders::compile_all(&output_directory, &xwin_root);

    println!("cargo:rerun-if-changed=res/riv.rc");
    println!("cargo:rerun-if-changed=res/resource.h");
    println!("cargo:rerun-if-changed=res/riv.manifest");
    println!("cargo:rerun-if-changed=res/riv.ico");

    let compiled_resource = output_directory.join("riv.res");

    // The package version is the single source: manifest substitution + VERSIONINFO.
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let four_part = format!("{version}.0");
    let numeric = four_part.replace('.', ",");
    let manifest_template = std::fs::read_to_string("res/riv.manifest").expect("manifest readable");
    let processed_manifest = output_directory.join("riv.manifest");
    std::fs::write(
        &processed_manifest,
        manifest_template.replace("@VERSION@", &four_part),
    )
    .expect("manifest writable");
    // 24 = RT_MANIFEST, 1 = CREATEPROCESS_MANIFEST_RESOURCE_ID
    let generated_source = output_directory.join("app.rc");
    let generated = format!(
        concat!(
            "#include \"riv.rc\"\n",
            "1 24 \"{manifest}\"\n",
            "1 VERSIONINFO\n",
            "FILEVERSION {numeric}\n",
            "PRODUCTVERSION {numeric}\n",
            "FILEOS 0x40004L\n", // VOS_NT_WINDOWS32
            "FILETYPE 0x1L\n",   // VFT_APP
            "BEGIN\n",
            "  BLOCK \"StringFileInfo\"\n",
            "  BEGIN\n",
            "    BLOCK \"040904B0\"\n", // en-US, Unicode
            "  BEGIN\n",
            "      VALUE \"FileDescription\", \"riv image viewer\"\n",
            "      VALUE \"FileVersion\", \"{version}\"\n",
            "      VALUE \"ProductName\", \"riv\"\n",
            "      VALUE \"ProductVersion\", \"{version}\"\n",
            "      VALUE \"OriginalFilename\", \"riv.exe\"\n",
            "      VALUE \"LegalCopyright\", \"Licensed under GPLv3\"\n",
            "    END\n",
            "  END\n",
            "  BLOCK \"VarFileInfo\"\n",
            "  BEGIN\n",
            "    VALUE \"Translation\", 0x0409, 0x04B0\n",
            "  END\n",
            "END\n",
        ),
        manifest = processed_manifest.display(),
        numeric = numeric,
        version = version,
    );
    std::fs::write(&generated_source, generated).expect("generated rc writable");

    let status = Command::new("llvm-rc")
        .args(["/I", "res", "/FO"])
        .arg(&compiled_resource)
        .arg(&generated_source)
        .status()
        .expect("failed to run llvm-rc");
    assert!(status.success(), "llvm-rc failed with {status}");

    println!("cargo:rustc-link-arg-bins={}", compiled_resource.display());

    for library_directory in XWIN_LIBRARY_DIRECTORIES {
        println!("cargo:rustc-link-search=native={xwin_root}/{library_directory}");
    }

    // Link every static library produced by deps/build_deps.sh.
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
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if let Some(library_name) = file_name.strip_suffix(".lib") {
            println!("cargo:rustc-link-lib=static={library_name}");
        } else if file_name.starts_with("lib") && file_name.ends_with(".a") {
            // Meson archives keep their Unix name; verbatim hands lld-link the literal file.
            println!("cargo:rustc-link-lib=static:+verbatim={file_name}");
        }
    }
    // Static MSVC C++ runtime for the C++ codecs (libheif, OpenEXR).
    println!("cargo:rustc-link-lib=libcpmt");
}
