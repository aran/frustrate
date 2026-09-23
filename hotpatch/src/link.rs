//! Compile a patch module and link it for the running image's target.
//!
//! The linker and its flags are the ones the library itself was linked with:
//! rules_rust hands rustc `-Clinker=` and one `-Clink-arg=` per toolchain flag,
//! and the patch builder is given that argv. What rustc adds on its own for a
//! dynamic library is added here per object format.

use crate::image::{Arch, Format};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Toolchain {
    pub llc: PathBuf,
    pub linker: String,
    pub link_args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Toolchain {
    /// Read the linker and its flags from a Rustc argv.
    pub fn from_rustc_argv(llc: PathBuf, argv: &[String], env: Vec<(String, String)>) -> Toolchain {
        let exec_root = std::env::current_dir().unwrap_or_default();
        let exec_root = exec_root.to_string_lossy().into_owned();
        let output_base = Path::new(&exec_root)
            .ancestors()
            .nth(2)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let substitute = |s: &str| {
            s.replace("${pwd}", &exec_root)
                .replace("${exec_root}", &exec_root)
                .replace("${output_base}", &output_base)
        };

        let mut linker = String::from("cc");
        let mut link_args = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            let arg = &argv[i];
            let codegen = if let Some(v) = arg.strip_prefix("--codegen=") {
                Some(v.to_string())
            } else if let Some(v) = arg.strip_prefix("-C") {
                if v.is_empty() {
                    i += 1;
                    argv.get(i).cloned()
                } else {
                    Some(v.to_string())
                }
            } else {
                None
            };
            if let Some(cg) = codegen {
                if let Some(l) = cg.strip_prefix("linker=") {
                    linker = substitute(l);
                } else if let Some(a) = cg.strip_prefix("link-arg=") {
                    link_args.push(substitute(a));
                } else if let Some(a) = cg.strip_prefix("link-args=") {
                    link_args.extend(a.split_whitespace().map(substitute));
                }
            }
            i += 1;
        }
        link_args.retain(|a| !a.contains("${"));
        let mut env: Vec<(String, String)> = env
            .into_iter()
            .map(|(k, v)| (k, substitute(&v)))
            .collect();
        apple_developer_env(&mut env);
        Toolchain {
            llc,
            linker,
            link_args,
            env,
        }
    }

    fn command(&self, program: &Path) -> Command {
        let mut c = Command::new(program);
        c.envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        c
    }

    pub fn compile(&self, module: &Path, object: &Path) -> Result<(), String> {
        let out = self
            .command(&self.llc)
            .args(["-O0", "-relocation-model=pic", "-filetype=obj", "-o"])
            .arg(object)
            .arg(module)
            .output()
            .map_err(|e| format!("could not run {}: {e}", self.llc.display()))?;
        if !out.status.success() {
            return Err(format!(
                "llc failed on {}:\n{}",
                module.display(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    }

    pub fn link(&self, format: Format, arch: Arch, objects: &[&Path], output: &Path) -> Result<(), String> {
        let file_name = output
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut c = self.command(Path::new(&self.linker));
        c.args(&self.link_args);
        match format {
            Format::MachO => {
                c.arg("-dynamiclib");
                c.args([
                    "-arch",
                    match arch {
                        Arch::Aarch64 => "arm64",
                        Arch::X86_64 => "x86_64",
                    },
                ]);
                c.arg(format!("-Wl,-install_name,@rpath/{file_name}"));
            }
            Format::Elf => {
                c.arg("-shared");
                c.arg(format!("-Wl,-soname,{file_name}"));
                // Bind the patch's references to its own definitions: a
                // same-named definition already in the global scope must not
                // capture them.
                c.arg("-Wl,-Bsymbolic");
            }
        }
        c.args(objects);
        c.arg("-o").arg(output);
        let out = c
            .output()
            .map_err(|e| format!("could not run linker {}: {e}", self.linker))?;
        if !out.status.success() {
            return Err(format!(
                "linking the patch failed:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    }
}

/// `DEVELOPER_DIR` and `SDKROOT` for an Apple link, which Bazel adds to an
/// action when it runs it rather than recording them in the action's
/// environment: apple_support's `wrapped_clang` refuses to run without them.
/// Derived the way Bazel's local Xcode environment is, from the SDK platform
/// and version the action does record. A variable the patch builder's own
/// environment already sets wins.
fn apple_developer_env(env: &mut Vec<(String, String)>) {
    let get = |env: &Vec<(String, String)>, key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let (Some(platform), Some(version)) = (get(env, "APPLE_SDK_PLATFORM"), get(env, "APPLE_SDK_VERSION_OVERRIDE")) else {
        return;
    };
    let developer_dir = get(env, "DEVELOPER_DIR").or_else(|| std::env::var("DEVELOPER_DIR").ok()).or_else(|| {
        let out = Command::new("/usr/bin/xcode-select").arg("--print-path").output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    let Some(developer_dir) = developer_dir else { return };
    if get(env, "DEVELOPER_DIR").is_none() {
        env.push(("DEVELOPER_DIR".into(), developer_dir.clone()));
    }
    if get(env, "SDKROOT").is_none() {
        let sdk = format!("{}{version}", platform.to_lowercase());
        if let Ok(out) = Command::new("/usr/bin/xcrun")
            .env("DEVELOPER_DIR", &developer_dir)
            .args(["--sdk", &sdk, "--show-sdk-path"])
            .output()
        {
            if out.status.success() {
                env.push(("SDKROOT".into(), String::from_utf8_lossy(&out.stdout).trim().to_string()));
            }
        }
    }
}
