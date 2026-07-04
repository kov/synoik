//! Compile every GLSL source in `shaders/` to SPIR-V (`<name>.spv` in OUT_DIR) with
//! glslangValidator. The compiled modules are pulled in with `include_bytes!` at build time,
//! so a missing/broken shader fails the build rather than the run.

use std::path::Path;
use std::process::Command;

fn main() {
    let shader_dir = Path::new("shaders");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    println!("cargo:rerun-if-changed=shaders");

    for entry in std::fs::read_dir(shader_dir).expect("read shaders/") {
        let path = entry.expect("dir entry").path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "vert" | "frag" | "comp") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap();
        let out = Path::new(&out_dir).join(format!("{name}.spv"));
        println!("cargo:rerun-if-changed={}", path.display());

        let status = Command::new("glslangValidator")
            .args(["-V", "--target-env", "vulkan1.3"])
            .arg(&path)
            .arg("-o")
            .arg(&out)
            .status()
            .expect("run glslangValidator (install glslang / vulkan tools)");
        assert!(status.success(), "glslangValidator failed for {name}");
    }
}
