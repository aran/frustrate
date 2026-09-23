//! Thin CLI over frustrate-codegen, for build-system invocation.
//!
//! Usage:
//!   frustrate-codegen \
//!     --crate-name my_bridge \
//!     --src api.rs:crate::api [--src more.rs:crate::more ...] \
//!     --rust-out frustrate_generated.rs \
//!     --dart-out bridge.frustrate.dart \
//!     [--ir-out interface.json] \
//!     [--claims-out claims.txt]
//!
//! There is no capability profile flag: every run checks the full surface
//! under native facts and the web subset under web facts, and emits both
//! Dart surfaces behind a conditional export.

use anyhow::{bail, Context, Result};
use frustrate_codegen::{GenerateConfig, SourceSpec};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut crate_name = None;
    let mut sources = vec![];
    let mut rust_out = None;
    let mut dart_out = None;
    let mut ir_out = None;
    let mut claims_out = None;

    while let Some(flag) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .with_context(|| format!("{name} requires a value"))
        };
        match flag.as_str() {
            "--crate-name" => crate_name = Some(value("--crate-name")?),
            "--src" => {
                let v = value("--src")?;
                let (path, module_path) = v
                    .split_once(':')
                    .context("--src expects <path>:<module path>, e.g. api.rs:crate::api")?;
                sources.push(SourceSpec {
                    path: path.into(),
                    module_path: module_path.to_string(),
                });
            }
            "--rust-out" => rust_out = Some(value("--rust-out")?.into()),
            "--dart-out" => dart_out = Some(value("--dart-out")?.into()),
            "--ir-out" => ir_out = Some(value("--ir-out")?.into()),
            "--claims-out" => claims_out = Some(value("--claims-out")?.into()),
            other => bail!("unknown flag `{other}`"),
        }
    }

    let config = GenerateConfig {
        crate_name: crate_name.context("--crate-name is required")?,
        sources,
        rust_out: rust_out.context("--rust-out is required")?,
        dart_out: dart_out.context("--dart-out is required")?,
        ir_out,
        claims_out,
    };
    if config.sources.is_empty() {
        bail!("at least one --src is required");
    }
    frustrate_codegen::generate(&config)?;
    Ok(())
}
