"""`frustrate_native_block_check`: the `no_block` claims, settled over native code.

Re-exported from //bazel:defs.bzl. The scanner's decision procedure is in
//bazel/wasm_block_check/ir.rs.
"""

load("@rules_cc//cc:find_cc_toolchain.bzl", "use_cc_toolchain")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("@rules_rust//rust/private:common.bzl", "rust_common")
load("@rules_rust//rust/private:rust.bzl", "RUSTC_ATTRS")
load(
    "@rules_rust//rust/private:rustc.bzl",
    "collect_deps",
    "collect_inputs",
    "construct_arguments",
)
load(
    "@rules_rust//rust/private:utils.bzl",
    "determine_output_hash",
    "find_cc_toolchain",
    "find_toolchain",
)

# rules_rust declares no LLVM IR output, so this re-runs rustc on the crate the
# way rules_rust's own `unpretty` aspect does, with one declared output: the
# whole program's IR. Fat LTO is what makes it whole: rustc merges every
# dependency's bitcode and std's embedded bitcode into the crate's module
# before writing it.
#
# The crate is emitted as a staticlib, the one crate type rustc runs fat LTO
# for without calling a linker. The IR is the same program either way; what
# the staticlib saves is the target's C library and linker, so this action
# needs only rustc and the target's std, from any host.
def _native_ir_aspect_impl(target, ctx):
    # A cdylib carries its CrateInfo inside TestCrateInfo.
    if rust_common.crate_info in target:
        crate_info = target[rust_common.crate_info]
    elif rust_common.test_crate_info in target:
        crate_info = target[rust_common.test_crate_info].crate
    else:
        return []

    crate_info = rust_common.create_crate_info(**(
        {k: getattr(crate_info, k) for k in dir(crate_info) if k not in ["to_json", "to_proto"]} |
        {"type": "staticlib"}
    ))
    toolchain = find_toolchain(ctx)
    cc_toolchain, feature_configuration = find_cc_toolchain(ctx)
    dep_info, build_info, _ = collect_deps(
        deps = crate_info.deps.to_list(),
        proc_macro_deps = crate_info.proc_macro_deps.to_list(),
        aliases = crate_info.aliases,
        extra_named_deps = crate_info.extra_named_deps,
    )
    compile_inputs, out_dir, build_env_files, build_flags_files, linkstamp_outs, ambiguous_libs = collect_inputs(
        ctx,
        ctx.rule.file,
        ctx.rule.files,
        depset([]),
        toolchain,
        cc_toolchain,
        feature_configuration,
        crate_info,
        dep_info,
        build_info,
        [],
    )
    ir = ctx.actions.declare_file(ctx.label.name + ".native_check.ll", sibling = crate_info.output)
    args, env = construct_arguments(
        ctx = ctx,
        attr = ctx.rule.attr,
        file = ctx.file,
        toolchain = toolchain,
        tool_path = toolchain.rustc.path,
        tool_file = toolchain.rustc,
        cc_toolchain = cc_toolchain,
        feature_configuration = feature_configuration,
        crate_info = crate_info,
        dep_info = dep_info,
        linkstamp_outs = linkstamp_outs,
        ambiguous_libs = ambiguous_libs,
        output_hash = determine_output_hash(crate_info.root, ctx.label),
        rust_flags = [],
        out_dir = out_dir,
        build_env_files = build_env_files,
        build_flags_files = build_flags_files,
        # `link` too: rustc performs LTO only when it writes a final crate
        # type, and without it the IR is the crate's own module. The archive
        # lands in the sandbox, undeclared, and is discarded.
        emit = ["link", ("llvm-ir", ir)],
        skip_expanding_rustc_env = True,
    )
    ctx.actions.run(
        executable = ctx.executable._process_wrapper,
        inputs = compile_inputs,
        outputs = [ir],
        env = env,
        arguments = args.all,
        mnemonic = "FrustrateNativeIr",
        progress_message = "emitting whole-program IR for %{label}",
        toolchain = "@rules_rust//rust:toolchain_type",
    )
    return [OutputGroupInfo(frustrate_native_ir = depset([ir]))]

_native_ir_aspect = aspect(
    implementation = _native_ir_aspect_impl,
    fragments = ["cpp"],
    attrs = RUSTC_ATTRS,
    toolchains = [str(Label("@rules_rust//rust:toolchain_type"))] + use_cc_toolchain(mandatory = False),
    required_providers = [[rust_common.crate_info], [rust_common.test_crate_info]],
)

_EXTRA_RUSTC_FLAGS = str(Label("@rules_rust//rust/settings:extra_rustc_flags"))
_LTO = str(Label("@rules_rust//rust/settings:lto"))
_EXTRA_RUSTC_FLAG = str(Label("@rules_rust//rust/settings:extra_rustc_flag"))
_ALLOCATOR_LIBRARY = str(Label("@rules_rust//rust/settings:default_allocator_library"))

def _native_check_transition_impl(settings, attr):
    return {platform.name: _native_check_config(settings, platform) for platform in attr.platforms}

def _native_check_config(settings, platform):
    return {
        # The platform the analysis proves the claims for. Only the crate
        # moves to it; the scanner stays on the host, so any platform's
        # analysis runs from any host.
        "//command_line_option:platforms": [str(platform)],
        # rules_rust's default is a C library every Rust toolchain depends
        # on, which needs a C toolchain for the target. Its symbols are for a
        # C link of Rust code, which this build never does, so an empty one
        # keeps the target's C toolchain out of the build. A dependency that
        # compiles C for the target still needs one registered.
        _ALLOCATOR_LIBRARY: str(Label("//bazel:no_block_check_allocator")),
        # Debug, as on wasm: an optimised build inlines across the boundaries
        # the witness paths name.
        "//command_line_option:compilation_mode": "fastbuild",
        # Fat LTO is what puts std and every dependency in the one module
        # the scanner reads; rules_rust builds the dependencies as bitcode
        # for it.
        _LTO: "fat",
        # The roots, and nothing else exported, exactly as on wasm.
        _EXTRA_RUSTC_FLAGS: [
            "--cfg=frustrate_block_check",
            "-Adead_code",
        ],
        # The repeatable flag reaches rustc after the list above, so a
        # project's own `-Copt-level` there would win over fastbuild's.
        # Appending here keeps the project's flags and makes ours last.
        _EXTRA_RUSTC_FLAG: settings[_EXTRA_RUSTC_FLAG] + ["-Copt-level=0"],
    }

_native_check_transition = transition(
    implementation = _native_check_transition_impl,
    inputs = [_EXTRA_RUSTC_FLAG],
    outputs = [
        "//command_line_option:platforms",
        _ALLOCATOR_LIBRARY,
        "//command_line_option:compilation_mode",
        _LTO,
        _EXTRA_RUSTC_FLAGS,
        _EXTRA_RUSTC_FLAG,
    ],
)

def _no_cc_allocator_impl(_ctx):
    return [CcInfo()]

# An allocator library with nothing in it, for the native analyses' builds.
no_cc_allocator = rule(implementation = _no_cc_allocator_impl)

def _frustrate_native_block_check_impl(ctx):
    census = ctx.attr.bridge[OutputGroupInfo].claims.to_list()
    if len(census) != 1:
        fail("frustrate_native_block_check: %s is not a frustrate_bridge target — " % ctx.attr.bridge.label +
             "it has no single `claims` output.")
    markers = []
    for platform, crate in sorted(ctx.split_attr.crate.items()):
        ir = crate[OutputGroupInfo].frustrate_native_ir.to_list()[0]
        marker = ctx.actions.declare_file("%s.%s.native_block_check" % (ctx.label.name, platform))
        ctx.actions.run(
            executable = ctx.executable._block_check,
            inputs = [ir, census[0]],
            outputs = [marker],
            arguments = ["--ir"] +
                        [a for p in ctx.attr.slow for a in ["--slow", p]] +
                        [a for p in ctx.attr.slow_externs for a in ["--slow-extern", p]] +
                        ["--claims", census[0].path, ir.path, "%s (%s)" % (ctx.label, platform), marker.path],
            mnemonic = "FrustrateNativeBlockCheck",
            progress_message = "checking no_block claims natively in %%{label} for %s" % platform,
        )
        markers.append(marker)
    return [
        DefaultInfo(files = depset([])),
        OutputGroupInfo(_validation = depset(markers)),
    ]

frustrate_native_block_check = rule(
    implementation = _frustrate_native_block_check_impl,
    doc = "Settle every `#[bridge(no_block)]` claim in a bridge crate over its " +
          "**native** build for each of `platforms`: fail if a claimed member " +
          "can reach a C library call that waits (`pthread_mutex_lock`, " +
          "`dispatch_semaphore_wait`, a futex wait, …), a function under a " +
          "`slow` path, or a C function in `slow_externs`.\n\n" +
          "The bridge crate is compiled once more for each platform under " +
          "`--cfg frustrate_block_check`, with fat LTO, and its whole-program " +
          "LLVM IR is scanned: std and every dependency included, every call " +
          "and every address-take attributed, so a wait reached through a " +
          "`dyn` call is attributed to the member that built the trait object. " +
          "Only the crate is built for the platform, and rustc writes the IR " +
          "without linking, so every platform's check runs from any host with " +
          "the platform's Rust std: the root module's `rust.toolchain` lists " +
          "its triple in `extra_target_triples`. Stock toolchain, so no " +
          "`manual` tag. Waits are known for Apple's libSystem, Android's " +
          "bionic and glibc; for another C library the scanner refuses rather " +
          "than guess.\n\n" +
          "Not followed: the panic handler, and the `std::sync` calls of " +
          "frustrate's own bounded runtime registries. " +
          "//bazel/wasm_block_check/ir.rs and libc.rs have the decision " +
          "procedure and the lists. A body split by `#[cfg]` is checked in " +
          "each platform's arm.",
    attrs = {
        "bridge": attr.label(
            mandatory = True,
            providers = [OutputGroupInfo],
            doc = "The frustrate_bridge target that generated this crate's " +
                  "glue, for its claim census.",
        ),
        "crate": attr.label(
            mandatory = True,
            cfg = _native_check_transition,
            aspects = [_native_ir_aspect],
            doc = "The bridge's native `rust_shared_library`.",
        ),
        "platforms": attr.label_list(
            mandatory = True,
            doc = "The platforms to prove the claims for: any of //bazel's " +
                  "`android_aarch64`, `android_x86_64`, `ios_aarch64`, " +
                  "`linux_aarch64`, `linux_x86_64`, `macos_aarch64`.",
        ),
        "slow": attr.string_list(
            doc = "Rust paths a claimed body may not reach, as for " +
                  "frustrate_block_check: a project's `#[inline(never)]` " +
                  "slow-work marker.",
        ),
        "slow_externs": attr.string_list(
            doc = "C functions a claimed body may not call, with an optional " +
                  "trailing `*`: `open`, `connect`, `getaddrinfo`. Native IO " +
                  "is a call to the C library, so this is how IO is checked " +
                  "here; the panic handler's stderr writes are not followed, " +
                  "so `write` is usable.",
        ),
        "_block_check": attr.label(
            default = Label("//bazel/wasm_block_check"),
            executable = True,
            cfg = "exec",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
