//! Build-side HLSL compilation: clang-cl builds the helper, wine runs it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Pixel shaders, each compiled as the shared source followed by its body.
const PIXEL_SHADERS: [&str; 3] = ["copy", "ordered", "fruit"];
const VERTEX_SHADER: &str = "fullscreen_triangle";

pub fn compile_all(output_directory: &Path, xwin_root: &str) {
    let compiler = build_compiler(output_directory, xwin_root);
    compile(
        &compiler,
        &source_path(&format!("{VERTEX_SHADER}.hlsl")),
        "vs_5_0",
        &output_directory.join(format!("{VERTEX_SHADER}.dxbc")),
    );

    let shared =
        std::fs::read_to_string(source_path("ps_shared.hlsl")).expect("shared source readable");
    for name in PIXEL_SHADERS {
        let body =
            std::fs::read_to_string(source_path(&format!("{name}.hlsl"))).expect("body readable");
        let combined = output_directory.join(format!("{name}.hlsl"));
        std::fs::write(&combined, format!("{shared}{body}")).expect("combined source writable");
        compile(
            &compiler,
            &combined,
            "ps_5_0",
            &output_directory.join(format!("{name}.dxbc")),
        );
    }
}

fn source_path(name: &str) -> PathBuf {
    PathBuf::from("res/shaders").join(name)
}

/// Builds the helper against the xwin SDK; d3dcompiler_47 resolves under wine.
fn build_compiler(output_directory: &Path, xwin_root: &str) -> PathBuf {
    let executable = output_directory.join("shader_compiler.exe");
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
        .arg("res/shaders/compiler.c")
        .arg(format!("/Fo{}\\", output_directory.display()))
        .arg(format!("/Fe{}", executable.display()))
        .arg("/link")
        .args([
            format!("/libpath:{xwin_root}/crt/lib/x86_64"),
            format!("/libpath:{xwin_root}/sdk/lib/um/x86_64"),
            format!("/libpath:{xwin_root}/sdk/lib/ucrt/x86_64"),
        ])
        .arg("d3dcompiler.lib")
        .status()
        .expect("failed to run clang-cl for the shader compiler");
    assert!(
        status.success(),
        "shader compiler build failed with {status}"
    );
    executable
}

fn compile(compiler: &Path, source: &Path, profile: &str, output: &Path) {
    let output_result = Command::new("wine")
        .arg(compiler)
        .arg(source)
        .arg(profile)
        .arg(output)
        .env("WINEDEBUG", "-all")
        .output()
        .expect("failed to run the shader compiler under wine");
    if !output_result.status.success() {
        panic!(
            "{} failed to compile as {profile}:\n{}",
            source.display(),
            String::from_utf8_lossy(&output_result.stderr)
        );
    }
    let blob = std::fs::read(output).expect("compiled blob readable");
    assert!(
        blob.starts_with(b"DXBC"),
        "{} did not produce a DXBC container",
        output.display()
    );
}
