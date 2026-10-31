"""Hot patching a running bridge library: the build half.

`frustrate_hot_patchable` declares these over one bridge cdylib, or over a
`frustrate_library` that links several bridges:

* `<name>` — the library, built so a running copy can take patches. Name it as
  `flutter_native_library.library`.
* `<name>.hot_patch` — what builds those patches. Name it as
  `flutter_native_library.hot_patch`; over a `frustrate_library`, every
  bridge's wrapper names this same target, since one patch serves them all.
* `<name>.<library>` — over a `frustrate_library` only: per bridge, the
  `frustrate_code_asset` binding its id to `<name>`. Wrap each in a
  `flutter_native_library` with that bridge's `.ir` as its contract.

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
the library and, for each bridge crate in it, the LLVM IR of that crate's
compilation, its binding contract and a list of every other input the
compilation read, and it answers each reload with one JSON line. See
`hotpatch/src/main.rs` for the protocol.
"""

load("@rules_rust//rust:defs.bzl", "rust_common")
load("//bazel:defs.bzl", "FrustrateBridgeInfo", "frustrate_code_asset")

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

        # `-Clink-dead-code` on the library and its bridge crates alone. It
        # does two things: rustc
        # stops passing the linker its dead-code strip, so the image keeps every
        # function of every archive member the link pulled in — which is what
        # lets a patch call something the launch build never called — and rustc
        # collects mono items eagerly for the crate carrying the flag. The
        # second is why this is per-crate: eager collection for every
        # dependency as well bloats the debug library and buys nothing, since
        # what a patch needs from a dependency is code that dependency already
        # compiled.
        # `patched_crate` rather than `library`, so a rule that compiles an
        # edited copy of a crate can name the library being patched and land in
        # the same configuration: the configuration's name reaches source paths
        # inside the IR, and two IRs compared against each other must agree on
        # them. Sorted for the same reason.
        patched = [getattr(attr, "patched_crate", None) or attr.library] + list(getattr(attr, "crates", None) or [])
        per_crate.extend(sorted(["{}@-Clink-dead-code".format(_prefix_filter(p)) for p in patched]))
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

    # The argv names tools by their paths in the exec configuration the Rustc
    # action resolved, so the copy has to run on that same platform. The aspect
    # asks for exactly the toolchains the Rust rules ask for, and the action
    # names the toolchain rules_rust's Rustc action names, which puts it in the
    # same exec group under --incompatible_auto_exec_groups.
    ctx.actions.run(
        executable = action.argv[0],
        arguments = args,
        inputs = action.inputs,
        outputs = [ir],
        env = action.env,
        toolchain = str(Label("@rules_rust//rust:toolchain_type")),
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
    toolchains = [
        str(Label("@rules_rust//rust:toolchain_type")),
        config_common.toolchain_type("@rules_cc//cc:toolchain_type", mandatory = False),
    ],
)

def _forward_impl(ctx):
    library = ctx.attr.library[0]
    return [DefaultInfo(files = library[DefaultInfo].files)]

frustrate_hot_patchable_library = rule(
    implementation = _forward_impl,
    doc = "The bridge cdylib, in the configuration `frustrate_hot_patch` builds patches for.",
    attrs = {
        "library": attr.label(mandatory = True, cfg = hot_patch_transition),
        # Only read by the transition: the bridge crates of a frustrate_library.
        "crates": attr.label_list(cfg = hot_patch_transition),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def _crate_name(argv):
    for i, arg in enumerate(argv):
        if arg == "--crate-name":
            return argv[i + 1]
        if arg.startswith("--crate-name="):
            return arg[len("--crate-name="):]
    fail("no --crate-name in a Rustc argv")

def _without_runfiles(files):
    # A tool's runfiles tree is left out: it is not a file a consumer can
    # stage, and Bazel does not build one in every configuration, so listing it
    # would make the patch builder read a directory that may not exist. The
    # files the tree links are inputs of their own.
    return [f for f in files if not f.path.endswith(".runfiles") and ".runfiles/" not in f.path]

def _unit(ctx, name, info, contract, skip = {}):
    """The patch builder's options for one compilation, and the files they name."""
    inputs = [f for f in _without_runfiles(info.inputs.to_list()) if f.path not in skip]
    inputs_list = ctx.actions.declare_file("%s.%s.inputs" % (ctx.label.name, name))
    ctx.actions.write(inputs_list, "".join([f.path + "\n" for f in inputs]))
    args = ["--unit", name, "--inputs", inputs_list.path]
    files = [inputs_list] + inputs
    if contract:
        args.extend(["--ir", info.ir.path, "--contract", contract.path])
        files.extend([info.ir, contract])
    for arg in info.argv:
        args.extend(["--rustc-arg", arg])
    for key, value in sorted(info.env.items()):
        args.extend(["--env", key + "=" + value])
    return args, files

_HOST_PLATFORM = "//command_line_option:host_platform"

def _host_transition_impl(settings, _attr):
    # `--host_platform` rather than `@platforms//host`: it is the build's own
    # description of this machine, and may carry constraints the detected one
    # lacks that select its C toolchain.
    return {"//command_line_option:platforms": [settings[_HOST_PLATFORM]]}

_host_transition = transition(
    implementation = _host_transition_impl,
    inputs = [_HOST_PLATFORM],
    outputs = ["//command_line_option:platforms"],
)

_LlcInfo = provider(
    doc = "The Rust toolchain's `llc`: its path, and the files that put it in the execution root.",
    fields = {
        "path": "str: the path of `llc`; None without a toolchain.",
        "files": "list[File]: `llvm-cov`, which `llc` ships beside, and the libLLVM both load.",
    },
)

def _rust_llc_impl(ctx):
    toolchain = ctx.toolchains[str(Label("@rules_rust//rust:toolchain_type"))]
    if toolchain and not toolchain.llvm_cov:
        fail((
            "%s: hot patching compiles with the Rust toolchain's own `llc`, which ships in " +
            "its llvm-tools component, and this toolchain has none. Declare the toolchain " +
            "with llvm-tools (rules_rust's `rust.toolchain` fetches it by default)."
        ) % ctx.label)
    if not toolchain:
        return [_LlcInfo(path = None, files = [])]

    # rules_rust names no target for `llc`, but it ships in the same
    # llvm-tools archive as `llvm-cov`, which the toolchain does declare, and
    # loads the same libLLVM, which it declares too. The repository is fetched
    # whole, so staging those puts `llc` in the execution root beside them.
    llvm_cov = toolchain.llvm_cov
    llc = llvm_cov.dirname + "/llc" + (".exe" if llvm_cov.extension == "exe" else "")
    return [_LlcInfo(path = llc, files = [llvm_cov] + toolchain.llvm_lib)]

frustrate_rust_llc = rule(
    implementation = _rust_llc_impl,
    doc = """The `llc` of the Rust toolchain that targets this configuration's platform and runs on the machine the target's `exec_compatible_with` describes.

Declare it with `exec_compatible_with = HOST_CONSTRAINTS`: the patch builder runs
the `llc` on the developer's machine. See `//hotpatch:llc`.""",
    toolchains = [config_common.toolchain_type(str(Label("@rules_rust//rust:toolchain_type")), mandatory = False)],
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

    # None when no Rust toolchain runs on this machine and targets the
    # library's platform, as for a library a remote test host loads. Then the
    # manifest names no `llc`, and the patch builder refuses each patch saying
    # why, rather than this target failing analysis for everything above it.
    llc = ctx.attr._llc[_LlcInfo]

    command = [ctx.executable._tool.path]
    command.extend(["--library", library.path])
    if llc.path:
        command.extend(["--llc", llc.path])

    # The units: each compilation the patch builder reads. The library's own
    # links the image, so the linker and its flags come from its argv. A single
    # bridge's library is also the crate a patch is built from. A
    # frustrate_library's crate root only names the bridge crates, so each
    # bridge crate is a unit of its own, and the library's unit fingerprints
    # what else it links — leaving out the bridge crates' outputs, which their
    # own units compare by their IR.
    files = []
    if ctx.attr.crates:
        bridge_outputs = {}
        for crate in ctx.attr.crates:
            crate_info = crate[rust_common.crate_info]
            bridge_outputs[crate_info.output.path] = True
            if crate_info.metadata:
                bridge_outputs[crate_info.metadata.path] = True
        args, unit_files = _unit(ctx, _crate_name(info.argv), info, None, bridge_outputs)
        command.extend(args + ["--links"])
        files.extend(unit_files)
        for crate, bridge in zip(ctx.attr.crates, ctx.attr.bridges):
            bridge_info = bridge[FrustrateBridgeInfo]
            if crate[rust_common.crate_info].name != bridge_info.crate_name:
                fail("frustrate_hot_patchable %s: %s generated bindings for crate `%s`, but the library it is paired with, %s, is crate `%s`" % (
                    ctx.label,
                    bridge.label,
                    bridge_info.crate_name,
                    crate.label,
                    crate[rust_common.crate_info].name,
                ))
            args, unit_files = _unit(ctx, bridge_info.crate_name, crate[HotPatchIrInfo], bridge_info.ir)
            command.extend(args)
            files.extend(unit_files)
    else:
        args, unit_files = _unit(ctx, _crate_name(info.argv), info, ctx.file.binding_contract)
        command.extend(args + ["--links"])
        files.extend(unit_files)

    manifest = ctx.actions.declare_file(ctx.label.name + ".hot_patch.json")
    ctx.actions.write(manifest, json.encode_indent({
        "version": 1,
        "library": library.path,
        "sources": sorted({f.path: True for f in info.sources.to_list()}.keys()),
        "command": command,
    }) + "\n")

    # Only a -c dbg library can take a patch, and the dev tool reads the
    # manifest only there. Elsewhere the manifest alone is the output, so a
    # test or release build of `//...` compiles no IR and stages nothing.
    if ctx.var["COMPILATION_MODE"] != "dbg":
        return [DefaultInfo(files = depset([manifest]))]

    # The compilations' other inputs, which the patch builder fingerprints and
    # reads from the execution root, are staged too: an intermediate output
    # that came from a cache is not downloaded unless something at the top
    # asks for it.
    return [DefaultInfo(files = depset(
        [manifest, ctx.executable._tool, library] + llc.files + files,
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
        "binding_contract": attr.label(allow_single_file = True),
        # A frustrate_library's bridge crates (rlibs), and the frustrate_bridge
        # each was generated by, in the same order.
        "crates": attr.label_list(
            cfg = hot_patch_transition,
            aspects = [rust_sources_aspect, hot_patch_ir_aspect],
            providers = [rust_common.crate_info],
        ),
        "bridges": attr.label_list(providers = [FrustrateBridgeInfo]),
        # The patch builder runs beside the app after the build, not in an
        # action, so it is built for the machine running Bazel whatever
        # platform the app targets or actions run on.
        "_tool": attr.label(
            default = Label("//hotpatch:frustrate_hotpatch"),
            executable = True,
            cfg = _host_transition,
        ),
        "_llc": attr.label(default = Label("//hotpatch:llc"), providers = [_LlcInfo]),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def frustrate_hot_patchable(name, library, binding_contract = None, bridges = None, visibility = None, **kwargs):
    """A bridge library that a running app can patch, and what builds the patches.

    Over one bridge, give its cdylib and its contract:

        frustrate_hot_patchable(name = "x_hot", library = ":x", binding_contract = ":bridge.ir")

    Over a `frustrate_library`, give it and the same `bridges` dict:

        frustrate_hot_patchable(name = "app_hot", library = ":app", bridges = BRIDGES)

    and wrap each bridge's code asset for rules_flutter, all naming one patch
    builder:

        flutter_native_library(
            name = "app_native.ping",
            library = ":app_hot.ping_lib",  # `<name>.<the bridge's Rust library's target name>`
            binding_contract = ["//ping:codegen.ir"],
            hot_patch = ":app_hot.hot_patch",
        )

    A patch never changes a contract in a way it cannot serve: a reload whose
    contract moved past that, in any bridge, answers `restart`.

    Args:
        name: `<name>` is the library to bundle; `<name>.hot_patch` builds patches for it.
        library: the bridge crate's `rust_shared_library`, or a `frustrate_library`.
        binding_contract: one bridge's `<bridge>.ir`. Not with `bridges`.
        bridges: for a `frustrate_library`, the dict it was given. Also
            declares `<name>.<library>`, each bridge's code asset over `<name>`.
        visibility: applied to every target.
        **kwargs: common attributes (`tags`, `testonly`) for every target.
    """
    if (binding_contract == None) == (bridges == None):
        fail("frustrate_hot_patchable %s: give `binding_contract` for one bridge, or `bridges` for a frustrate_library" % name)
    crates = [native.package_relative_label(lib) for lib in (bridges or {})]
    frustrate_hot_patchable_library(name = name, library = library, crates = crates, visibility = visibility, **kwargs)
    _frustrate_hot_patch(
        name = name + ".hot_patch",
        library = library,
        binding_contract = binding_contract,
        crates = crates,
        bridges = list((bridges or {}).values()),
        visibility = visibility,
        **kwargs
    )
    for lib, bridge in zip(crates, (bridges or {}).values()):
        frustrate_code_asset(
            name = "%s.%s" % (name, lib.name),
            bridge = bridge,
            library = name,
            visibility = visibility,
            **kwargs
        )
