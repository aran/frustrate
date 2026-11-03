//! Runs frustrate codegen at build time: the cargo-driven development loop.
//! (Bazel invokes the frustrate-codegen CLI as an action instead.)

use std::path::PathBuf;

fn main() {
    // Everything generated goes to OUT_DIR, never the source tree. The crate
    // includes the glue from there under this cfg (src/lib.rs); Bazel declares
    // it at src/frustrate_generated.rs instead and sets no cfg.
    println!("cargo:rustc-check-cfg=cfg(frustrate_cargo)");
    println!("cargo:rustc-cfg=frustrate_cargo");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=src/api.rs");
    println!("cargo:rerun-if-changed=src/cancel_api.rs");
    println!("cargo:rerun-if-changed=src/shapes.rs");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

    frustrate_codegen::generate(&frustrate_codegen::GenerateConfig {
        crate_name: "test_api".into(),
        dart_package: "frustrate_integration".into(),
        // Three sources, matching the Bazel target's `srcs`/`module_paths`. The
        // two flows must scan the same set or the generated fn ids diverge and
        // the schema hash with them.
        sources: vec![
            frustrate_codegen::SourceSpec {
                path: manifest.join("src/api.rs"),
                module_path: "crate::api".into(),
            },
            frustrate_codegen::SourceSpec {
                path: manifest.join("src/cancel_api.rs"),
                module_path: "crate::cancel_api".into(),
            },
            frustrate_codegen::SourceSpec {
                path: manifest.join("src/shapes.rs"),
                module_path: "crate::shapes".into(),
            },
        ],
        models: vec![],
        rust_out: out.join("frustrate_generated.rs"),
        dart_out: out.join("test_api.frustrate.dart"),
        // The interface the hostile-request driver reads (`src/hostile.rs`),
        // written by the same run that writes the glue so the two cannot
        // describe different interfaces. `include_str!`, not a file read:
        // the driver runs under Miri, whose isolation refuses `std::fs`, and
        // parsing 120KB of Rust with syn inside the interpreter would cost
        // more than every request it then builds.
        ir_out: Some(out.join("frustrate_ir.json")),
        claims_out: None,
    })
    .expect("frustrate codegen failed");
}
