//! Runs frustrate codegen at build time: the cargo-driven development loop.
//! (Bazel invokes the frustrate-codegen CLI as an action instead.)

use std::path::PathBuf;
// OUT_DIR is unused: the generated module must sit next to the sources so the
// same `mod frustrate_generated;` works under cargo and Bazel.

fn main() {
    println!("cargo:rerun-if-changed=src/api.rs");
    println!("cargo:rerun-if-changed=src/cancel_api.rs");
    println!("cargo:rerun-if-changed=src/shapes.rs");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

    frustrate_codegen::generate(&frustrate_codegen::GenerateConfig {
        crate_name: "test_api".into(),
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
        rust_out: manifest.join("src/frustrate_generated.rs"),
        dart_out: manifest.join("../dart_integration/lib/test_api.frustrate.dart"),
        // The interface the hostile-request driver reads (`src/hostile.rs`),
        // written by the same run that writes the glue so the two cannot
        // describe different interfaces. `include_str!`, not a file read:
        // the driver runs under Miri, whose isolation refuses `std::fs`, and
        // parsing 120KB of Rust with syn inside the interpreter would cost
        // more than every request it then builds.
        ir_out: Some(manifest.join("src/frustrate_ir.json")),
        // The claim census `tools/check_block.dart` reads: which `no_block`
        // claims want an artifact, and which are already settled by placement.
        // Beside the generated glue for the same reason it is — one directory
        // holds everything this crate's codegen produces.
        claims_out: Some(manifest.join("src/frustrate_claims.txt")),
    })
    .expect("frustrate codegen failed");
}
