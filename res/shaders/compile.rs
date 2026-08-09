//! Build-side HLSL compilation: clang-cl builds the helper, wine runs it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Pixel shaders; each includes ps_shared.hlsl for the bindings and dither math.
const PIXEL_SHADERS: [&str; 4] = ["copy", "ordered", "fruit", "gain_apply"];
const VERTEX_SHADER: &str = "fullscreen_triangle";
const SHADER_DIRECTORY: &str = "res/shaders";

pub fn compile_all(output_directory: &Path, xwin_root: &str) {
    let compiler = build_compiler(output_directory, xwin_root);
    compile(&compiler, VERTEX_SHADER, "vs_5_0", output_directory);
    for name in PIXEL_SHADERS {
        compile(&compiler, name, "ps_5_0", output_directory);
    }
}

/// Builds the helper against the xwin SDK; d3dcompiler_47 resolves under wine.
fn build_compiler(output_directory: &Path, xwin_root: &str) -> PathBuf {
    let executable = output_directory.join("shader_compiler.exe");
    let source = PathBuf::from(format!("{SHADER_DIRECTORY}/compiler.c"));
    if !crate::is_stale(&executable, &[&source]) {
        return executable;
    }
    let status = Command::new("clang-cl")
        .args([
            "--target=x86_64-pc-windows-msvc",
            "-fuse-ld=lld",
            "/nologo",
            "/O2",
            "/D_CRT_SECURE_NO_WARNINGS",
        ])
        .args([
            format!("-imsvc{xwin_root}/crt/include"),
            format!("-imsvc{xwin_root}/sdk/include/ucrt"),
            format!("-imsvc{xwin_root}/sdk/include/um"),
            format!("-imsvc{xwin_root}/sdk/include/shared"),
        ])
        .arg(&source)
        .arg(format!("/Fo{}\\", output_directory.display()))
        .arg(format!("/Fe{}", executable.display()))
        .arg("/link")
        .args(
            crate::XWIN_LIBRARY_DIRECTORIES
                .map(|directory| format!("/libpath:{xwin_root}/{directory}")),
        )
        .arg("d3dcompiler.lib")
        .status()
        .expect("failed to run clang-cl for the shader compiler");
    assert!(
        status.success(),
        "shader compiler build failed with {status}"
    );
    executable
}

/// Runs from the shader directory so `#include` resolves against it.
fn compile(compiler: &Path, name: &str, profile: &str, output_directory: &Path) {
    let source = format!("{name}.hlsl");
    let output = output_directory.join(format!("{name}.dxbc"));
    let source_path = PathBuf::from(format!("{SHADER_DIRECTORY}/{source}"));
    let shared_path = PathBuf::from(format!("{SHADER_DIRECTORY}/ps_shared.hlsl"));
    if !crate::is_stale(&output, &[&source_path, &shared_path, compiler]) {
        return;
    }
    let result = Command::new("wine")
        .arg(compiler)
        .arg(&source)
        .arg(profile)
        .arg(&output)
        .current_dir(SHADER_DIRECTORY)
        .env("WINEDEBUG", "-all")
        .output()
        .expect("failed to run the shader compiler under wine");
    if !result.status.success() {
        panic!(
            "{source} failed to compile as {profile}:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let blob = std::fs::read(&output).expect("compiled blob readable");
    assert!(
        blob.starts_with(b"DXBC"),
        "{} did not produce a DXBC container",
        output.display()
    );
}
