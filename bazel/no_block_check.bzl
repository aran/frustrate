"""`frustrate_no_block_check`: every `#[bridge(no_block)]` claim, settled once per target.

Re-exported from //bazel:defs.bzl. The scanner every analysis here runs, and
its decision procedure, is //bazel/no_block_check (main.rs for wasm, ir.rs
for native IR, io.rs for the IO categories).
"""

load("@frustrate_custom_std_threads//:staged.bzl", _THREADS_STD_STAGED = "STAGED")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")

# Build-setting labels for the transitions below, canonicalized in frustrate's
# own repo mapping. A transition's `outputs` and its returned dict take plain
# strings, and these must be the same string in both places; the `Label()`
# round-trip makes them name these packages unambiguously, from any module.
_NO_BLOCK_CHECK_FLAG = str(Label("//bazel:no_block_check"))
_EXTRA_RUSTC_FLAGS = str(Label("@rules_rust//rust/settings:extra_rustc_flags"))
_EXTRA_RUSTC_FLAG = str(Label("@rules_rust//rust/settings:extra_rustc_flag"))
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
        # and //bazel:wasm32_wasip1 for `wasi`.
        "//command_line_option:platforms": [str(attr.platform)],
        # Debug, and this is a correctness requirement rather than a build-speed
        # preference. At release opt levels rustc inlines the wait intrinsic
        # into its callers, leaving no single named function for the scanner to
        # name in its witness chain; at opt-level 0 it stays one function.
        # Pinned rather than inherited so a
        # `bazel build -c opt` of this target still answers the question.
        "//command_line_option:compilation_mode": "fastbuild",
        # The cfg that inverts the module's export surface: codegen emits one
        # `frustrate_check_block_*` root per claimed member under it and gates
        # every other `#[no_mangle]` — in the generated glue and in the runtime —
        # on `#[cfg(not(frustrate_no_block_check))]`. lld then GCs everything the
        # roots cannot reach, which is what makes a surviving wait mean
        # something.
        #
        # The *build setting*, not a `rustc_flags` attribute, because the gated
        # exports live in //runtime/rust as well as in the bridge crate, and a
        # per-target flag reaches neither deps nor the generated glue's crate
        # from outside it. This is the only knob that reaches every crate in the
        # target configuration.
        #
        # The repeatable `extra_rustc_flag`, not the list `extra_rustc_flags`:
        # the platform named above applies its own `flags` over what this
        # transition returns, and a platform that carries a rustc flag of its
        # own (`//ext/getrandom:wasm32_threads` sets `extra_rustc_flags`)
        # replaces that list wholesale, dropping the cfg below. The platform
        # leaves this setting alone, so the check keeps the flags the platform
        # exists to carry and adds its own to them. Appended, so it lands after
        # the platform's flags and the project's own, and stays last.
        #
        # `-Adead_code`: with the exports gated out, whole regions of the
        # runtime and of the generated glue are legitimately unreachable in this
        # configuration only. It lands *after* any target's own `rustc_flags`
        # (rules_rust appends the extra flags), so it also relaxes a crate built
        # with `-Dwarnings` — deliberately, since a diagnostic artifact should
        # not be able to fail for a lint about code it exists to delete.
        #
        # `-Copt-level=0` is last, so a project's `-Copt-level` (a `.bazelrc`
        # line, say) cannot beat the debug build pinned above.
        #
        # The export filter, which cuts dependency rlibs' `#[wasm_bindgen]`
        # exports off the link line, is not here: frustrate's custom-std
        # toolchains, which carry it in their sysroot, add its `-Clinker=`
        # under //bazel:no_block_check themselves (custom_std.bzl). A stock
        # toolchain links with rust-lld, and the scanner refuses any export
        # that is not a check root.
        _EXTRA_RUSTC_FLAG: settings[_EXTRA_RUSTC_FLAG] + [
            "--cfg=frustrate_no_block_check",
            "-Adead_code",
            "-Copt-level=0",
        ],
        # Turns //runtime/rust's `wasm-threads` feature off while the threaded
        # platform keeps the +atomics std on. //bazel:BUILD.bazel has the full
        # argument; the short version is that this is what saves the design a
        # second platform, a second toolchain and a second custom_std flavour.
        _NO_BLOCK_CHECK_FLAG: True,
    }

_wasm_check_transition = transition(
    implementation = _wasm_check_transition_impl,
    inputs = [_EXTRA_RUSTC_FLAG],
    outputs = [
        "//command_line_option:platforms",
        "//command_line_option:compilation_mode",
        _EXTRA_RUSTC_FLAG,
        _NO_BLOCK_CHECK_FLAG,
    ],
)

# ---------------------------------------------------------------- native --

_CrateIrInfo = provider(
    doc = "The LLVM IR the native analysis reads for a crate and the crates it depends on.",
    fields = {
        "own": "File or None: this crate's module.",
        "modules": "depset[File]: the modules of this crate and of every Rust crate beneath it.",
        "summaries": "depset[File]: each of those modules' Rust symbols, for std's part.",
        "std": "File or None: std's part of the program, for a crate rustc links.",
        "panic": "str or None: the crate's panic strategy, when its flags set one.",
    },
)

# The values rustc reads as a codegen switch turned on.
_ON = ["", "y", "yes", "on", "true"]

def _codegen_value(argv, key):
    """The last value `argv` gives codegen option `key`, or None."""
    value = None
    for i, a in enumerate(argv):
        for prefix in ("-C" + key + "=", "--codegen=" + key + "="):
            if a.startswith(prefix):
                value = a[len(prefix):]
        if a in ("-C" + key, "--codegen=" + key):
            value = ""
        if a in ("-C", "--codegen") and i + 1 < len(argv):
            nxt = argv[i + 1]
            if nxt.startswith(key + "="):
                value = nxt[len(key) + 1:]
            elif nxt == key:
                value = ""
    return value

def _crate_type(argv):
    for i, a in enumerate(argv):
        if a.startswith("--crate-type="):
            return a[len("--crate-type="):]
        if a == "--crate-type" and i + 1 < len(argv):
            return argv[i + 1]
    return None

def _switch(on):
    return "on" if on else "off"

def _ir_flags(argv):
    """What the module's compilation adds to the crate's own.

    Opt-level 0, so no function is inlined into another and every name the
    findings and silences use is a function of its own; the crate's own
    debug assertions and overflow checks, which rustc otherwise ties to the
    opt-level, so the code read is the code the build runs. One unit, so one
    file. No debug info, which the scan never reads. No LTO: the program is
    linked from the modules (modules.rs). The cfg adds the check roots beside
    the crate's exports; it changes no item another crate can see, so a crate
    compiled with it uses its dependencies' metadata compiled without it.
    """
    opt = _codegen_value(argv, "opt-level") or "0"
    asserts = _codegen_value(argv, "debug-assertions")
    asserts = (asserts in _ON) if asserts != None else (opt == "0")
    overflow = _codegen_value(argv, "overflow-checks")
    overflow = (overflow in _ON) if overflow != None else asserts
    return [
        "-Cdebug-assertions=" + _switch(asserts),
        "-Coverflow-checks=" + _switch(overflow),
        "-Copt-level=0",
        "-Ccodegen-units=1",
        "-Cdebuginfo=0",
        "-Clto=off",
        "--cfg=frustrate_no_block_check",
        "--cap-lints=allow",
    ]

def _std_closure(ctx, summaries, panic):
    """std's part of the program: rustc's fat LTO of the crate that names it."""
    name = ctx.label.name + ".no_block"
    listing = ctx.actions.declare_file(name + ".modules")
    args = ctx.actions.args()
    args.set_param_file_format("multiline")
    args.add_all(summaries)
    ctx.actions.write(listing, args)
    keep = ctx.actions.declare_file(name + ".std.rs")
    ctx.actions.run(
        executable = ctx.executable._no_block_check,
        inputs = depset([listing], transitive = [summaries]),
        outputs = [keep],
        arguments = ["--std-keep", keep.path, "--modules", listing.path],
        mnemonic = "FrustrateNoBlockStdKeep",
        progress_message = "naming std's part of %{label}",
    )
    toolchain = ctx.toolchains[str(Label("@rules_rust//rust:toolchain_type"))]
    ir = ctx.actions.declare_file(name + ".std.module.ll")
    archive = ctx.actions.declare_file(name + ".std.a")
    ctx.actions.run(
        executable = toolchain.rustc,
        inputs = depset([keep], transitive = [toolchain.all_files]),
        outputs = [ir, archive],
        arguments = [
            keep.path,
            "--crate-name=frustrate_no_block_std",
            "--crate-type=staticlib",
            "--edition=2024",
            "--target=" + toolchain.target_flag_value,
            "--sysroot=" + toolchain.sysroot,
            "-Copt-level=0",
            "-Ccodegen-units=1",
            "-Cdebuginfo=0",
            "-Clto=fat",
            "--cap-lints=allow",
            # `link` too: rustc runs LTO only when it writes a crate type that
            # links.
            "--emit=link=%s,llvm-ir=%s" % (archive.path, ir.path),
        ] + (["-Cpanic=" + panic] if panic else []),
        mnemonic = "FrustrateNoBlockStd",
        progress_message = "emitting std's part of %{label}",
        toolchain = str(Label("@rules_rust//rust:toolchain_type")),
    )
    return ir

def _crate_ir_aspect_impl(target, ctx):
    # A build script runs on the execution platform; nothing it compiles is
    # in the program.
    if "build_script" in ctx.rule.kind:
        return [_CrateIrInfo(own = None, modules = depset(), summaries = depset(), std = None, panic = None)]
    infos = [d[_CrateIrInfo] for d in getattr(ctx.rule.attr, "deps", []) if _CrateIrInfo in d]
    deps = [i.modules for i in infos]
    dep_summaries = [i.summaries for i in infos]
    rustc = [a for a in target.actions if a.mnemonic == "Rustc"]
    if not rustc:
        return [_CrateIrInfo(own = None, modules = depset(transitive = deps), summaries = depset(transitive = dep_summaries), std = None, panic = None)]
    if len(rustc) != 1:
        fail("%s: expected exactly one Rustc action, found %d" % (target.label, len(rustc)))
    action = rustc[0]
    argv = action.argv

    # The crate's own Rustc command, as the build ran it, with only the emit
    # changed and the flags above added: its inputs, the dependencies'
    # metadata among them, are the build's own.
    if len([a for a in argv[1:] if a.startswith("--emit=")]) != 1:
        # Not a compilation rules_rust's rules make (its own bootstrap tools'
        # are one). Nothing of it is read, and a program that calls into it
        # is refused for the functions it lacks.
        return [_CrateIrInfo(own = None, modules = depset(transitive = deps), summaries = depset(transitive = dep_summaries), std = None, panic = None)]
    ir = ctx.actions.declare_file(ctx.label.name + ".no_block.module.ll")
    args = ["--emit=llvm-ir=" + ir.path if a.startswith("--emit=") else a for a in argv[1:]]
    ctx.actions.run(
        executable = argv[0],
        arguments = args + _ir_flags(argv),
        inputs = action.inputs,
        outputs = [ir],
        env = action.env,
        # The argv names tools by their paths in the exec configuration the
        # Rustc action resolved, so this runs where that action would.
        toolchain = str(Label("@rules_rust//rust:toolchain_type")),
        mnemonic = "FrustrateNoBlockIr",
        progress_message = "emitting no_block IR for %{label}",
    )
    summary = ctx.actions.declare_file(ctx.label.name + ".no_block.symbols")
    ctx.actions.run(
        executable = ctx.executable._no_block_check,
        inputs = [ir],
        outputs = [summary],
        arguments = ["--summary", summary.path, ir.path],
        mnemonic = "FrustrateNoBlockSymbols",
        progress_message = "listing the symbols of %{label}",
    )
    modules = depset([ir], transitive = deps)
    summaries = depset([summary], transitive = dep_summaries)
    panic = _codegen_value(argv, "panic")
    std = None
    if _crate_type(argv) in ("cdylib", "staticlib", "bin"):
        std = _std_closure(ctx, summaries, panic)
    return [_CrateIrInfo(own = ir, modules = modules, summaries = summaries, std = std, panic = panic)]

_crate_ir_aspect = aspect(
    implementation = _crate_ir_aspect_impl,
    attr_aspects = ["deps"],
    attrs = {
        "_no_block_check": attr.label(
            default = Label("//bazel/no_block_check"),
            executable = True,
            cfg = "exec",
        ),
    },
    toolchains = [
        str(Label("@rules_rust//rust:toolchain_type")),
        config_common.toolchain_type("@rules_cc//cc:toolchain_type", mandatory = False),
    ],
)

def _native_check_transition_impl(_settings, attr):
    return {
        # The platform the analysis proves the claims for, for a build that
        # does not already target it. Only the crate moves to it; the test
        # and the scanner stay on the host, so any platform's analysis runs
        # from any host. Everything else is the build's own.
        "//command_line_option:platforms": [str(attr.platform)],
        # rules_rust's default is a C library every Rust toolchain depends
        # on, which needs a C toolchain for the target. Its symbols are for a
        # C link of Rust code, which the analysis never does, so an empty one
        # keeps the target's C toolchain out of the build. A dependency that
        # compiles C for the target still needs one registered.
        _ALLOCATOR_LIBRARY: str(Label("//bazel:no_block_check_allocator")),
    }

_native_check_transition = transition(
    implementation = _native_check_transition_impl,
    inputs = [],
    outputs = [
        "//command_line_option:platforms",
        _ALLOCATOR_LIBRARY,
    ],
)

def _no_cc_allocator_impl(_ctx):
    return [CcInfo()]

# An allocator library with nothing in it, for the native analyses' builds.
no_cc_allocator = rule(implementation = _no_cc_allocator_impl)

def _program(library):
    """The library's modules in reading order: its own first, std's last."""
    info = library[_CrateIrInfo]
    if not info.std:
        fail("frustrate_no_block_check: %s is not a crate rustc links (a " % library.label +
             "`rust_shared_library`, say), so it is not a whole program.")
    rest = [m for m in info.modules.to_list() if m != info.own] + [info.std]
    return info.own, rest

# ----------------------------------------------------------------- tests --

def _census_of(bridge):
    census = bridge[OutputGroupInfo].claims.to_list()
    if len(census) != 1:
        fail("frustrate_no_block_check: %s is not a frustrate_bridge target — it " % bridge.label +
             "has no single `claims` output. Point `bridge` at the " +
             "frustrate_bridge that generated this crate's glue.")
    return census[0]

def _censuses(ctx):
    """The claim census of every bridge the crate links."""
    return [_census_of(b) for b in ctx.attr.bridges]

def _claims_args(flag, censuses):
    """`flag` with the first census, and each further bridge's beside it."""
    return [flag, censuses[0].path] + [a for c in censuses[1:] for a in ["--also-claims", c.path]]

def _sink_args(ctx):
    return [a for p in ctx.attr.slow for a in ["--slow", p]] + \
           [a for c in ctx.attr.io for a in ["--io", c]]

def _allow_args(ctx):
    return [a for s in ctx.attr.allow for a in ["--allow", s]]

def _verdict_test(ctx, inputs, arguments, mnemonic, extra_outputs = {}, applied = False):
    """Run the scanner as a build action and make its verdict a test.

    A red verdict fails the action, so the test fails to build; a test that
    runs is green, and its log is what was checked. The test's executable is
    the scanner itself, told by the environment to print the verdict.

    With `applied`, the action also writes the silences it applied, as the
    `silences` output group, for the declaration's `.silences` test.
    """
    verdict = ctx.actions.declare_file(ctx.label.name + ".verdict")
    outputs = [verdict]
    if applied:
        applied_file = ctx.actions.declare_file(ctx.label.name + ".applied")
        outputs.append(applied_file)
        arguments = ["--applied", applied_file.path] + arguments
        extra_outputs = extra_outputs | {"silences": depset([applied_file])}
    ctx.actions.run(
        executable = ctx.executable._no_block_check,
        inputs = inputs,
        outputs = outputs,
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
    # The frustrate_bridge of every bridge the crate links: one, or each of a
    # `frustrate_shared_library`'s.
    "bridges": attr.label_list(mandatory = True, allow_empty = False, providers = [OutputGroupInfo]),
    "_no_block_check": attr.label(
        default = Label("//bazel/no_block_check"),
        executable = True,
        cfg = "exec",
    ),
    "_runner": attr.label(
        default = Label("//bazel/no_block_check"),
        executable = True,
        cfg = "target",
    ),
}

_SINK_ATTRS = {
    "allow": attr.string_list(),
    "io": attr.string_list(),
    "slow": attr.string_list(),
}

def _census_test_impl(ctx):
    censuses = _censuses(ctx)
    return _verdict_test(ctx, censuses, _claims_args("--claims-only", censuses), "FrustrateNoBlockCensus")

_census_test = rule(
    implementation = _census_test_impl,
    test = True,
    attrs = _SCANNER_ATTRS,
)

def _wasm_test_impl(ctx):
    censuses = _censuses(ctx)
    files = ctx.attr.library[0][DefaultInfo].files.to_list()
    wasm = [f for f in files if f.extension == "wasm"]
    if len(wasm) != 1:
        fail("frustrate_no_block_check: expected exactly one .wasm output from %s, got [%s]" % (
            ctx.attr.library[0].label,
            ", ".join([f.basename for f in files]),
        ))

    # Run over rustc's own output: the scan reads mangled Rust symbols out of
    # the name section and whitelists the export set, so anything that
    # rewrote names or added an export would make the answer meaningless.
    return _verdict_test(
        ctx,
        [wasm[0]] + censuses,
        (["--wasi"] if ctx.attr.wasi else []) + _sink_args(ctx) + _allow_args(ctx) +
        _claims_args("--claims", censuses) + [wasm[0].path],
        "FrustrateWasmBlockCheck",
        # The module, for `wasm-tools` or the scanner's `--report`.
        {"module": depset(wasm)},
        applied = True,
    )

_wasm_test = rule(
    implementation = _wasm_test_impl,
    test = True,
    attrs = _SCANNER_ATTRS | _SINK_ATTRS | {
        "library": attr.label(mandatory = True, cfg = _wasm_check_transition),
        "platform": attr.label(mandatory = True),
        # The scanner treats `poll_oneoff` as a wait.
        "wasi": attr.bool(),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def _native_library(ctx):
    """The library as the build has it, or built for the analysis's platform."""
    return ctx.attr.library or ctx.attr.elsewhere[0]

def _modules_list(ctx, modules, name = None):
    listing = ctx.actions.declare_file((name or ctx.label.name) + ".modules")
    args = ctx.actions.args()
    args.set_param_file_format("multiline")
    args.add_all(modules)
    ctx.actions.write(listing, args)
    return listing

def _native_test_impl(ctx):
    censuses = _censuses(ctx)
    own, rest = _program(_native_library(ctx))
    listing = _modules_list(ctx, rest)
    return _verdict_test(
        ctx,
        [own, listing] + rest + censuses,
        ["--ir", "--modules", listing.path] + _sink_args(ctx) + _allow_args(ctx) +
        _claims_args("--claims", censuses) + [own.path],
        "FrustrateNativeBlockCheck",
        applied = True,
    )

_native_test = rule(
    implementation = _native_test_impl,
    test = True,
    attrs = _SCANNER_ATTRS | _SINK_ATTRS | {
        # The library in the build's configuration, where the build targets
        # the analysis's platform; otherwise None.
        "library": attr.label(aspects = [_crate_ir_aspect]),
        # Otherwise the library built for the platform; otherwise None.
        "elsewhere": attr.label(
            cfg = _native_check_transition,
            aspects = [_crate_ir_aspect],
        ),
        "platform": attr.label(mandatory = True),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

_ShippedInfo = provider(
    doc = "Every program rustc links beneath a target, in the configuration it is built in there.",
    fields = {"programs": "depset[struct(label, own, modules, std)]"},
)

def _deps_of(value):
    if type(value) == "Target":
        return [value]
    if type(value) == "list":
        return [v for v in value if type(v) == "Target"]
    if type(value) == "dict":
        return [v for v in value.keys() + value.values() if type(v) == "Target"]
    return []

def _shipped_aspect_impl(target, ctx):
    transitive = [
        d[_ShippedInfo].programs
        for name in dir(ctx.rule.attr)
        for d in _deps_of(getattr(ctx.rule.attr, name))
        if _ShippedInfo in d
    ]
    found = []
    if _CrateIrInfo in target and target[_CrateIrInfo].std:
        info = target[_CrateIrInfo]
        found.append(struct(label = target.label, own = info.own, modules = info.modules, std = info.std))
    return [_ShippedInfo(programs = depset(found, transitive = transitive))]

# Down every attribute of a shipped app to the libraries in it, each in the
# configuration the app's own rules give it, with its IR beside it.
_shipped_aspect = aspect(
    implementation = _shipped_aspect_impl,
    attr_aspects = ["*"],
    requires = [_crate_ir_aspect],
)

def _shipped_test_impl(ctx):
    censuses = _censuses(ctx)
    want = ctx.attr.library.label
    programs = {}
    for p in ctx.attr.app[_ShippedInfo].programs.to_list():
        if p.label == want:
            programs[p.own.path] = p
    if not programs:
        fail("frustrate_no_block_check: %s does not ship %s, so its analysis " % (ctx.attr.app.label, want) +
             "would check nothing. Name the app that bundles the library.")
    verdicts = []
    applied = []
    for i, p in enumerate(sorted(programs.values(), key = lambda p: p.own.path)):
        rest = [m for m in p.modules.to_list() if m != p.own] + [p.std]
        name = "%s.%d" % (ctx.label.name, i)
        listing = ctx.actions.declare_file(name + ".modules")
        args = ctx.actions.args()
        args.set_param_file_format("multiline")
        args.add_all(rest)
        ctx.actions.write(listing, args)
        verdict = ctx.actions.declare_file(name + ".verdict")
        out = ctx.actions.declare_file(name + ".applied")
        ctx.actions.run(
            executable = ctx.executable._no_block_check,
            inputs = [p.own, listing] + rest + censuses,
            outputs = [verdict, out],
            arguments = ["--applied", out.path, "--ir", "--modules", listing.path] +
                        _sink_args(ctx) + _allow_args(ctx) + _claims_args("--claims", censuses) +
                        [p.own.path, str(ctx.label), verdict.path],
            mnemonic = "FrustrateNativeBlockCheck",
            progress_message = "checking no_block claims in %{label}",
        )
        verdicts.append(verdict)
        applied.append(out)
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = exe, target_file = ctx.executable._runner, is_executable = True)
    return [
        DefaultInfo(executable = exe, files = depset(verdicts), runfiles = ctx.runfiles(files = verdicts)),
        RunEnvironmentInfo(environment = {"FRUSTRATE_NO_BLOCK_VERDICT": ":".join([v.short_path for v in verdicts])}),
        OutputGroupInfo(silences = depset(applied)),
    ]

# A native analysis over the library as a shipped app builds it: one verdict
# for each configuration the app builds it in (an Android app's ABIs, say).
_shipped_test = rule(
    implementation = _shipped_test_impl,
    test = True,
    attrs = _SCANNER_ATTRS | _SINK_ATTRS | {
        "app": attr.label(mandatory = True, aspects = [_shipped_aspect]),
        # Only its label: which library in the app to analyse.
        "library": attr.label(mandatory = True),
    },
)

def _silences_test_impl(ctx):
    censuses = _censuses(ctx)
    applied = [f for c in ctx.attr.checks for f in c[OutputGroupInfo].silences.to_list()]
    return _verdict_test(
        ctx,
        censuses + applied,
        _allow_args(ctx) + _claims_args("--silences", censuses) + [f.path for f in applied],
        "FrustrateNoBlockSilences",
    )

# A silence's name is what one target's finding names, so only the union of
# the analyses can say one matches nothing. Depending on every analysis makes
# this incompatible wherever one of them is, so it is never judged on a part.
_silences_test = rule(
    implementation = _silences_test_impl,
    test = True,
    attrs = _SCANNER_ATTRS | {
        "allow": attr.string_list(),
        "checks": attr.label_list(mandatory = True, providers = [OutputGroupInfo]),
    },
)

def _native_fixture_transition_impl(settings, attr):
    out = {}
    for platform in attr.platforms:
        config = _native_check_transition_impl(settings, struct(platform = platform))
        config[_EXTRA_RUSTC_FLAGS] = settings[_EXTRA_RUSTC_FLAGS] + ["--cfg=" + c for c in attr.cfgs]
        out[platform.name] = config
    return out

_native_fixture_transition = transition(
    implementation = _native_fixture_transition_impl,
    inputs = [_EXTRA_RUSTC_FLAGS],
    outputs = [
        "//command_line_option:platforms",
        _ALLOCATOR_LIBRARY,
        _EXTRA_RUSTC_FLAGS,
    ],
)

def _native_artifacts_impl(ctx):
    files = _censuses(ctx)
    for platform, library in ctx.split_attr.library.items():
        own, rest = _program(library)
        listing = _modules_list(ctx, rest, "%s/%s" % (ctx.label.name, platform))
        program = ctx.actions.declare_file("%s/%s.ll" % (ctx.label.name, platform))
        ctx.actions.run(
            executable = ctx.executable._no_block_check,
            inputs = [own, listing] + rest,
            outputs = [program],
            arguments = ["--link", program.path, "--modules", listing.path, own.path],
            mnemonic = "FrustrateNoBlockLink",
            progress_message = "linking the no_block program of %{label}",
        )
        files.append(program)
        for i, m in enumerate([own] + rest):
            module = ctx.actions.declare_file("%s/%s/%d.module.ll" % (ctx.label.name, platform, i))
            ctx.actions.symlink(output = module, target_file = m)
            files.append(module)
    return [DefaultInfo(files = depset(files))]

# The native analysis's inputs with no verdict: the claim census of each
# bridge and, for each platform, the program the analysis reads as one file,
# `<name>/<platform>.ll`, and each module it is linked from,
# `<name>/<platform>/<n>.module.ll`. For a test that runs the scanner over a
# fixture it expects to fail (//tests/bazel_rules/native_red:native_red_test),
# and the reader's check against LLVM's call graph.
_no_block_check_artifacts = rule(
    implementation = _native_artifacts_impl,
    attrs = {
        "bridges": attr.label_list(mandatory = True, allow_empty = False, providers = [OutputGroupInfo]),
        "library": attr.label(
            mandatory = True,
            cfg = _native_fixture_transition,
            aspects = [_crate_ir_aspect],
        ),
        # Extra cfgs for every crate in the build, for a fixture that turns on
        # test-only code in the runtime.
        "cfgs": attr.string_list(),
        # The platforms to build the IR for: any of the native analyses'.
        "platforms": attr.label_list(mandatory = True),
        "_no_block_check": attr.label(
            default = Label("//bazel/no_block_check"),
            executable = True,
            cfg = "exec",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

# ----------------------------------------------------------------- macro --

def _bridges(bridge):
    """`bridge` as the list it names: one frustrate_bridge, or one per linked bridge."""
    return [bridge] if type(bridge) == "string" or type(bridge) == "Label" else list(bridge)

def no_block_check_artifacts(name, bridge, **kwargs):
    """`_no_block_check_artifacts`, with `bridge` one label or a list of them."""
    _no_block_check_artifacts(name = name, bridges = _bridges(bridge), **kwargs)

def frustrate_no_block_check(
        name,
        bridge,
        library = None,
        targets = None,
        slow = [],
        io = [],
        allow = [],
        web_threads_platform = None,
        shipped = {},
        **kwargs):
    """Settle every `#[bridge(no_block)]` claim in a bridge, for the targets it ships on.

    `#[bridge(no_block)]` is the author's claim that main-thread Dart calling
    the member cannot be stalled: it reaches no wait, no function named in
    `slow`, and no IO in an `io` class. Codegen settles what placement
    settles (an actor member's body, a dispatched member's body) and writes a
    claim census; the rest is settled here, over the code of `library`.
    Nothing this builds ships.

    `targets` says where the bridge ships, and each analysis runs only where
    it proves something there. They are tests under `name`, a test_suite:

    - `android`, `ios`, `linux`, `macos` → `<name>.<os>_<cpu>`, one per CPU
      frustrate builds that OS for: `android_aarch64` and `android_x86_64`;
      `ios_aarch64` (a device); `linux_x86_64` and `linux_aarch64` (glibc);
      `macos_aarch64`. Each scans the LLVM IR of every Rust crate in the
      library for `slow`, `io`, and any call out of the Rust program that its
      C library (Apple's libSystem, Android's bionic, glibc) is not known to
      return from without waiting; a C dependency's function is "cannot
      verify" until a silence names it.

      Where the build already targets the analysis's platform (a Mac's own
      `bazel test`, for `macos_aarch64`), the analysis reads the library as
      that build has it: the same configuration, the same compiled
      dependencies. Each crate's IR comes from that crate's own Rustc command
      run again with `--emit=llvm-ir` and `-Copt-level=0` (keeping the
      crate's debug assertions and overflow checks) and the check's cfg,
      which adds the roots; std's part comes from the bitcode its rlibs ship
      with, through rustc's fat LTO of a crate naming what the program uses
      of it. So the analysis costs one IR emission per crate the build
      compiles, and a change costs what it recompiles, again. Elsewhere it
      builds the library for the platform, in the build's configuration
      otherwise. The test and the scanner run on the host and nothing is
      linked, so every analysis runs from any host with no SDK, NDK or C
      toolchain for the platform. What each needs is the platform's Rust
      std: the root module's `rust.toolchain` lists the triple in
      `extra_target_triples` (`aarch64-linux-android`,
      `x86_64-linux-android`, `aarch64-apple-ios`, and the Linux and macOS
      triples rules_rust lists by default), and the crate universe the
      bridge's dependencies come from supports it. A dependency that
      compiles C for the platform also needs the platform's C toolchain
      registered. A crate built by a rule other than rules_rust's has no IR,
      and the analysis refuses a program that calls into one.
      With `shipped` naming an app for the OS, the OS's analysis is
      `<name>.<os>`, over the library as that app builds it (see `shipped`).
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
    - `wasi` → `<name>.wasi`: the crate built for //bazel:wasm32_wasip1 with
      the stock std, scanned for `poll_oneoff` (wasip1's only wait), `slow`,
      and `io`. Hermetic.

    And `<name>.silences`, which fails on a silence that no analysis
    applied (see `allow`). It needs every analysis, so it is SKIPPED wherever
    one of them is.

    SKIPPED is Bazel's report that an analysis cannot run here, not a pass:
    it is in every `bazel test` summary, and naming the skipped test directly
    is an error that says why.

    Without `library`, the target declares that every claim is settled by
    placement: it builds nothing, and is one test, `name`, that reads the
    census and fails naming any claim that wants an artifact. A crate whose
    claims are all placed is refused with `library`, for the same reason.

    Each root takes its member's request bytes as parameters, so no
    optimizer can delete a body behind a decode it proved fails, and a
    native analysis reports a root that does not reach its member's own
    function as "cannot verify". The wasm analyses append `-Copt-level=0`
    to `--@rules_rust//rust/settings:extra_rustc_flag` for readable witness
    paths; a crate's own `rustc_flags` reach rustc after it. Their
    configurations also set `--cfg=frustrate_no_block_check -Adead_code`,
    appended to the platform's own flags.

    For `web_threads`, the whole graph must build `+atomics`, and
    crate_universe resolves deps per triple, before RUSTFLAGS exist, so it
    drops any dependency declared under `cfg(target_feature = ...)`.
    getrandom 0.4.3 loses `js-sys` that way, with an `E0433` inside
    getrandom. Two ways out, and the graph decides which one fits (see
    docs/ext.md); the check builds under whichever your app does.

    1. The ext platform, when nothing in the graph enables getrandom 0.4's
       `wasm_js` feature. Depend on
       `@frustrate//ext/getrandom:frustrate_getrandom`, write
       `use frustrate_getrandom as _;` in the crate, build the app on
       `@frustrate//ext/getrandom:wasm32_threads`, and name that platform to
       the check:

           frustrate_no_block_check(
               ...
               web_threads_platform = "@frustrate//ext/getrandom:wasm32_threads",
           )

       getrandom then takes its `custom` backend and never compiles the
       `wasm_js` module that names `js-sys`, and no version appears in your
       build files. Entropy comes through frustrate's host import instead of
       getrandom's own `wasm_js`; both are `crypto.getRandomValues`. With that
       feature on, 0.4.3 fails with `E0599` on `Error::WEB_CRYPTO`.
    2. A crate annotation in your root MODULE.bazel, which keeps `wasm_js`;
       the route when the feature is on:

           crate.annotation_select(
               crate = "getrandom",
               version = "0.4.3",
               deps = ["@your_crates//:js-sys"],
               repositories = ["your_crates"],
               triples = ["wasm32-unknown-unknown"],
           )

       It names getrandom's version. When the lockfile moves getrandom to
       another version it stops matching, without an error, and the
       `E0433` comes back.

    The `web_threads` module links through `frustrate-link-filter`, which
    the custom-std toolchains carry, so a graph containing wasm-bindgen still
    has only the roots as exports; //bazel/no_block_check/link_filter.rs
    says what it can and cannot break.

    Args:
      name: The test_suite; the analyses are `<name>.<target>`.
      bridge: The frustrate_bridge that generated the crate's glue, for its
        claim census. For a `frustrate_shared_library` that links several bridges, a
        list of every one of them: the check settles all their claims over the
        one build, and attributes each root to its bridge by the crate name the
        root carries. A root that no listed bridge claims fails the check, so a
        bridge the library links and the list lacks is not skipped.
      library: The bridge's `rust_shared_library`. Omit it when every claim is
        settled by placement.
      targets: Where the bridge ships: any of `android`, `ios`, `linux`,
        `macos`, `web`, `web_threads`, `wasi`. Required with `library`.
      slow: Rust paths a claimed body may not reach, checked by every
        analysis: a slow-work marker (`my_crate::main_thread::slow`), or a
        third-party function. Each matches every function whose demangled
        path starts with it, whole segments only; start with the crate's
        name, never `crate::`. A marker must be `#[inline(never)]`: even
        `#[inline]` vanishes at opt-level 0. A native analysis refuses a
        path that names no function in the program, since its build keeps
        every production export. A wasm module keeps only what the claims
        reach, so with no native target a misspelt path stays green: try a
        new one once by calling it from a claimed member.
      io: IO classes a claimed body may not do: `files` (opening, naming,
        reading or syncing a file), `network` (sockets and name lookup),
        `process` (starting a program). frustrate maps each to the target's
        calls: C library functions natively, `wasi_snapshot_preview1`
        imports on wasi. A native analysis already reports every such call
        as "cannot verify", so there a class only makes the finding red and
        names it; `wasi` finds the calls only through a class. Sleeping needs no class:
        it is a wait everywhere.
        Writes to an open descriptor are in no class, because stdout, stderr
        and the panic report are such writes. //bazel/no_block_check/io.rs
        has the maps and what each covers.
      web_threads_platform: The platform `web_threads` builds under, when the
        app ships on one other than the default `//bazel:wasm32_threads`: a
        `platform()` that `parents` it, such as
        `@frustrate//ext/getrandom:wasm32_threads`. It must be a threaded
        platform, or the check finds no wait instruction. Its `flags` reach
        the check's build, so what the app compiles is what is scanned.
      shipped: For a native OS, the app target that ships the library there
        (`{"ios": "//app:ios", "android": "//app:android"}`). The OS's
        analysis is then one test, `<name>.<os>`, over the library as that app
        builds it: the analysis follows the app's own rules down to the
        library, so it reads the library in each configuration the app
        builds it in (one verdict each), and a build of the app with the
        same flags shares its compiled crates. Without it, `<name>.<os>_<cpu>`
        builds the library for frustrate's platform for that OS and CPU.
      allow: Silences for every claim the target checks, for code you cannot
        annotate. A name is what a finding says to silence it as, and each
        analysis names its findings its own way (natively a wait is the C
        function that waits, on `web_threads` the function holding the wait
        instruction), so a finding on two targets wants a name for each. In
        your own code, put `#[frustrate::no_block(allow)]` on the function
        instead, which every analysis reads the same way; for one member's
        finding, `no_block(allow(<name>))` in its `#[bridge(...)]`, its
        impl's, or its file's `bridge_file!`. Each analysis lists the
        silences it applied, and `<name>.silences` fails on one that none of
        them applied.
      **kwargs: Common attributes (`tags`, `visibility`, `size`, ...) for
        every test and the suite.
    """
    bridges = _bridges(bridge)
    if not library:
        if slow or io or allow or targets != None:
            fail("frustrate_no_block_check: %s has no `library`, so it builds " % name +
                 "nothing and `slow`, `io`, `allow` and `targets` would check nothing. " +
                 "Name the library, or drop them.")
        _census_test(name = name, bridges = bridges, **kwargs)
        return

    if not targets:
        fail("frustrate_no_block_check: %s names a `library` but no `targets`. " % name +
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
             "code, or drop `library` if every claim is placed.")
    if web_threads_platform and "web_threads" not in analyses:
        fail("frustrate_no_block_check: %s names `web_threads_platform` but " % name +
             "`targets` has no `web_threads`, so nothing would build under it.")
    for os in shipped:
        if os not in _NATIVE or os not in analyses:
            fail("frustrate_no_block_check: `shipped` names %r, which is not a native " % os +
                 "OS in `targets`.")
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
        if os in shipped:
            _shipped_test(
                name = "%s.%s" % (name, os),
                bridges = bridges,
                app = shipped[os],
                library = library,
                slow = slow,
                io = io,
                allow = allow,
                tags = tags,
                target_compatible_with = compatible,
                **kwargs
            )
            tests.append("%s.%s" % (name, os))
            continue
        for cpu in _NATIVE[os]:
            test = "%s.%s_%s" % (name, os, cpu)
            here = Label("//bazel:%s_%s.targeted" % (os, cpu))
            _native_test(
                name = test,
                bridges = bridges,
                library = select({here: library, "//conditions:default": None}),
                elsewhere = select({here: None, "//conditions:default": library}),
                platform = Label("//bazel:%s_%s" % (os, cpu)),
                slow = slow,
                io = io,
                allow = allow,
                tags = tags,
                target_compatible_with = compatible,
                **kwargs
            )
            tests.append(test)
    if "web_threads" in analyses:
        _wasm_test(
            name = name + ".web_threads",
            bridges = bridges,
            library = library,
            slow = slow,
            allow = allow,
            platform = web_threads_platform or Label("//bazel:wasm32_threads"),
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
            bridges = bridges,
            library = library,
            slow = slow,
            io = io,
            allow = allow,
            platform = Label("//bazel:wasm32_wasip1"),
            wasi = True,
            tags = tags,
            target_compatible_with = compatible,
            **kwargs
        )
        tests.append(name + ".wasi")
    _silences_test(
        name = name + ".silences",
        bridges = bridges,
        checks = [":" + t for t in tests],
        allow = allow,
        tags = tags,
        target_compatible_with = compatible,
        **kwargs
    )
    tests.append(name + ".silences")
    native.test_suite(
        name = name,
        tests = [":" + t for t in tests],
        tags = tags,
        **suite_kwargs
    )
