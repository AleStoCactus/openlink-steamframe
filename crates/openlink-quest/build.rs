fn main() {
    // khronos-egl's "static" feature expects libEGL to be linked by us (no pkg-config on Android).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=EGL");
    }
}
