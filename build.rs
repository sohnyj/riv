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
    println!("cargo:rerun-if-changed={}", shaders::SHADER_DIRECTORY);

    let output_directory = PathBuf::from(env::var("OUT_DIR").unwrap());
    write_blue_noise_table(&output_directory);

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

    // The package version is the single source: manifest substitution + VERSIONINFO.
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let four_part_version = format!("{version}.0");
    let processed_manifest = write_manifest(&output_directory, &four_part_version);
    let generated_source = write_resource_script(
        &output_directory,
        &processed_manifest,
        &version,
        &four_part_version,
    );
    let compiled_resource = compile_resources(&output_directory, &generated_source);
    println!("cargo:rustc-link-arg-bins={}", compiled_resource.display());

    for library_directory in XWIN_LIBRARY_DIRECTORIES {
        println!("cargo:rustc-link-search=native={xwin_root}/{library_directory}");
    }
    link_codec_libraries();
}

/// The texels are a pure function of the generator, so an up-to-date table is kept.
fn write_blue_noise_table(output_directory: &Path) {
    let blue_noise_table = output_directory.join("blue_noise.bin");
    let blue_noise_source = shaders::blue_noise_source();
    if is_stale(&blue_noise_table, &[&blue_noise_source]) {
        blue_noise::write_table(&blue_noise_table);
    }
}

/// VERSIONINFO language and code page: en-US, Unicode.
const LANGUAGE_ID: u16 = 0x0409;
const CODE_PAGE: u16 = 0x04B0;

/// The manifest with the four-part version substituted; the path llvm-rc embeds.
fn write_manifest(output_directory: &Path, four_part_version: &str) -> PathBuf {
    let manifest_template = std::fs::read_to_string("res/riv.manifest").expect("manifest readable");
    let processed_manifest = output_directory.join("riv.manifest");
    std::fs::write(
        &processed_manifest,
        manifest_template.replace("@VERSION@", four_part_version),
    )
    .expect("manifest writable");
    processed_manifest
}

/// The resource script that includes riv.rc and adds the manifest and VERSIONINFO.
fn write_resource_script(
    output_directory: &Path,
    manifest: &Path,
    version: &str,
    four_part_version: &str,
) -> PathBuf {
    let comma_separated_version = four_part_version.replace('.', ",");
    let language_block = format!("{LANGUAGE_ID:04X}{CODE_PAGE:04X}");
    let language_id = format!("0x{LANGUAGE_ID:04X}");
    let code_page = format!("0x{CODE_PAGE:04X}");
    // 24 = RT_MANIFEST, 1 = CREATEPROCESS_MANIFEST_RESOURCE_ID
    let generated_source = output_directory.join("app.rc");
    let generated = format!(
        concat!(
            "#include \"riv.rc\"\n",
            "1 24 \"{manifest}\"\n",
            "1 VERSIONINFO\n",
            "FILEVERSION {comma_separated_version}\n",
            "PRODUCTVERSION {comma_separated_version}\n",
            "FILEOS 0x40004L\n", // VOS_NT_WINDOWS32
            "FILETYPE 0x1L\n",   // VFT_APP
            "BEGIN\n",
            "  BLOCK \"StringFileInfo\"\n",
            "  BEGIN\n",
            "    BLOCK \"{language_block}\"\n",
            "  BEGIN\n",
            "      VALUE \"FileDescription\", \"{description}\"\n",
            "      VALUE \"FileVersion\", \"{version}\"\n",
            "      VALUE \"ProductName\", \"riv\"\n",
            "      VALUE \"ProductVersion\", \"{version}\"\n",
            "      VALUE \"OriginalFilename\", \"riv.exe\"\n",
            "      VALUE \"LegalCopyright\", \"Licensed under GPLv3\"\n",
            "    END\n",
            "  END\n",
            "  BLOCK \"VarFileInfo\"\n",
            "  BEGIN\n",
            "    VALUE \"Translation\", {language_id}, {code_page}\n",
            "  END\n",
            "END\n",
        ),
        manifest = manifest.display(),
        comma_separated_version = comma_separated_version,
        version = version,
        language_block = language_block,
        language_id = language_id,
        code_page = code_page,
        description = env::var("CARGO_PKG_DESCRIPTION").unwrap(),
    );
    std::fs::write(&generated_source, generated).expect("generated rc writable");
    generated_source
}

/// Runs llvm-rc over the script; the compiled .res the binary links.
fn compile_resources(output_directory: &Path, generated_source: &Path) -> PathBuf {
    let compiled_resource = output_directory.join("riv.res");
    let status = Command::new("llvm-rc")
        .args(["/I", "res", "/FO"])
        .arg(&compiled_resource)
        .arg(generated_source)
        .status()
        .expect("failed to run llvm-rc");
    assert!(status.success(), "llvm-rc failed with {status}");
    compiled_resource
}

/// Links every static library produced by deps/build_deps.sh, plus the static C++ runtime.
fn link_codec_libraries() {
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
