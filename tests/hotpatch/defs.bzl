"""The hot patch test's fixture crates and the rules that build its data."""

load("@rules_rust//rust:defs.bzl", "rust_shared_library")
load("//bazel:defs.bzl", "frustrate_bridge", "frustrate_bridge_library")
load("//bazel:hot_patch.bzl", "HotPatchIrInfo", "hot_patch_ir_aspect", "hot_patch_transition", "rust_sources_aspect")

# One edition per package, with fixed target names.
# buildifier: disable=unnamed-macro
def hot_patch_fixture():
    """One edition of the fixture bridge crate, declared in its own package.

    Every edition compiles to the same crate name with the same flags, so
    symbol names match across them; only `src/api.rs` differs.
    """
    frustrate_bridge(
        name = "bridge",
        srcs = ["src/api.rs"],
        crate_name = "hotpatch_fixture",
        package_name = "hotpatch_fixture",
        module_paths = ["crate::api"],
        visibility = ["//tests/hotpatch:__pkg__"],
    )
    rust_shared_library(
        name = "fixture",
        srcs = [
            "src/api.rs",
            "src/lib.rs",
            ":bridge.rs",
        ],
        crate_name = "hotpatch_fixture",
        edition = "2021",
        proc_macro_deps = ["//macros:frustrate_macros"],
        visibility = ["//tests/hotpatch:__pkg__"],
        link_deps = ["//tests/hotpatch:native_dep"],
        deps = ["//runtime/rust:frustrate"],
    )

def hot_patch_bridge(name):
    """One edition of a bridge of the composed fixture (`composed/<name>*`).

    `hotpatch_<name>`, declared as `<name>` (and `<name>_lib`, the crate a
    `frustrate_shared_library` links), with its bindings as `bridge`.
    """
    frustrate_bridge(
        name = "bridge",
        srcs = ["src/api.rs"],
        crate_name = "hotpatch_" + name,
        package_name = "hotpatch_" + name,
        module_paths = ["crate::api"],
        visibility = ["//tests/hotpatch:__pkg__"],
    )
    frustrate_bridge_library(
        name = name,
        srcs = [
            "src/api.rs",
            "src/lib.rs",
            ":bridge.rs",
        ],
        crate_name = "hotpatch_" + name,
        edition = "2021",
        proc_macro_deps = ["//macros:frustrate_macros"],
        visibility = ["//tests/hotpatch:__pkg__"],
        deps = [
            "//runtime/rust:frustrate",
            "//tests/hotpatch/composed/shared",
        ],
    )

def _crate_dir(argv):
    # process_wrapper flags, `--`, rustc, then the crate root.
    root = argv[argv.index("--") + 2]
    return root.rsplit("/src/", 1)[0]

_LABEL_HASHED = ("--codegen=metadata=", "--codegen=extra-filename=")

def _edit_ir_impl(ctx):
    edit = ctx.attr.library[0][HotPatchIrInfo]
    ir = ctx.actions.declare_file(ctx.label.name + ".ll")

    # An rlib's symbols carry a hash rules_rust derives from its label, which
    # differs between the edit's package and the launch crate's. The launch
    # crate's, so the two name the same functions alike.
    launch = {}
    if ctx.attr.launch_crate:
        for arg in ctx.attr.launch_crate[0][HotPatchIrInfo].argv:
            for flag in _LABEL_HASHED:
                if arg.startswith(flag):
                    launch[flag] = arg
    args = []
    for arg in edit.argv[1:]:
        if arg.startswith("--emit="):
            arg = "--emit=llvm-ir=" + ir.path
        for flag in _LABEL_HASHED:
            if arg.startswith(flag):
                arg = launch.get(flag, arg)
        args.append(arg)
    args.append("-Ccodegen-units=1")

    # The edit lives in its own package; as far as source paths reach the IR
    # (panic locations, debug info) it is the launch crate edited in place. The
    # two differ by one path segment, so the launch crate's directory is this
    # one with the package swapped.
    edit_dir = _crate_dir(edit.argv)
    launch_dir = edit_dir.rsplit("/", 1)[0] + "/" + ctx.attr.launch_package
    args.append("--remap-path-prefix=%s=%s" % (edit_dir, launch_dir))
    ctx.actions.run(
        executable = edit.argv[0],
        arguments = args,
        inputs = edit.compile_inputs,
        outputs = [ir],
        env = edit.env,
        # The Rustc action's exec group, so the copied argv runs on the
        # platform its tools were built for.
        toolchain = str(Label("@rules_rust//rust:toolchain_type")),
        mnemonic = "FrustrateHotPatchEditIr",
    )
    return [DefaultInfo(files = depset([ir, ctx.file.contract]))]

hot_patch_edit = rule(
    implementation = _edit_ir_impl,
    doc = "The IR of an edited fixture, compiled as if it were the launch crate edited in place.",
    attrs = {
        # `library`, because frustrate_hot_patchable's transition reads the
        # crate it is patching from an attribute of that name.
        "library": attr.label(mandatory = True, cfg = hot_patch_transition, aspects = [rust_sources_aspect, hot_patch_ir_aspect]),
        # The crate this is an edit of: it names the configuration, so this
        # compilation lands in the one the launch library was built in.
        "patched_crate": attr.label(mandatory = True),
        # The launch library's bridge crates, when it is a frustrate_shared_library:
        # they name the configuration too.
        "crates": attr.label_list(),
        # The launch build of the crate this edits, when that is an rlib.
        "launch_crate": attr.label(cfg = hot_patch_transition, aspects = [rust_sources_aspect, hot_patch_ir_aspect]),
        "launch_package": attr.string(mandatory = True),
        "contract": attr.label(mandatory = True, allow_single_file = True),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
    # The Rust rules' own toolchains, so the copied argv runs on the platform
    # it was resolved for.
    toolchains = [
        str(Label("@rules_rust//rust:toolchain_type")),
        config_common.toolchain_type("@rules_cc//cc:toolchain_type", mandatory = False),
    ],
)

def _mode_transition_impl(_settings, attr):
    return {"//command_line_option:compilation_mode": attr.mode}

_mode_transition = transition(
    implementation = _mode_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:compilation_mode"],
)

def _in_mode_impl(ctx):
    files = depset(transitive = [s[DefaultInfo].files for s in ctx.attr.srcs])
    return [DefaultInfo(files = files, runfiles = ctx.runfiles(transitive_files = files))]

in_mode = rule(
    implementation = _in_mode_impl,
    doc = "Files built in one compilation mode, whatever the test itself is built with.",
    attrs = {
        "mode": attr.string(mandatory = True, values = ["dbg", "fastbuild", "opt"]),
        "srcs": attr.label_list(mandatory = True, cfg = _mode_transition),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
