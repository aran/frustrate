"""`rust_toolchain.linker` for the custom-std toolchains: rust-lld plus extra tools.

rules_rust builds a toolchain's sysroot from its attributes, and places the
`linker` file and its runfiles by their path within their repository. This
rule re-roots rust-lld (with its own runfiles) and each extra tool at
`lib/rustlib/<exec_triple>/bin/` of the repository it is declared in, which is
the directory rustc prepends to a linker's PATH. A bare `-Clinker=<tool>` then
finds the tool, and the tool finds `rust-lld`, on whichever host the action
runs, because the tools are built for that host.

Declare it in a repository's root package: a file's place in the sysroot is
its path below the repository root.
"""

def _sysroot_linker_impl(ctx):
    bin_dir = "lib/rustlib/%s/bin/" % ctx.attr.exec_triple

    def place(file, rel):
        out = ctx.actions.declare_file(rel)
        ctx.actions.symlink(output = out, target_file = file, is_executable = True)
        return out

    lld_root = ctx.attr.lld.label.workspace_root
    lld = None
    runfiles = []
    lld_files = ctx.attr.lld[DefaultInfo].files.to_list() + \
                ctx.attr.lld[DefaultInfo].default_runfiles.files.to_list()
    for f in lld_files:
        rel = f.path.split(lld_root + "/", 1)[-1] if lld_root else f.short_path
        if f == ctx.file.lld:
            if lld == None:
                lld = place(f, rel)
        else:
            runfiles.append(place(f, rel))
    for tool, name in ctx.attr.tools.items():
        runfiles.append(place(tool[DefaultInfo].files_to_run.executable, bin_dir + name))

    return [DefaultInfo(
        files = depset([lld]),
        runfiles = ctx.runfiles(files = runfiles),
    )]

sysroot_linker = rule(
    implementation = _sysroot_linker_impl,
    attrs = {
        "exec_triple": attr.string(mandatory = True),
        "lld": attr.label(mandatory = True, allow_single_file = True),
        "tools": attr.label_keyed_string_dict(
            doc = "Executables to place beside rust-lld, each under its name.",
        ),
    },
)
