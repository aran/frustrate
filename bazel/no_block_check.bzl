"""`frustrate_no_block_check`: every `#[bridge(no_block)]` claim, settled once per target.

Re-exported from //bazel:defs.bzl. The scanner every analysis here runs, and
its decision procedure, is //bazel/wasm_block_check (main.rs for wasm, ir.rs
for native IR, io.rs for the IO categories).
"""

load("@frustrate_custom_std_threads//:staged.bzl", _THREADS_STD_STAGED = "STAGED")
load("@rules_cc//cc:find_cc_toolchain.bzl", "use_cc_toolchain")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load("@rules_rust//rust:defs.bzl", "rust_common")
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

# Build-setting labels for the transitions below, canonicalized in frustrate's
# own repo mapping. A transition's `outputs` and its returned dict take plain
# strings, and these must be the same string in both places; the `Label()`
# round-trip makes them name these packages unambiguously, from any module.
_BLOCK_CHECK_FLAG = str(Label("//bazel:block_check"))
_EXTRA_RUSTC_FLAGS = str(Label("@rules_rust//rust/settings:extra_rustc_flags"))
_EXTRA_RUSTC_FLAG = str(Label("@rules_rust//rust/settings:extra_rustc_flag"))
_LTO = str(Label("@rules_rust//rust/settings:lto"))
_ALLOCATOR_LIBRARY = str(Label("@rules_rust//rust/settings:default_allocator_library"))

# The native OSes `targets` may name, and the CPUs each one's analyses build
# for: the ones a Flutter app ships on there that frustrate builds for (the
# triples MODULE.bazel lists in `supported_platform_triples`). One analysis per
# CPU, because std and the bridge's dependencies choose code by `target_arch`
# as well as by OS.
_NATIVE = {
    "android": ["aarch64", "x86_64"],
    "ios": ["aarch64"],
    "linux": ["x86_64", "aarch64"],
    "macos": ["aarch64"],
}

# What `targets` may name.
_TARGETS = sorted(_NATIVE) + ["web", "web_threads", "wasi"]

# ------------------------------------------------------------------ wasm --

def _wasm_check_transition_impl(settings, attr):
    return {
        # The platform is the analysis's: //bazel:wasm32_threads for `web_threads`,
        # whose +atomics build is the only one that holds wait instructions,
        # and //bazel:wasm32_wasi for `wasi`.
        "//command_line_option:platforms": [str(attr.platform)],
        # Debug, and this is a correctness requirement rather than a build-speed
        # preference. Measured on the threaded fixture: at release opt levels
        # rustc inlines the wait intrinsic into ~10 callers, leaving no single
        # named function for the scanner to name in its witness chain; at
        # opt-level 0 it stays one function. Pinned rather than inherited so a
        # `bazel build -c opt` of this target still answers the question.
        "//command_line_option:compilation_mode": "fastbuild",
        # The cfg that inverts the module's export surface: codegen emits one
        # `frustrate_check_block_*` root per claimed member under it and gates
        # every other `#[no_mangle]` — in the generated glue and in the runtime —
        # on `#[cfg(not(frustrate_block_check))]`. lld then GCs everything the
        # roots cannot reach, which is what makes a surviving wait mean
        # something.
        #
        # The *build setting*, not a `rustc_flags` attribute, because the gated
        # exports live in //runtime/rust as well as in the bridge crate, and a
        # per-target flag reaches neither deps nor the generated glue's crate
        # from outside it. This is the only knob that reaches every crate in the
        # target configuration.
        #
        # `-Adead_code`: with the exports gated out, whole regions of the
        # runtime and of the generated glue are legitimately unreachable in this
        # configuration only. It lands *after* any target's own `rustc_flags`
        # (rules_rust appends the extra flags), so it also relaxes a crate built
        # with `-Dwarnings` — deliberately, since a diagnostic artifact should
        # not be able to fail for a lint about code it exists to delete.
        #
        # The export filter, which cuts dependency rlibs' `#[wasm_bindgen]`
        # exports off the link line, is not here: frustrate's custom-std
        # toolchains, which carry it in their sysroot, add its `-Clinker=`
        # under //bazel:block_check themselves (custom_std.bzl). A stock
        # toolchain links with rust-lld, and the scanner refuses any export
        # that is not a check root.
        _EXTRA_RUSTC_FLAGS: [
            "--cfg=frustrate_block_check",
            "-Adead_code",
        ],
        # The repeatable flag reaches rustc after the list above, so a
        # project's `-Copt-level` there (a `.bazelrc` line, say) would beat
        # the debug build pinned above. Appended, so it stays last.
        _EXTRA_RUSTC_FLAG: settings[_EXTRA_RUSTC_FLAG] + ["-Copt-level=0"],
        # Turns //runtime/rust's `wasm-threads` feature off while the threaded
        # platform keeps the +atomics std on. //bazel:BUILD.bazel has the full
        # argument; the short version is that this is what saves the design a
        # second platform, a second toolchain and a second custom_std flavour.
        _BLOCK_CHECK_FLAG: True,
    }

_wasm_check_transition = transition(
    implementation = _wasm_check_transition_impl,
    inputs = [_EXTRA_RUSTC_FLAG],
    outputs = [
        "//command_line_option:platforms",
        "//command_line_option:compilation_mode",
        _EXTRA_RUSTC_FLAG,
        _EXTRA_RUSTC_FLAGS,
        _BLOCK_CHECK_FLAG,
    ],
)

# ---------------------------------------------------------------- native --

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

def _native_check_transition_impl(settings, attr):
    return {
        # The platform the analysis proves the claims for. Only the crate
        # moves to it; the test and the scanner stay on the host, so any
        # platform's analysis runs from any host.
        "//command_line_option:platforms": [str(attr.platform)],
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
        # As on wasm: appended, so a project's own `-Copt-level` cannot beat
        # fastbuild's.
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

# ----------------------------------------------------------------- tests --

def _census(ctx):
    census = ctx.attr.bridge[OutputGroupInfo].claims.to_list()
    if len(census) != 1:
        fail("frustrate_no_block_check: %s is not a frustrate_bridge target — it " % ctx.attr.bridge.label +
             "has no single `claims` output. Point `bridge` at the " +
             "frustrate_bridge that generated this crate's glue.")
    return census[0]

def _sink_args(ctx):
    return [a for p in ctx.attr.slow for a in ["--slow", p]] + \
           [a for c in ctx.attr.io for a in ["--io", c]]

def _verdict_test(ctx, inputs, arguments, mnemonic, extra_outputs = {}):
    """Run the scanner as a build action and make its verdict a test.

    A red verdict fails the action, so the test fails to build; a test that
    runs is green, and its log is what was checked. The test's executable is
    the scanner itself, told by the environment to print the verdict.
    """
    verdict = ctx.actions.declare_file(ctx.label.name + ".verdict")
    ctx.actions.run(
        executable = ctx.executable._block_check,
        inputs = inputs,
        outputs = [verdict],
        arguments = arguments + [str(ctx.label), verdict.path],
        mnemonic = mnemonic,
        progress_message = "checking no_block claims in %{label}",
    )
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = exe, target_file = ctx.executable._runner, is_executable = True)
    return [
        DefaultInfo(
            executable = exe,
            files = depset([verdict]),
            runfiles = ctx.runfiles(files = [verdict]),
        ),
        RunEnvironmentInfo(environment = {"FRUSTRATE_NO_BLOCK_VERDICT": verdict.short_path}),
        OutputGroupInfo(**extra_outputs),
    ]

_SCANNER_ATTRS = {
    "bridge": attr.label(mandatory = True, providers = [OutputGroupInfo]),
    "_block_check": attr.label(
        default = Label("//bazel/wasm_block_check"),
        executable = True,
        cfg = "exec",
    ),
    "_runner": attr.label(
        default = Label("//bazel/wasm_block_check"),
        executable = True,
        cfg = "target",
    ),
}

_SINK_ATTRS = {
    "io": attr.string_list(),
    "slow": attr.string_list(),
}

def _census_test_impl(ctx):
    census = _census(ctx)
    return _verdict_test(ctx, [census], ["--claims-only", census.path], "FrustrateNoBlockCensus")

_census_test = rule(
    implementation = _census_test_impl,
    test = True,
    attrs = _SCANNER_ATTRS,
)

def _wasm_test_impl(ctx):
    census = _census(ctx)
    files = ctx.attr.crate[0][DefaultInfo].files.to_list()
    wasm = [f for f in files if f.extension == "wasm"]
    if len(wasm) != 1:
        fail("frustrate_no_block_check: expected exactly one .wasm output from %s, got [%s]" % (
            ctx.attr.crate[0].label,
            ", ".join([f.basename for f in files]),
        ))

    # Run over rustc's own output: the scan reads mangled Rust symbols out of
    # the name section and whitelists the export set, so anything that
    # rewrote names or added an export would make the answer meaningless.
    return _verdict_test(
        ctx,
        [wasm[0], census],
        (["--wasi"] if ctx.attr.wasi else []) + _sink_args(ctx) +
        ["--claims", census.path, wasm[0].path],
        "FrustrateWasmBlockCheck",
        # The module, for `wasm-tools` or the scanner's `--report`.
        {"module": depset(wasm)},
    )

_wasm_test = rule(
    implementation = _wasm_test_impl,
    test = True,
    attrs = _SCANNER_ATTRS | _SINK_ATTRS | {
        "crate": attr.label(mandatory = True, cfg = _wasm_check_transition),
        "platform": attr.label(mandatory = True),
        # The scanner treats `poll_oneoff` as a wait.
        "wasi": attr.bool(),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def _native_test_impl(ctx):
    census = _census(ctx)
    ir = ctx.attr.crate[0][OutputGroupInfo].frustrate_native_ir.to_list()[0]
    return _verdict_test(
        ctx,
        [ir, census],
        ["--ir"] + _sink_args(ctx) + ["--claims", census.path, ir.path],
        "FrustrateNativeBlockCheck",
    )

_native_test = rule(
    implementation = _native_test_impl,
    test = True,
    attrs = _SCANNER_ATTRS | _SINK_ATTRS | {
        "crate": attr.label(
            mandatory = True,
            cfg = _native_check_transition,
            aspects = [_native_ir_aspect],
        ),
        "platform": attr.label(mandatory = True),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

# ----------------------------------------------------------------- macro --

def frustrate_no_block_check(
        name,
        bridge,
        crate = None,
        targets = None,
        slow = [],
        io = [],
        **kwargs):
    """Settle every `#[bridge(no_block)]` claim in a bridge, for the targets it ships on.

    `#[bridge(no_block)]` is the author's claim that main-thread Dart calling
    the member cannot be stalled: it reaches no wait, no function named in
    `slow`, and no IO in an `io` class. Codegen settles what placement
    settles (an actor member's body, a dispatched member's body) and writes a
    claim census; the rest is settled here, over a throwaway build of `crate`
    whose only exports are one root per claim
    (docs/design/blocking.md). Nothing this builds ships.

    `targets` says where the bridge ships, and each analysis runs only where
    it proves something there. They are tests under `name`, a test_suite:

    - `android`, `ios`, `linux`, `macos` → `<name>.<os>_<cpu>`, one per CPU
      frustrate builds that OS for: `android_aarch64` and `android_x86_64`;
      `ios_aarch64` (a device); `linux_x86_64` and `linux_aarch64` (glibc);
      `macos_aarch64`. Each builds the crate for its platform with fat LTO
      and scans the whole-program LLVM IR for its C library's waits (Apple's
      libSystem, Android's bionic, glibc), `slow`, and `io`. Proves the
      claims for that platform, and for every target's portable code,
      single-threaded web's included. The test and the scanner run on the
      host; only the crate is built for the platform, and rustc writes the
      IR without linking, so every analysis runs from any host with no SDK,
      NDK or C toolchain for the platform. What each needs is the platform's
      Rust std: the root module's `rust.toolchain` lists the triple in
      `extra_target_triples` (`aarch64-linux-android`, `x86_64-linux-android`,
      `aarch64-apple-ios`, and the Linux and macOS triples rules_rust lists
      by default), and the crate universe the bridge's dependencies come
      from supports it. A dependency that compiles C for the platform also
      needs the platform's C toolchain registered.
    - `web` → nothing of its own. Single-threaded wasm has no wait
      instruction (std panics where it would wait), its std IO is stubs and
      JavaScript IO is asynchronous; its portable code is the native
      analyses'.
    - `web_threads` → `<name>.web_threads`: the crate built for
      //bazel:wasm32_threads, scanned for `memory.atomic.wait` and `slow`.
      Proves the claims where a wait on the browser's main thread traps, over
      the code only a wasm build compiles. It needs the locally built
      +atomics std (`bazel run //toolchain/custom_std:build`); until that is
      staged `bazel test` reports it SKIPPED, and a stale one fails the build
      with the builder's command.
    - `wasi` → `<name>.wasi`: the crate built for //bazel:wasm32_wasi with
      the stock std, scanned for `poll_oneoff` (wasip1's only wait), `slow`,
      and `io`. Hermetic.

    SKIPPED is Bazel's report that an analysis cannot run here, not a pass:
    it is in every `bazel test` summary, and naming the skipped test directly
    is an error that says why.

    Without `crate`, the target declares that every claim is settled by
    placement: it builds nothing, and is one test, `name`, that reads the
    census and fails naming any claim that wants an artifact. A crate whose
    claims are all placed is refused with `crate`, for the same reason.

    The analyses pin opt-level 0, because an optimized build inlines the
    boundaries the scan names; they append `-Copt-level=0` to
    `--@rules_rust//rust/settings:extra_rustc_flag`. A crate's own
    `rustc_flags` and `per_crate_rustc_flag` reach rustc after it, so an
    opt-level there still wins: the scanner refuses a build whose roots no
    longer reach their member's glue, which catches the optimizer deleting
    a body, not every inlining. Keep opt-level out of a bridge crate's own
    flags. The configurations also replace `extra_rustc_flags` (a list, so a
    transition can only replace it) with `--cfg=frustrate_block_check
    -Adead_code`.

    For `web_threads`, the whole graph must build `+atomics`, and
    crate_universe resolves deps per triple, before RUSTFLAGS exist, so it
    drops any dependency declared under `cfg(target_feature = ...)`.
    getrandom 0.4.3 loses `js-sys` that way, with an `E0433` inside
    getrandom. The fix is one annotation, scoped so it reaches nothing else:

        crate.annotation_select(
            crate = "getrandom",
            version = "0.4.3",
            deps = ["@your_crates//:js-sys"],
            repositories = ["your_crates"],
            triples = ["wasm32-unknown-unknown"],
        )

    The `web_threads` module links through `frustrate-link-filter`, which
    the custom-std toolchains carry, so a graph containing wasm-bindgen still
    has only the roots as exports; //bazel/wasm_block_check/link_filter.rs
    says what it can and cannot break.

    Args:
      name: The test_suite; the analyses are `<name>.<target>`.
      bridge: The frustrate_bridge that generated the crate's glue, for its
        claim census.
      crate: The bridge's `rust_shared_library`. Omit it when every claim is
        settled by placement.
      targets: Where the bridge ships: any of `android`, `ios`, `linux`,
        `macos`, `web`, `web_threads`, `wasi`. Required with `crate`.
      slow: Rust paths a claimed body may not reach, checked by every
        analysis: a slow-work marker (`my_crate::main_thread::slow`), or a
        third-party function. Each matches every function whose demangled
        path starts with it, whole segments only; start with the crate's
        name, never `crate::`. A marker must be `#[inline(never)]`: even
        `#[inline]` vanishes at opt-level 0. A misspelt path matches nothing
        and stays green, so try a new one once by calling it from a claimed
        member.
      io: IO classes a claimed body may not do: `files` (opening, naming,
        reading or syncing a file), `network` (sockets and name lookup),
        `process` (starting a program). frustrate maps each to the target's
        calls: C library functions natively, `wasi_snapshot_preview1`
        imports on wasi. Sleeping needs no class: it is a wait everywhere.
        Writes to an open descriptor are in no class, because stdout, stderr
        and the panic report are such writes. //bazel/wasm_block_check/io.rs
        has the maps and what each covers.
      **kwargs: Common attributes (`tags`, `visibility`, `size`, ...) for
        every test and the suite.
    """
    if not crate:
        if slow or io or targets != None:
            fail("frustrate_no_block_check: %s has no `crate`, so it builds " % name +
                 "nothing and `slow`, `io` and `targets` would check nothing. " +
                 "Name the crate, or drop them.")
        _census_test(name = name, bridge = bridge, **kwargs)
        return

    if not targets:
        fail("frustrate_no_block_check: %s names a `crate` but no `targets`. " % name +
             "Say where the bridge ships (%s): each runs the analysis " % ", ".join(_TARGETS) +
             "that proves its claims there.")
    for t in targets:
        if t not in _TARGETS:
            fail("frustrate_no_block_check: `targets` takes %s, not %r." % (", ".join(_TARGETS), t))
    analyses = [t for t in targets if t != "web"]
    if not analyses:
        fail("frustrate_no_block_check: %s ships only on single-threaded web, " % name +
             "which has no wait instruction, so no analysis proves anything " +
             "there. Add a native target to check `slow` over its portable " +
             "code, or drop `crate` if every claim is placed.")
    if io and not [t for t in analyses if t in _NATIVE or t == "wasi"]:
        fail("frustrate_no_block_check: %s names `io`, but only the native " % name +
             "targets and `wasi` check IO: a web build has none that blocks.")

    tags = kwargs.pop("tags", [])
    compatible = kwargs.pop("target_compatible_with", [])
    suite_kwargs = {k: v for k, v in kwargs.items() if k in ["visibility", "testonly"]}
    tests = []
    for os in sorted(_NATIVE):
        if os not in analyses:
            continue
        for cpu in _NATIVE[os]:
            test = "%s.%s_%s" % (name, os, cpu)
            _native_test(
                name = test,
                bridge = bridge,
                crate = crate,
                platform = Label("//bazel:%s_%s" % (os, cpu)),
                slow = slow,
                io = io,
                tags = tags,
                target_compatible_with = compatible,
                **kwargs
            )
            tests.append(test)
    if "web_threads" in analyses:
        _wasm_test(
            name = name + ".web_threads",
            bridge = bridge,
            crate = crate,
            slow = slow,
            platform = Label("//bazel:wasm32_threads"),
            tags = tags,
            target_compatible_with = compatible + (
                [] if _THREADS_STD_STAGED else [Label("//bazel:wasm_threads_std_staged")]
            ),
            **kwargs
        )
        tests.append(name + ".web_threads")
    if "wasi" in analyses:
        _wasm_test(
            name = name + ".wasi",
            bridge = bridge,
            crate = crate,
            slow = slow,
            io = io,
            platform = Label("//bazel:wasm32_wasi"),
            wasi = True,
            tags = tags,
            target_compatible_with = compatible,
            **kwargs
        )
        tests.append(name + ".wasi")
    native.test_suite(
        name = name,
        tests = [":" + t for t in tests],
        tags = tags,
        **suite_kwargs
    )
