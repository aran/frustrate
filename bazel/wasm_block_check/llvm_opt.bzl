"""`opt` from the Rust toolchain's own llvm-tools, for :callgraph_test.

rules_rust unpacks the llvm-tools archive of the pinned Rust release into
each toolchain's tools repo, so its `opt` is the LLVM rustc was built with,
but exports only `llvm-cov` and `llvm-profdata` from it. One repo per host
here links `opt` and the libLLVM it loads out of that host's tools repo,
in the same layout, so `opt`'s relative library path still finds it.
Nothing is downloaded twice. Dev-only: MODULE.bazel names the tools repos
in a dev `use_repo`.
"""

# Host -> (Rust triple, the host's tools repo for that triple). The repo
# names are the ones rules_rust's `rust` extension gives them.
_HOSTS = {
    "linux_aarch64": ("aarch64-unknown-linux-gnu", "@rust_linux_aarch64__aarch64-unknown-linux-gnu__stable_tools"),
    "linux_x86_64": ("x86_64-unknown-linux-gnu", "@rust_linux_x86_64__x86_64-unknown-linux-gnu__stable_tools"),
    "macos_aarch64": ("aarch64-apple-darwin", "@rust_macos_aarch64__aarch64-apple-darwin__stable_tools"),
}

_BUILD = """\
filegroup(
    name = "opt",
    srcs = ["{dir}/bin/opt"],
    data = glob(["{dir}/lib/libLLVM*"]),
    visibility = ["//visibility:public"],
)
"""

def _llvm_opt_repo_impl(rctx):
    triple = rctx.attr.triple
    tools = rctx.path(rctx.attr.tools).dirname
    rel = "lib/rustlib/%s" % triple
    opt = tools.get_child(rel).get_child("bin").get_child("opt")
    if not opt.exists:
        fail("%s has no %s/bin/opt: the toolchain was fetched without llvm-tools" % (tools, rel))
    rctx.symlink(opt, rel + "/bin/opt")
    for f in tools.get_child(rel).get_child("lib").readdir():
        if f.basename.startswith("libLLVM"):
            rctx.symlink(f, "%s/lib/%s" % (rel, f.basename))
    rctx.file("BUILD.bazel", _BUILD.format(dir = rel))

_llvm_opt_repo = repository_rule(
    implementation = _llvm_opt_repo_impl,
    attrs = {
        "tools": attr.label(mandatory = True),
        "triple": attr.string(mandatory = True),
    },
)

def _llvm_opt_impl(module_ctx):
    for host, (triple, tools) in _HOSTS.items():
        _llvm_opt_repo(
            name = "llvm_opt_" + host,
            tools = Label(tools + "//:BUILD.bazel"),
            triple = triple,
        )
    return module_ctx.extension_metadata(reproducible = True)

llvm_opt = module_extension(implementation = _llvm_opt_impl)
