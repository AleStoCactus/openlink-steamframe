//! Compiles the video compute shaders to SPIR-V with `glslc` (from shaderc / the Vulkan SDK).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "linux" && os != "windows" {
        return;
    }
    println!("cargo:rerun-if-env-changed=VULKAN_SDK");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for name in ["rgba_to_nv12.comp"] {
        let src = format!("shaders/{name}");
        println!("cargo:rerun-if-changed={src}");
        let dst = out.join(format!("{name}.spv"));
        let status = Command::new(glslc())
            .args(["-O", "--target-env=vulkan1.3", &src, "-o"])
            .arg(&dst)
            .status()
            .unwrap_or_else(|e| panic!("cannot run glslc ({e}); install shaderc or the Vulkan SDK"));
        assert!(status.success(), "glslc failed on {src}");
    }
}

/// `glslc` from PATH, or from the Vulkan SDK (whose installer sets `VULKAN_SDK` but, on
/// Windows, does not always put `Bin` on PATH for already-open shells).
fn glslc() -> PathBuf {
    let exe = if cfg!(windows) { "glslc.exe" } else { "glslc" };
    if let Some(sdk) = std::env::var_os("VULKAN_SDK") {
        for dir in ["Bin", "bin"] {
            let p = PathBuf::from(&sdk).join(dir).join(exe);
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from(exe)
}
