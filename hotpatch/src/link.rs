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
        let mut static_libs = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            let arg = &argv[i];
            let lib = if arg == "-l" {
                i += 1;
                argv.get(i).map(String::as_str)
            } else {
                arg.strip_prefix("-l")
            };
            if let Some((kind, name)) = lib.and_then(|l| l.split_once('=')) {
                if kind.split(':').next() == Some("static") {
                    static_libs.push(format!("-l{name}"));
                }
                i += 1;
                continue;
            }
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
        link_args.retain(|a| !a.contains("${") && !links_an_archive(a, &static_libs));
        let mut env: Vec<(String, String)> =
            env.into_iter().map(|(k, v)| (k, substitute(&v))).collect();
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

    pub fn link(
        &self,
        format: Format,
        arch: Arch,
        objects: &[&Path],
        output: &Path,
    ) -> Result<(), String> {
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

/// Whether a link argument puts a static archive into the link, or asks for
/// archives to be loaded whole (including `--undefined-glob`, which the `llvm`
/// toolchain adds). A patch links no archive: what it calls and does not carry
/// is bound to the running image's copy, and a second copy in the patch would
/// collide with that binding or run with its own state.
/// rules_rust names a static library both as rustc's `-lstatic=<name>` and as
/// a `-Clink-arg=-l<name>` (`static_libs` holds those `-l<name>` forms), and an
/// `alwayslink` one by its path. A `-Wl,` argument may carry several linker
/// arguments at once (`-Wl,--whole-archive,libx.a,--no-whole-archive`), so
/// each is judged.
fn links_an_archive(arg: &str, static_libs: &[String]) -> bool {
    if static_libs.iter().any(|l| l == arg) {
        return true;
    }
    let linker_args = match arg.strip_prefix("-Wl,") {
        Some(list) => list.split(',').collect(),
        None => vec![arg],
    };
    linker_args.iter().any(|a| {
        a.ends_with(".a")
            || a.ends_with(".lo")
            || *a == "-all_load"
            || *a == "-force_load"
            || *a == "--whole-archive"
            || *a == "--no-whole-archive"
            // Loads every archive member whatever references it, as
            // `--whole-archive` does, for the archives the link names.
            || a.starts_with("--undefined-glob=")
    })
}

/// `DEVELOPER_DIR` and `SDKROOT` for an Apple link, which Bazel adds to an
/// action when it runs it rather than recording them in the action's
/// environment: apple_support's `wrapped_clang` refuses to run without them.
/// Derived the way Bazel's local Xcode environment is, from the SDK platform
/// and version the action does record. A variable the patch builder's own
/// environment already sets wins.
fn apple_developer_env(env: &mut Vec<(String, String)>) {
    let get = |env: &Vec<(String, String)>, key: &str| {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };
    let (Some(platform), Some(version)) = (
        get(env, "APPLE_SDK_PLATFORM"),
        get(env, "APPLE_SDK_VERSION_OVERRIDE"),
    ) else {
        return;
    };
    let developer_dir = get(env, "DEVELOPER_DIR")
        .or_else(|| std::env::var("DEVELOPER_DIR").ok())
        .or_else(|| {
            let out = Command::new("/usr/bin/xcode-select")
                .arg("--print-path")
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        });
    let Some(developer_dir) = developer_dir else {
        return;
    };
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
                env.push((
                    "SDKROOT".into(),
                    String::from_utf8_lossy(&out.stdout).trim().to_string(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link_args(argv: &[&str]) -> Vec<String> {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        Toolchain::from_rustc_argv(PathBuf::new(), &argv, Vec::new()).link_args
    }

    #[test]
    fn a_patch_links_none_of_the_librarys_archives() {
        let args = link_args(&[
            "--codegen=link-arg=-shared",
            "-Lnative=bazel-out/bin/c",
            "--codegen=link-arg=-Wl,-all_load",
            "-lstatic=shim",
            "-Clink-arg=-lshim",
            "-l",
            "static:+whole-archive=api",
            "-Clink-arg=-lapi",
            "-Clink-arg=-Wl,bazel-out/bin/c/libnative.lo",
            "-Clink-arg=-Wl,-force_load,bazel-out/bin/c/libother.a",
            "-ldylib=z",
            "--codegen=link-arg=-lc++",
            "--codegen=link-arg=-lz",
        ]);
        assert_eq!(args, ["-shared", "-lc++", "-lz"]);
    }

    #[test]
    fn an_archive_inside_a_combined_linker_argument_is_left_out() {
        let args = link_args(&[
            "-Clink-arg=-Wl,--whole-archive,bazel-out/bin/c/libx.a,--no-whole-archive",
            "-Clink-arg=-Wl,--whole-archive,-lfoo,--no-whole-archive",
            "-Clink-arg=-Wl,-rpath,$ORIGIN",
            "-Clink-arg=-Wl,--undefined-glob=*",
        ]);
        assert_eq!(args, ["-Wl,-rpath,$ORIGIN"]);
    }
}
