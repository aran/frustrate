"""Hot patching a running bridge library: the build half.

`frustrate_hot_patchable` declares two targets over one bridge cdylib:

* `<name>` — the library, built so a running copy can take patches. Name it as
  `flutter_native_library.library`.
* `<name>.hot_patch` — what builds those patches. Name it as
  `flutter_native_library.hot_patch`.

Only a `-c dbg` build is patchable. There the library is compiled with
`--cfg=frustrate_hot_patch`, which routes the generated entry points through
patchable slots and adds the exports a loader calls, and it is linked to hold
every function its crates compiled rather than only the ones the build happened
to call: a patch binds what it does not carry to the running image by symbol, so
what the image lacks it cannot use. That makes a patchable debug library bigger
than a plain one — measurably so for a large dependency graph, and a device
reinstalls it on every restart. Under any other compilation mode the transition
changes nothing, so a release build is the plain library, byte for byte.

The manifest's command is frustrate's patch builder (`//hotpatch`). It reads
the library, the LLVM IR of the same compilation, the binding contract and a
list of every other input the compilation read, and it answers each reload
with one JSON line. See `hotpatch/src/main.rs` for the protocol.
"""

_EXTRA_RUSTC_FLAGS = str(Label("@rules_rust//rust/settings:extra_rustc_flags"))
_PER_CRATE_RUSTC_FLAG = str(Label("@rules_rust//rust/settings:per_crate_rustc_flag"))
_HOT_PATCH_FLAG = str(Label("//bazel:hot_patch"))
_HOT_PATCH_CFG = "--cfg=frustrate_hot_patch"

def _hot_patch_transition_impl(settings, attr):
    flags = list(settings[_EXTRA_RUSTC_FLAGS])
    per_crate = list(settings[_PER_CRATE_RUSTC_FLAG])

    # Debug only, and identity otherwise: returning the settings unchanged is
    # what keeps a release build in its own configuration, so nothing a release
    # ships was compiled with any of this.
    #
    # str(): Bazel hands a transition the option's own enum value, which never
    # compares equal to a string.
    patchable = str(settings["//command_line_option:compilation_mode"]) == "dbg"
    if patchable and _HOT_PATCH_CFG not in flags:
        # The cfg reaches every crate in the configuration, because the routed
        # entry points are emitted in the bridge crate's generated glue and the
        # loader exports live in //runtime/rust.
        flags.append(_HOT_PATCH_CFG)

        # `-Clink-dead-code` on the library alone. It does two things: rustc
        # stops passing the linker its dead-code strip, so the image keeps every
        # function of every archive member the link pulled in — which is what
        # lets a patch call something the launch build never called — and rustc
        # collects mono items eagerly for the crate carrying the flag. The
        # second is why this is per-crate: measured on e2e/iroh_demo's bridge,
        # eager collection for every dependency as well costs 37 MB of debug
        # library (88.6 MB against 125.5 MB) and buys nothing, since what a
        # patch needs from a dependency is code that dependency already
        # compiled.
        # `patched_crate` rather than `library`, so a rule that compiles an
        # edited copy of the crate can name the crate being patched and land in
        # the same configuration: the configuration's name reaches source paths
        # inside the IR, and two IRs compared against each other must agree on
        # them.
        patched = getattr(attr, "patched_crate", None) or attr.library
        per_crate.append("{}@-Clink-dead-code".format(_prefix_filter(patched)))
    return {
        _EXTRA_RUSTC_FLAGS: flags,
        _PER_CRATE_RUSTC_FLAG: per_crate,
        _HOT_PATCH_FLAG: patchable,
    }

def _prefix_filter(library):
    """The library's label as rules_rust matches `per_crate_rustc_flag` against."""
    label = str(library)
    for prefix in ("@@//", "@//"):
        if label.startswith(prefix):
            return label[len(prefix) - 2:]
    return label

hot_patch_transition = transition(
    implementation = _hot_patch_transition_impl,
    inputs = [
        "//command_line_option:compilation_mode",
        _EXTRA_RUSTC_FLAGS,
        _PER_CRATE_RUSTC_FLAG,
    ],
    outputs = [_EXTRA_RUSTC_FLAGS, _PER_CRATE_RUSTC_FLAG, _HOT_PATCH_FLAG],
)

HotPatchIrInfo = provider(
    doc = "The LLVM IR of a crate's own compilation, and what else that compilation read.",
    fields = {
        "ir": "File: textual LLVM IR, from the crate's Rustc argv with only the emit and codegen-unit count changed.",
        "argv": "list[str]: the crate's Rustc argv (process_wrapper first), as Bazel ran it.",
        "env": "dict[str, str]: that action's environment.",
        "compile_inputs": "depset[File]: every input of that action.",
        "inputs": "depset[File]: every input of that action outside the crate's own sources.",
        "sources": "depset[File]: source files in the main repository read by any Rust compilation this crate depends on, itself included.",
    },
)

_SourcesInfo = provider(fields = ["sources"])

def _main_repo_sources(action):
    return [f for f in action.inputs.to_list() if f.is_source and f.owner and f.owner.workspace_name == ""]

def _rust_sources_aspect_impl(target, ctx):
    # The rule's own source attributes as well as its actions' inputs: a crate
    # with generated sources compiles symlinks to its checked-in files, which
    # are outputs as far as the action is concerned.
    direct = [
        f
        for attr in ("srcs", "compile_data")
        for f in getattr(ctx.rule.files, attr, [])
        if f.is_source and f.owner and f.owner.workspace_name == ""
    ]
    for action in target.actions:
        if action.mnemonic in ("Rustc", "RustcMetadata", "CargoBuildScriptRun"):
            direct.extend(_main_repo_sources(action))
    transitive = []
    for attr in ("deps", "proc_macro_deps", "crate", "build_script", "srcs"):
        value = getattr(ctx.rule.attr, attr, None)
        if value == None:
            continue
        for dep in (value if type(value) == "list" else [value]):
            if _SourcesInfo in dep:
                transitive.append(dep[_SourcesInfo].sources)
    return [_SourcesInfo(sources = depset(direct, transitive = transitive))]

rust_sources_aspect = aspect(
    implementation = _rust_sources_aspect_impl,
    attr_aspects = ["deps", "proc_macro_deps", "crate", "build_script", "srcs"],
    provides = [_SourcesInfo],
)

def _hot_patch_ir_aspect_impl(target, ctx):
    # A cdylib carries no CrateInfo (nothing can link it as a crate), so the
    # Rustc action is what identifies a Rust compilation here.
    rustc = [a for a in target.actions if a.mnemonic == "Rustc"]
    if not rustc:
        return []
    if len(rustc) != 1:
        fail("%s: expected exactly one Rustc action, found %d" % (target.label, len(rustc)))
    action = rustc[0]

    ir = ctx.actions.declare_file(ctx.label.name + ".hot_patch.ll")
    args = []
    emits = 0
    for arg in action.argv[1:]:
        if arg.startswith("--emit="):
            args.append("--emit=llvm-ir=" + ir.path)
            emits += 1
        else:
            args.append(arg)
    if emits != 1:
        fail("%s: expected one --emit in the Rustc argv, found %d" % (target.label, emits))

    # One module, so one .ll. The library itself was built with rustc's default
    # unit count; the patch builder binds by symbol name, which does not depend
    # on it, and a name the library lacks is carried or refused, never guessed.
    args.append("-Ccodegen-units=1")

    ctx.actions.run(
        executable = action.argv[0],
        arguments = args,
        inputs = action.inputs,
        outputs = [ir],
        env = action.env,
        mnemonic = "FrustrateHotPatchIr",
        progress_message = "Emitting hot patch IR for %{label}",
    )

    # By short path: a crate with generated sources compiles symlinks to its
    # checked-in files, and those are the crate's own sources too.
    own = {f.short_path: True for f in ctx.rule.files.srcs + getattr(ctx.rule.files, "compile_data", [])}
    others = [f for f in action.inputs.to_list() if f.short_path not in own]
    return [OutputGroupInfo(hot_patch_ir = depset([ir])), HotPatchIrInfo(
        ir = ir,
        argv = action.argv,
        env = action.env,
        compile_inputs = action.inputs,
        inputs = depset(others),
        sources = target[_SourcesInfo].sources,
    )]

hot_patch_ir_aspect = aspect(
    implementation = _hot_patch_ir_aspect_impl,
    required_aspect_providers = [[_SourcesInfo]],
)

def _forward_impl(ctx):
    library = ctx.attr.library[0]
    return [DefaultInfo(files = library[DefaultInfo].files)]

frustrate_hot_patchable_library = rule(
    implementation = _forward_impl,
    doc = "The bridge cdylib, in the configuration `frustrate_hot_patch` builds patches for.",
    attrs = {
        "library": attr.label(mandatory = True, cfg = hot_patch_transition),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def _hot_patch_impl(ctx):
    library_target = ctx.attr.library[0]
    info = library_target[HotPatchIrInfo]
    libraries = [
        f
        for f in library_target[DefaultInfo].files.to_list()
        if f.extension in ("dylib", "so")
    ]
    if len(libraries) != 1:
        fail("%s: expected one .dylib or .so from %s, found %d" % (ctx.label, ctx.attr.library[0].label, len(libraries)))
    library = libraries[0]

    toolchain = ctx.toolchains[str(Label("@rules_rust//rust:toolchain_type"))]
    if not toolchain.llvm_cov:
        fail((
            "%s: hot patching compiles with the Rust toolchain's own `llc`, which ships in " +
            "its llvm-tools component, and this toolchain has none. Declare the toolchain " +
            "with llvm-tools (rules_rust's `rust.toolchain` fetches it by default)."
        ) % ctx.label)

    # rules_rust names no target for `llc`, but it ships in the same llvm-tools
    # archive as `llvm-cov`, which the toolchain does declare. Staging llvm-cov
    # is what puts that directory in the execution root.
    llc_path = toolchain.llvm_cov.dirname + "/llc"

    inputs_list = ctx.actions.declare_file(ctx.label.name + ".inputs")
    ctx.actions.write(inputs_list, "\n".join([f.path for f in info.inputs.to_list()]) + "\n")

    command = [ctx.executable._tool.path]
    command.extend(["--library", library.path])
    command.extend(["--ir", info.ir.path])
    command.extend(["--contract", ctx.file.binding_contract.path])
    command.extend(["--inputs", inputs_list.path])
    command.extend(["--llc", llc_path])
    for arg in info.argv:
        command.extend(["--rustc-arg", arg])
    for key, value in sorted(info.env.items()):
        command.extend(["--env", key + "=" + value])

    manifest = ctx.actions.declare_file(ctx.label.name + ".hot_patch.json")
    ctx.actions.write(manifest, json.encode_indent({
        "version": 1,
        "library": library.path,
        "sources": sorted({f.path: True for f in info.sources.to_list()}.keys()),
        "command": command,
    }) + "\n")

    # The compilation's other inputs are listed too: the patch builder reads
    # them from the execution root, and an intermediate output that came from a
    # cache is not downloaded unless something at the top asks for it. A tool's
    # runfiles tree is among them and is not a file a consumer can stage, so it
    # is left out.
    inputs = [f for f in info.inputs.to_list() if not f.path.endswith(".runfiles") and ".runfiles/" not in f.path]
    return [DefaultInfo(files = depset(
        [manifest, ctx.executable._tool, library, info.ir, ctx.file.binding_contract, inputs_list, toolchain.llvm_cov] + inputs,
    ))]

_frustrate_hot_patch = rule(
    implementation = _hot_patch_impl,
    doc = "Builds the patch manifest for a `frustrate_hot_patchable_library`.",
    attrs = {
        "library": attr.label(
            mandatory = True,
            cfg = hot_patch_transition,
            aspects = [rust_sources_aspect, hot_patch_ir_aspect],
        ),
        "binding_contract": attr.label(mandatory = True, allow_single_file = True),
        "_tool": attr.label(
            default = Label("//hotpatch:frustrate_hotpatch"),
            executable = True,
            cfg = "exec",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
    toolchains = [str(Label("@rules_rust//rust:toolchain_type"))],
)

def frustrate_hot_patchable(name, library, binding_contract, visibility = None, **kwargs):
    """A bridge cdylib that a running app can patch, and what builds the patches.

    Args:
        name: `<name>` is the library to bundle; `<name>.hot_patch` builds patches for it.
        library: the bridge crate's `rust_shared_library`.
        binding_contract: the bridge's `<bridge>.ir`. A patch never changes it:
            a reload whose contract moved answers `restart`.
        visibility: applied to both targets.
        **kwargs: common attributes (`tags`, `testonly`) for both targets.
    """
    frustrate_hot_patchable_library(name = name, library = library, visibility = visibility, **kwargs)
    _frustrate_hot_patch(
        name = name + ".hot_patch",
        library = library,
        binding_contract = binding_contract,
        visibility = visibility,
        **kwargs
    )
