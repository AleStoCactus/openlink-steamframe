//! Compiles the video compute shaders to SPIR-V with `glslc` (from shaderc / the Vulkan SDK).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for name in ["rgba_to_nv12.comp"] {
        let src = format!("shaders/{name}");
        println!("cargo:rerun-if-changed={src}");
        let dst = out.join(format!("{name}.spv"));
        let status = Command::new("glslc")
            .args(["-O", "--target-env=vulkan1.3", &src, "-o"])
            .arg(&dst)
            .status()
            .unwrap_or_else(|e| panic!("cannot run glslc ({e}); install shaderc or the Vulkan SDK"));
        assert!(status.success(), "glslc failed on {src}");
    }
}
