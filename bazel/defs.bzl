"""frustrate bridge rules.

`frustrate_bridge` runs the codegen as a declared action over explicitly
declared bridge sources, producing the Rust glue module and the Dart bindings
as separate output groups, and declares a filegroup for each.

The Rust output is declared as `<name>/src/frustrate_generated.rs` so that,
combined with the bridge crate's own `src/`, rules_rust's unified source tree
gives `mod frustrate_generated;` the layout it expects. The Dart output is
declared under the package's `lib/` so a dart_library in the same package
gets a conventional package root.

`frustrate_bridge_library` is the companion that declares the crate those
outputs go into. Use it instead of a bare `rust_shared_library`: a cdylib is
the only thing that ships, but it is also the one crate type nothing can link,
so a bridge crate declared as a bare cdylib had unit tests and nothing else.
"""

load("@rules_dart//dart:providers.bzl", "DartCodeAssetInfo")
load("@rules_dart//dart:utils.bzl", "non_dart_transition")
load("@rules_rust//rust:defs.bzl", "rust_common", "rust_library", "rust_shared_library")
load(":dsym.bzl", "library_dsym")
load(":no_block_check.bzl", _frustrate_no_block_check = "frustrate_no_block_check")

FrustrateBridgeInfo = provider(
    doc = "What `frustrate_bridge` generated bindings for: the code-asset id they resolve the bridge library by, and what a bridge naming it in `models` reads.",
    fields = {
        "asset_id": "str: `package:<package_name>/<crate_name>`, the id the generated `@Native` declarations name.",
        "crate_name": "str: the bridge crate's name.",
        "dart_import": "str: import URI of the generated Dart bindings entry.",
        "data": "File or None: the data library, which `frustrate_bridge_data` places in its Dart package; None without `data_package_name`.",
        "data_import": "str: import URI of the data library, or empty without `data_package_name`.",
        "interface": "File: the interface JSON, which a bridge naming this one in `models` checks its shared types against.",
        "models_import": "str: import URI of the library through which a bridge naming this one in `models` reaches its plain data classes and their codecs.",
        "package_name": "str: the Dart package the bindings compile into.",
    },
)

def _frustrate_bridge_impl(ctx):
    if len(ctx.attr.srcs) != len(ctx.attr.module_paths):
        fail("srcs and module_paths must have the same length")

    # Declared at exactly src/frustrate_generated.rs (no target-name prefix):
    # rules_rust unifies generated and checked-in sources by path, so this
    # lines up with the crate's src/lib.rs `mod frustrate_generated;`. One
    # frustrate_bridge per package, by construction.
    rust_out = ctx.actions.declare_file("src/frustrate_generated.rs")

    # Likewise lib/<crate>.frustrate.dart: dart_library resolves package files
    # by package_root (= this package) prefix, so the bindings dart_library must
    # live in this same package and the file must sit directly under lib/.
    # The entry is a conditional export of the per-platform surfaces under
    # lib/src/ (web omits native-only members). Beside it, the models library
    # exports the plain data classes and their codecs to bridges that name
    # this one in `models`.
    stem = ctx.attr.crate_name + ".frustrate"
    dart_out = ctx.actions.declare_file("lib/" + stem + ".dart")
    dart_models_out = ctx.actions.declare_file("lib/" + stem + ".models.dart")
    dart_native_out = ctx.actions.declare_file("lib/src/" + stem + ".native.dart")
    dart_web_out = ctx.actions.declare_file("lib/src/" + stem + ".web.dart")
    dart_outs = [dart_out, dart_models_out, dart_native_out, dart_web_out]
    interface_out = ctx.actions.declare_file(ctx.label.name + "/interface.frustrate.json")

    # The `#[bridge(no_block)]` claim census: which claims want a check
    # artifact and which are already settled by placement. `frustrate_no_block_check`
    # reads it, and it is a separate output from the interface rather than a field
    # in it because the interface JSON is the wire-schema fingerprint — see
    # `emit_rust::claim_census`.
    claims_out = ctx.actions.declare_file(ctx.label.name + "/no_block.claims")

    args = ctx.actions.args()
    args.add("--crate-name", ctx.attr.crate_name)
    args.add("--dart-package", ctx.attr.package_name)
    for src, module_path in zip(ctx.files.srcs, ctx.attr.module_paths):
        args.add("--src", "{}:{}".format(src.path, module_path))
    models = [m[FrustrateBridgeInfo] for m in ctx.attr.models]
    for m in models:
        args.add("--model", "{}={}".format(m.interface.path, m.models_import))
    args.add("--rust-out", rust_out)
    args.add("--dart-out", dart_out)
    args.add("--interface-out", interface_out)
    args.add("--claims-out", claims_out)

    # The data library, outside `lib/`: it belongs to another Dart package,
    # whose BUILD file places it with `frustrate_bridge_data`.
    data_out = None
    data_import = ""
    if ctx.attr.data_package_name:
        data_out = ctx.actions.declare_file(ctx.label.name + "/" + stem + ".data.dart")
        data_import = "package:{}/{}.data.dart".format(ctx.attr.data_package_name, stem)
        args.add("--data-import", data_import)
        args.add("--data-out", data_out)

    ctx.actions.run(
        executable = ctx.executable._codegen,
        inputs = ctx.files.srcs + [m.interface for m in models],
        outputs = [rust_out, interface_out, claims_out] + dart_outs + ([data_out] if data_out else []),
        arguments = [args],
        mnemonic = "FrustrateCodegen",
        progress_message = "frustrate codegen for %{label}",
    )
    return [
        FrustrateBridgeInfo(
            asset_id = "package:{}/{}".format(ctx.attr.package_name, ctx.attr.crate_name),
            crate_name = ctx.attr.crate_name,
            dart_import = "package:{}/{}.dart".format(ctx.attr.package_name, stem),
            data = data_out,
            data_import = data_import,
            interface = interface_out,
            models_import = "package:{}/{}.models.dart".format(ctx.attr.package_name, stem),
            package_name = ctx.attr.package_name,
        ),
        DefaultInfo(files = depset([rust_out, interface_out, claims_out] + dart_outs + ([data_out] if data_out else []))),
        OutputGroupInfo(
            rust = depset([rust_out]),
            dart = depset(dart_outs),
            interface = depset([interface_out]),
            claims = depset([claims_out]),
        ),
    ]

# Every Dart build, for any web compiler and with or without creation-location
# tracking, reaches the one generated set: the codegen has no compiler to vary by (a per-compiler type is a typedef the Dart build picks, see
# `//runtime/dart:frustrate`), and the Rust glue it writes is the crate's source.
_frustrate_bridge = rule(
    implementation = _frustrate_bridge_impl,
    cfg = non_dart_transition,
    doc = "Generate frustrate bridge glue (Rust) and bindings (Dart) from " +
          "declared bridge sources.",
    attrs = {
        "srcs": attr.label_list(
            allow_files = [".rs"],
            mandatory = True,
            doc = "Bridge source files scanned for #[frustrate::bridge] items.",
        ),
        "module_paths": attr.string_list(
            mandatory = True,
            doc = "Rust module path of each src from the crate root, e.g. 'crate::api'.",
        ),
        "crate_name": attr.string(mandatory = True),
        "package_name": attr.string(
            mandatory = True,
            doc = "The Dart package the generated bindings compile into (the " +
                  "`package_name` of the `dart_library` holding them). It " +
                  "namespaces the bridge's code-asset id, which the Dart VM " +
                  "resolves only under the package that declares it.",
        ),
        "data_package_name": attr.string(
            doc = "The Dart package the bridge's data library compiles into, " +
                  "which gives the bridge one: its plain data classes (structs " +
                  "and enums reaching no handle, with no bridged members, " +
                  "naming only such types, and not taken through `models`) " +
                  "are declared there instead of in the bindings, which " +
                  "import and re-export them. The library imports only " +
                  "`package:frustrate` and the Dart homes of any " +
                  "bridge-external types it names, and holds no codec or " +
                  "fingerprint, so a change to the Rust that leaves the data " +
                  "declarations alone leaves it byte-identical. Place it with " +
                  "`frustrate_bridge_data` in the BUILD file of that package, " +
                  "beside a `dart_library` of that name, which the bindings' " +
                  "`dart_library` must depend on. Code that uses only the data " +
                  "classes depends on that library, and so not on the native " +
                  "library or the bindings.",
        ),
        "models": attr.label_list(
            providers = [FrustrateBridgeInfo],
            doc = "Other `frustrate_bridge` targets whose plain data types " +
                  "(structs and enums reaching no handle) this bridge uses. " +
                  "A struct or enum declared here under the name of one of " +
                  "theirs is theirs: list the source that declares it in " +
                  "`srcs` as usual, and the Dart bindings import its class " +
                  "and codec from that bridge's package instead of declaring " +
                  "their own, so one Dart object crosses both bridges. The " +
                  "bindings' `dart_library` must depend on theirs. Codegen " +
                  "refuses a type whose declaration differs between the two.",
        ),
        "_codegen": attr.label(
            default = Label("//codegen:frustrate_codegen_bin"),
            executable = True,
            cfg = "exec",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

# rustc compiles the crate to an object file inside its Rustc action, links the
# library from it and deletes it, so no later dsymutil run can read the crate's
# line tables. On macOS rustc writes the library's dSYM in that action, and the
# asset carries it.
_RUSTC_LINK_INPUTS_GONE = (
    "a Rust shared library is linked from an object file rustc makes and deletes " +
    "inside the action that writes it"
)

def _frustrate_code_asset_impl(ctx):
    libraries = [
        f
        for f in ctx.attr.library[DefaultInfo].files.to_list()
        if f.extension in ("so", "dylib", "dll")
    ]
    if len(libraries) != 1:
        fail("frustrate_code_asset %s: `library` %s must provide exactly one shared library, found %s" % (
            ctx.label,
            ctx.attr.library.label,
            [f.short_path for f in libraries] or "none",
        ))
    library = libraries[0]
    return [
        DefaultInfo(files = depset([library])),
        DartCodeAssetInfo(
            asset_id = ctx.attr.bridge[FrustrateBridgeInfo].asset_id,
            link_mode = "dynamic_loading_bundle",
            file = library,
            uri = "",
            link_inputs_gone = True,
            link_inputs_unset_reason = _RUSTC_LINK_INPUTS_GONE,
            dsym = library_dsym(ctx.attr.library, library),
        ),
    ]

def _frustrate_bridge_data_impl(ctx):
    info = ctx.attr.bridge[FrustrateBridgeInfo]
    if not info.data:
        fail("frustrate_bridge_data %s: %s has no data library; set its `data_package_name`" % (
            ctx.label,
            ctx.attr.bridge.label,
        ))
    out = ctx.actions.declare_file("lib/" + info.data.basename)
    ctx.actions.symlink(output = out, target_file = info.data)
    return [DefaultInfo(files = depset([out]))]

frustrate_bridge_data = rule(
    implementation = _frustrate_bridge_data_impl,
    cfg = non_dart_transition,
    doc = """A bridge's data library, placed under this package's `lib/`.

`bridge` is a `frustrate_bridge` with a `data_package_name`, and this BUILD
file is that Dart package's: put this target in the `srcs` of the package's
`dart_library`, which depends on `package:frustrate`
(`@frustrate//runtime/dart:frustrate`) and on the Dart packages of any
bridge-external types the classes name, and nothing of the bridge's. The
library is `lib/<crate>.frustrate.data.dart`.""",
    attrs = {
        "bridge": attr.label(
            mandatory = True,
            providers = [FrustrateBridgeInfo],
            doc = "The `frustrate_bridge` whose data library this is.",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

frustrate_code_asset = rule(
    implementation = _frustrate_code_asset_impl,
    cfg = non_dart_transition,
    doc = """A bridge library as the Dart code asset its generated bindings resolve.

The generated `<crate>Library` names its exports by the asset id of `bridge`;
this binds that id to `library`'s file. List it in the `code_assets` of the
`dart_library` that holds the bindings, and any `dart_test` or `dart_binary`
depending on that package loads `library` built for the host it runs on. An app
bundler (rules_flutter) ships it per platform.

`library` is any target providing the bridge's shared library: a
`frustrate_bridge_library`, `frustrate_manual_scheduler_library`,
`frustrate_hot_patchable`.""",
    attrs = {
        "bridge": attr.label(
            mandatory = True,
            providers = [FrustrateBridgeInfo],
            doc = "The `frustrate_bridge` that generated the bindings.",
        ),
        "library": attr.label(
            mandatory = True,
            doc = "The shared library built from `bridge`'s crate.",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
    provides = [DartCodeAssetInfo],
)

def frustrate_bridge(name, visibility = None, **kwargs):
    """Generate a bridge's Rust glue and Dart bindings, plus per-output filegroups.

    Declares `<name>`, `<name>.rs` (Rust glue), `<name>.dart` (Dart bindings)
    and `<name>.interface` (the interface JSON), all with the same visibility.

    `<name>` itself carries every output, so depending on it to reach one of
    them drags in the rest. These exist so a consumer can name exactly the one
    it needs.

    `.interface` rather than `.json`, which would name the encoding rather than the
    content and would collide with any second JSON output.

    Args:
        name: Name of the bridge target; the filegroups are `<name>.rs`,
            `<name>.dart` and `<name>.interface`.
        visibility: Visibility of every target declared.
        **kwargs: The bridge's attributes (`srcs`, `module_paths`,
            `crate_name`, `package_name`, `data_package_name`, `models`), as
            `_frustrate_bridge` documents them.
    """
    _frustrate_bridge(name = name, visibility = visibility, **kwargs)
    native.filegroup(
        name = name + ".rs",
        srcs = [":" + name],
        output_group = "rust",
        visibility = visibility,
    )
    native.filegroup(
        name = name + ".dart",
        srcs = [":" + name],
        output_group = "dart",
        visibility = visibility,
    )

    # The wire schema as a dependable label. `--output_groups=interface` already
    # reaches this file, but only from the command line; a rule that wants it
    # as an input needs a label, and depending on `<name>` would pull in the
    # Rust glue and both Dart surfaces to get it.
    #
    # What makes it worth depending on is that it is the *input* the schema
    # fingerprint is computed over (`hash::schema_hash`), not a report about
    # the bindings: two builds whose interface bytes match generate identical
    # bindings, so byte equality is a sound answer to "can code built against
    # the old one still call this?" without parsing the file at all.
    native.filegroup(
        name = name + ".interface",
        srcs = [":" + name],
        output_group = "interface",
        visibility = visibility,
    )

def frustrate_bridge_library(name, crate_name = None, shared_library = True, visibility = None, **kwargs):
    """The bridge crate, declared twice: as the shipped cdylib and as a linkable rlib.

    Use this instead of a bare `rust_shared_library` for a bridge crate. It
    declares two targets over **the same sources and the same attributes**:

    * `<name>` — `rust_shared_library` (crate-type `cdylib`), the target
      consumers name: `frustrate_wasm_module(library = ":<name>")`,
      `frustrate_code_asset(library = ":<name>")`, which binds the shared
      library to the bindings' code-asset id; the app bundler ships it in each
      platform's form. Absent under `shared_library = False`.
    * `<name>_lib` — `rust_library` (crate-type `rlib`). Same crate, same
      crate *name*, in the one form another Rust crate can link.

    **Why the second target exists.** rustc cannot `extern crate` a cdylib,
    and `rust_test(crate = ":<name>")` recompiles the crate's own sources
    under `--test` and forbids `srcs` (rules_rust `rust_test.crate` and
    `rust_test.srcs` are mutually exclusive), so against the cdylib only unit
    tests exist. With the rlib, both ordinary forms work:

        rust_test(name = "unit", crate = ":<name>_lib")             # unit
        rust_test(                                                  # integration
            name = "integration",
            srcs = ["tests/two_endpoints.rs"],
            deps = [":<name>_lib"],
        )

    and the integration test is an *ordinary* one — it sees only the crate's
    public API, plus whatever the crate exports through the C ABI, which is
    exactly the surface Dart sees.

    **cargo still never applies.** This closes the Bazel half only, and the
    other half is not a defect to be fixed: `cargo check` in a bridge
    directory fails with `file not found for module frustrate_generated` and
    `unresolved import frustrate`, and both are by design — `frustrate`
    arrives as a Bazel label so the app's lockfile stays free of path
    dependencies, and `frustrate_generated.rs` is a build action's output that
    does not exist until Bazel runs the codegen. A bridge crate's `Cargo.toml`
    exists for `crate.from_cargo`'s dependency resolution, not for cargo
    builds. `cargo test`, `cargo clippy` and rust-analyzer's default project
    model do not work on a bridge crate and are not meant to.

    **Why the two targets share one crate name.** Both crates are named
    `crate_name` (default: `name`), so a test writes `use my_bridge::…` rather
    than some derived spelling. That is only safe because the two are **never
    linked together** — nothing can depend on a cdylib as a Rust crate, so the
    rlib and the cdylib never meet. A thin cdylib over the rlib would need a
    crate name of its own (E0519); that is what `frustrate_shared_library` builds when
    several bridges share one library, and the rlib's `#[no_mangle]` exports
    reach it because rustc forces in every symbol an upstream crate exports.
    The single-bridge cdylib stays a direct build of the crate, so its bytes
    do not depend on that route. The price is that the crate's own sources
    compile twice in the native configuration (once per crate type).
    Dependencies are shared, and a bridge crate is thin by design.

    **Attributes go to both targets, and that is the invariant.** `srcs`,
    `deps`, `edition`, `crate_features`, `rustc_flags`, `proc_macro_deps`,
    `compile_data`, `aliases`, `tags` — everything in `**kwargs` is forwarded
    unchanged to each. Two compilations of one crate are only interchangeable
    while nothing distinguishes them, so pass nothing that only one rule
    accepts (`platform` on the shared library, `disable_pipelining` on the
    library); split them by hand if you ever need to.

    Args:
        name: the cdylib target, and the name of the shipped artifact.
        crate_name: the Rust crate name for both targets. Defaults to `name`.
            This is what a test's `use` line spells, and what the native
            artifact is named after.
        shared_library: False for a bridge that only ever ships inside a
            `frustrate_shared_library`: `<name>` is then not declared, and only
            `<name>_lib`, the key of its `frustrate_shared_library` entry, is built.
        visibility: applied to both targets.
        **kwargs: forwarded verbatim to both targets.
    """
    crate_name = crate_name or name

    rust_library(
        name = name + "_lib",
        crate_name = crate_name,
        visibility = visibility,
        **kwargs
    )
    if shared_library:
        rust_shared_library(
            name = name,
            crate_name = crate_name,
            visibility = visibility,
            **kwargs
        )

def _frustrate_shared_library_shim_impl(ctx):
    names = []
    for crate, bridge in zip(ctx.attr.crates, ctx.attr.bridges):
        name = crate[rust_common.crate_info].name
        expected = bridge[FrustrateBridgeInfo].crate_name
        if name != expected:
            fail("frustrate_shared_library %s: %s generated bindings for crate `%s`, but the library it is paired with, %s, is crate `%s`" % (
                ctx.label,
                bridge.label,
                expected,
                crate.label,
                name,
            ))
        names.append(name)
    out = ctx.actions.declare_file(ctx.label.name + "/lib.rs")
    ctx.actions.write(out, "".join(
        ["//! @generated by frustrate_shared_library. Links each bridge crate into one library.\n"] +
        ["extern crate %s;\n" % n for n in names],
    ))
    return [DefaultInfo(files = depset([out]))]

_frustrate_shared_library_shim = rule(
    implementation = _frustrate_shared_library_shim_impl,
    doc = "The crate root of a `frustrate_shared_library`: an `extern crate` per bridge rlib.",
    attrs = {
        "crates": attr.label_list(
            mandatory = True,
            providers = [rust_common.crate_info],
        ),
        # The frustrate_bridge each crate's glue was generated by, in the same order.
        "bridges": attr.label_list(
            mandatory = True,
            providers = [FrustrateBridgeInfo],
        ),
    },
)

def frustrate_shared_library(name, bridges, crate_name = None, visibility = None, **kwargs):
    """Several bridges in one shared library, served by one Rust runtime.

    An isolate (a page, on web) runs one Rust runtime: one std, one
    allocator, one worker pool, one memory. Bridges that one app uses together
    are linked into one library here, and each stays its own crate, its own
    Dart bindings, its own schema and its own code-asset id. Every export a
    bridge's glue defines carries its crate name, so the bridges do not collide;
    the runtime's own exports exist once.

    Declares:

    * `<name>` — `rust_shared_library` whose crate root is a generated
      `extern crate` for each bridge's Rust library. Each one's exports reach
      the library without anything referencing them: rustc forces in every
      symbol an upstream crate exports. Name it in `frustrate_wasm_module(library = …)` for
      the web.
    * `<name>.<library>` — per bridge, where `<library>` is its Rust library's
      target name, a `frustrate_code_asset` binding that bridge's code-asset
      id to `<name>`. List them in the `code_assets` of the
      `dart_test` or `dart_binary` that uses the bridges together: an explicit
      code asset replaces the one a bindings package declares under its id.

    In Dart, `FrustrateNative.init` each bridge's generated library, and on
    web `FrustrateWeb.init` each bridge's `<crate>Bridge` with the one
    module; every init after the first attaches to the transport the first
    built.

    Args:
        name: the shared library target.
        bridges: dict from each bridge's Rust library (`rust_library`: the
            `<name>_lib` of a `frustrate_bridge_library`) to the
            `frustrate_bridge` that generated its glue. Analysis fails when
            a library is not the crate its bridge was generated for.
        crate_name: the combined crate's name. Defaults to `name`; it must
            differ from every bridge crate's name (E0519).
        visibility: applied to every target declared.
        **kwargs: forwarded to the `rust_shared_library`; `tags` and
            `testonly` also to every other target declared.
    """
    common = {k: kwargs[k] for k in ("tags", "testonly") if k in kwargs}
    libraries = [native.package_relative_label(library) for library in bridges]
    _frustrate_shared_library_shim(
        name = name + "_root",
        crates = libraries,
        bridges = list(bridges.values()),
        visibility = ["//visibility:private"],
        **common
    )
    rust_shared_library(
        name = name,
        crate_name = crate_name or name.replace("-", "_"),
        crate_root = name + "_root",
        srcs = [name + "_root"],
        deps = libraries + kwargs.pop("deps", []),
        edition = kwargs.pop("edition", "2021"),
        visibility = visibility,
        **kwargs
    )
    for library, bridge in zip(libraries, bridges.values()):
        frustrate_code_asset(
            name = "%s.%s" % (name, library.name),
            bridge = bridge,
            library = name,
            visibility = visibility,
            **common
        )

def _wasm_platform_transition_impl(_settings, attr):
    # The whole transition. Everything a target platform needs beyond its
    # constraints — the rules_rust channel for the threaded build, a C
    # toolchain selection, anything else — rides on that platform's own
    # `flags` attribute (Bazel 8+), NOT on a table in here. That is what
    # keeps the vocabulary open: a consumer writes their own platform() and
    # this rule needs no change.
    #
    # Verified, because it is the subtle part: a platform-declared flag
    # reaches the EXEC configuration too, where the nightly host toolchain
    # builds the proc-macros nightly rustc must load (a stable-built
    # proc-macro dylib cannot be loaded by nightly rustc). The two exec
    # configs for //macros:frustrate_macros under //bazel:wasm32 vs
    # //bazel:wasm32_threads differ in exactly one line —
    # `rules_rust+//rust/toolchain/channel:channel: nightly`.
    return {"//command_line_option:platforms": [str(attr.platform)]}

_wasm_platform_transition = transition(
    implementation = _wasm_platform_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

# For rules that analyze a crate under the platform `frustrate_wasm_module`
# would build it for (bazel/clippy.bzl). The rule has a `platform` attribute.
wasm_platform_transition = _wasm_platform_transition

def _run_wasm_opt(ctx, module, out):
    """Run the binaryen `wasm-opt` pass over `module`, producing `out`.

    Shared by `frustrate_wasm_module`'s `wasm_opt` attribute and by the
    standalone `frustrate_wasm_opt` rule, so both spell the action the same
    way and a change to the contract lands in one place.

    **No `--enable-*` feature flags are passed, and that is deliberate rather
    than an omission.** rustc writes a `target_features` custom section into
    every module it emits, and binaryen reads it — so a `+atomics` module from
    //bazel:wasm32_threads is optimized with threads enabled without this rule
    knowing which platform produced it, and the open-vocabulary property the
    platform transition works to preserve is not quietly re-closed here by a
    table of triples. A module *without* that section is the case to know
    about: dart2wasm output carries none, and wasm-opt rejects it outright
    (`array.new requires gc [--enable-gc]`) rather than miscompiling it. This
    rule only ever sees rustc's output, so that failure mode is out of reach —
    but it is why `wasm_opt_args` exists at all.

    The bridge ABI survives the pass: the same exports and the same imports,
    name for name, as the module rustc emitted. The import half of that is not
    a claim in a comment — //bazel/wasm_import_check runs over the optimized
    module, so a pass that disturbed the import surface would fail the build.
    What the pass does drop is the `name` section, which is why
    //bazel/wasm_std_check must keep reading rustc's own output — it reads
    mangled Rust symbols out of exactly that section, and the comment at its
    call site says so for its own reason.
    """
    args = ctx.actions.args()
    args.add(ctx.attr.opt_level)
    args.add_all(ctx.attr.wasm_opt_args)
    args.add(module)
    args.add("-o", out)
    ctx.actions.run(
        executable = ctx.executable.wasm_opt,
        inputs = [module],
        outputs = [out],
        arguments = [args],
        mnemonic = "FrustrateWasmOpt",
        progress_message = "wasm-opt %s for %%{label}" % ctx.attr.opt_level,
    )
    return out

def _frustrate_wasm_module_impl(ctx):
    files = ctx.attr.library[0][DefaultInfo].files.to_list()
    wasm = [f for f in files if f.extension == "wasm"]
    if len(wasm) != 1:
        fail("frustrate_wasm_module: expected exactly one .wasm output from %s, got [%s]" % (
            ctx.attr.library[0].label,
            ", ".join([f.basename for f in files]),
        ))
    module = wasm[0]
    bindgen_js = []
    validations = []

    if ctx.attr.std_check == "off" and ctx.attr.std_check_allow:
        # A waiver under a check that is not running does nothing, and reads as
        # a narrower exemption than the target actually has — the author who
        # wrote `std_check_allow = ["…SystemTime::now"]` believes println! is
        # still guarded here, and it is not. Same treatment the tool gives a
        # waiver naming an uncovered facility, for the same reason.
        fail("frustrate_wasm_module: %s sets std_check = \"off\", which waives " % ctx.label +
             "every facility, and also lists std_check_allow = %s. " % ctx.attr.std_check_allow +
             "The list does nothing. Drop `std_check = \"off\"` to waive only " +
             "what the list names, or drop the list.")

    if ctx.attr.std_check == "error":
        # The std-facility tripwire (//bazel/wasm_std_check). Run over rustc's
        # own output rather than the post-pass result below, because it reads
        # mangled Rust symbols out of the name section and wasm-bindgen
        # rewrites them: a processed module carries no mangled symbol.
        #
        # A *validation* action rather than an input to the module: it produces
        # nothing anyone consumes, and validations run for every target that
        # builds this one without the rule having to thread a marker through
        # DefaultInfo. `--run_validations` defaults to true, so this needs no
        # .bazelrc entry — which is the point, since diagnostic flags do not
        # live there.
        marker = ctx.actions.declare_file(ctx.label.name + ".std_check")
        ctx.actions.run(
            executable = ctx.executable._std_check,
            inputs = [wasm[0]],
            outputs = [marker],
            arguments = [wasm[0].path, str(ctx.label), marker.path] +
                        ctx.attr.std_check_allow,
            mnemonic = "FrustrateWasmStdCheck",
            progress_message = "checking std facilities in %{label}",
        )
        validations = [marker]

    if ctx.attr.bindgen:
        # The wasm-bindgen post-pass. Required for any bridge crate whose
        # dependency graph contains wasm-bindgen — which is every crate that
        # reaches a browser API from Rust, and is a *structural* dependency
        # (`[target.'cfg(wasm)'.dependencies]`), not a feature anything can
        # turn off. Without it rustc's module carries imports in a
        # `__wbindgen_placeholder__` namespace that nothing can satisfy and
        # `WebAssembly.instantiate` rejects outright — which the `else` branch
        # below turns into a build failure rather than a browser one.
        #
        # What the pass does NOT do is as important as what it does: it leaves
        # frustrate's own `extern "C"` exports and its `frustrate` import
        # namespace untouched, so the bridge ABI is unaffected. It rewrites the
        # placeholder imports into one real namespace and emits the JS module
        # that satisfies it, which the runtime loads by URL
        # (`FrustrateWeb.init(bindgenGlueUrl:)`).
        #
        # `--target bundler` rather than `--target web`: bundler emits plain
        # `export function __wbg_*` declarations, which is exactly the shape a
        # namespace import needs. `web` additionally *exports* the module's
        # memory and generates its own instantiation entry point, neither of
        # which frustrate can use — the runtime owns instantiation.
        # Declared directly in the consuming package, NOT in a subdirectory,
        # and that is required rather than tidy: `flutter_web_app` stages
        # assets by `short_path` with the package prefix stripped, so a file
        # under `<name>.bindgen/` stages into a subdirectory of the bundle and
        # the app's `bindgenGlueUrl` would have to name it. Same constraint
        # `frustrate_web_glue` documents, same answer.
        #
        # The stem drops a trailing `.wasm` from the target name, so the
        # conventional `frustrate_wasm_module(name = "foo.wasm")` yields
        # `foo_bg.js` — which is also the module name wasm-bindgen writes into
        # the wasm's import section, so the two agree by construction.
        stem = ctx.label.name
        if stem.endswith(".wasm"):
            stem = stem[:-len(".wasm")]
        processed = ctx.actions.declare_file(stem + "_bg.wasm")
        js = ctx.actions.declare_file(stem + "_bg.js")
        entry = ctx.actions.declare_file(stem + ".js")

        args = ctx.actions.args()
        args.add("--target", "bundler")
        args.add("--no-typescript")
        args.add("--out-dir", processed.dirname)
        args.add("--out-name", stem)
        args.add(module)

        ctx.actions.run(
            executable = ctx.executable.bindgen,
            inputs = [module],
            outputs = [processed, js, entry],
            arguments = [args],
            mnemonic = "FrustrateWasmBindgen",
            progress_message = "wasm-bindgen post-pass for %{label}",
        )
        module = processed
        bindgen_js = [js]

        # The namespace the post-pass writes into the module's imports.
        bindgen_namespace = "./" + js.basename
    else:
        bindgen_namespace = None

    # wasm-opt, when the app named a binary — AFTER the wasm-bindgen post-pass.
    # That order is wasm-pack's and it is the only one that can work: the CLI
    # rewrites the module wholesale, so optimizing first would optimize bytes
    # that are then thrown away.
    #
    # Opt-in by naming the binary, exactly like `bindgen` above, and for a
    # weaker reason than that one has. `bindgen`'s version must equal the
    # wasm-bindgen crate version in the app's lockfile, so frustrate *cannot*
    # supply it; wasm-opt has no such coupling and frustrate could ship a
    # default. It does not, because a binaryen release is a large download and
    # a module that pulls one into every consumer's graph — including the ones
    # that never build for web — has decided something for them.
    # //bazel/wasm_opt has the one-line way to get it.
    #
    # What the pass buys is size, and how much depends on the graph — the more
    # third-party Rust in it, the more there is to strip.
    # //tests/bazel_rules:wasm_opt_shrinks_the_module prints the before/after
    # pair for its fixture; no figure is copied into this tree, because a
    # copied figure drifts from the build that produced it. What the pass does
    # NOT buy is speed: rustc's own LLVM pass has already done that work.
    if ctx.executable.wasm_opt:
        module = _run_wasm_opt(
            ctx,
            module,
            ctx.actions.declare_file(ctx.label.name + ".opt.wasm"),
        )

    # The import contract, over the bytes that actually ship — which is why
    # this sits after wasm-opt rather than inside either branch above. A check
    # that reads a module the app does not serve is a check of something else.
    #
    # One rule: every namespace the module imports is one the runtime puts in
    # the import object, one `host_imports` declares, or the one the post-pass
    # wrote, which its generated JS supplies. Without it, a graph that needed
    # `bindgen` and did not get it builds green and dies at
    # `WebAssembly.instantiate`, in a browser, with unsatisfiable imports.
    #
    # A post-passed module is also checked for the shape the CLI's thread
    # transform produced — it fires whenever the memory is shared and cannot be
    # switched off, rewrites the memory import, deletes the thread bootstrap
    # symbols, and hands per-thread setup to
    # `__wbindgen_start`. frustrate drives that shape, and the CLI version is
    # the *app's* pin rather than frustrate's, so those facts are checked here
    # rather than discovered in a browser. A no-op on a single-threaded module.
    #
    # A validation action for the same reasons the std check is one, and in the
    # same output group.
    check_marker = ctx.actions.declare_file(ctx.label.name + ".import_check")
    check_args = []
    for ns in ctx.attr.host_imports:
        check_args += ["--host", ns]
    if bindgen_namespace:
        check_args += ["--bindgen", bindgen_namespace]
    ctx.actions.run(
        executable = ctx.executable._import_check,
        inputs = [module],
        outputs = [check_marker],
        arguments = check_args + [module.path, str(ctx.label), check_marker.path],
        mnemonic = "FrustrateWasmImportCheck",
        progress_message = "checking the imports of %{label}",
    )
    validations.append(check_marker)

    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = out, target_file = module)
    return [
        DefaultInfo(files = depset([out])),
        # The generated JS, for the app to stage beside the module and name in
        # `bindgenGlueUrl`. An output group rather than a second DefaultInfo
        # file because `flutter_web_app(extra_web_assets)` stages every file it
        # is given, and the two assets need different names in the bundle.
        OutputGroupInfo(
            bindgen_js = depset(bindgen_js),
            _validation = depset(validations),
        ),
    ]

def _frustrate_web_glue_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = out, target_file = ctx.file._glue)
    return [DefaultInfo(files = depset([out]))]

frustrate_web_glue = rule(
    implementation = _frustrate_web_glue_impl,
    cfg = non_dart_transition,
    doc = "Stage frustrate's web glue (the served asset for strict-CSP " +
          "pages) into the consuming package — " +
          "name the target `frustrate.js` (the URL the app's " +
          "`<script src>` tag loads; declared in the app's own package so " +
          "asset staging lands it at the bundle root) and list it in " +
          "flutter_web_app's extra_web_assets next to the wasm module.",
    attrs = {
        "_glue": attr.label(
            default = Label("//runtime/dart:lib/src/js/frustrate.js"),
            allow_single_file = True,
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

frustrate_wasm_module = rule(
    implementation = _frustrate_wasm_module_impl,
    cfg = non_dart_transition,
    doc = "Build a bridge rust_shared_library for a wasm platform and " +
          "expose the module under this target's name — name the target " +
          "`<something>.wasm`, conventionally matching the URL the app's " +
          "web init fetches.\n\n" +
          "This rule is the only thing that pins a platform. The bridge " +
          "crate itself stays platform-agnostic and must remain buildable " +
          "under any --platforms — that invariant is what lets one crate " +
          "target serve the native tests, the wasm module and the threaded " +
          "wasm module at once.",
    attrs = {
        "library": attr.label(
            mandatory = True,
            cfg = _wasm_platform_transition,
            doc = "The bridge rust_shared_library target.",
        ),
        "platform": attr.label(
            default = Label("//bazel:wasm32"),
            doc = "The target platform to build the crate for. **Which one " +
                  "you want is a property of your Rust dependencies, not a " +
                  "preference.** frustrate ships three:\n\n" +
                  "  * //bazel:wasm32 — stable Rust, single-threaded, the " +
                  "charter's default web execution model. Add the " +
                  "`bindgen` attribute to this one when the crate graph " +
                  "reaches a browser API from Rust.\n" +
                  "  * //bazel:wasm32_wasip1 — triple wasm32-wasip1, stable " +
                  "Rust, one build step. The branch for a graph that needs " +
                  "std facilities wasm32-unknown-unknown has no host for: " +
                  "`SystemTime::now`, `Instant::now`, `getrandom`, " +
                  "`println!`. frustrate's JS runtime supplies the wasi " +
                  "host. Mutually exclusive with web-sys/js-sys, which do " +
                  "not compile there.\n" +
                  "  * //bazel:wasm32_threads — nightly Rust, a locally " +
                  "built +atomics std, real pool worker threads on one " +
                  "shared memory. Run " +
                  "`bazel run //toolchain/custom_std:build` once first, and " +
                  "tag threaded targets `manual` so wildcard builds stay " +
                  "green without the local artifacts.\n\n" +
                  "Any platform() works, including one you declare " +
                  "yourself for extra constraints or a custom C toolchain. " +
                  "Build settings the platform needs beyond its " +
                  "constraints belong on its own `flags` attribute. One " +
                  "trap worth knowing: if the platform's triple is missing from " +
                  "crate.from_cargo's supported_platform_triples, deps " +
                  "resolve to //:incompatible silently, and this rule's " +
                  "'expected exactly one .wasm' error is the first symptom.",
        ),
        "bindgen": attr.label(
            executable = True,
            cfg = "exec",
            doc = "The `wasm-bindgen` CLI, when the bridge crate's graph " +
                  "contains the wasm-bindgen crate (any crate reaching a " +
                  "browser API from Rust). Set it and this rule runs the " +
                  "post-pass, exposing the generated JS in the " +
                  "`bindgen_js` output group for the app to serve and name " +
                  "in `FrustrateWeb.init(bindgenGlueUrl:)`. **The CLI's " +
                  "version must equal the wasm-bindgen crate version in the " +
                  "bridge's lockfile exactly** — a mismatch is a hard error " +
                  "naming both versions. frustrate does not supply the " +
                  "binary: the version is a property of the app's crate " +
                  "graph, so the app declares it (e.g. a crate_universe hub " +
                  "with `gen_binaries = [\"wasm-bindgen\"]`).\n\n" +
                  "Omitting it on a graph that needs it is a build failure, " +
                  "not a browser one: without the post-pass the module keeps " +
                  "imports nothing can satisfy, and //bazel/wasm_import_check " +
                  "names them.",
        ),
        "host_imports": attr.string_list(
            doc = "Import namespaces the app's own JavaScript supplies — how " +
                  "a module reaches JS without wasm-bindgen. Rust declares " +
                  "the functions in a " +
                  "`#[link(wasm_import_module = \"my_ns\")] extern \"C\" { … }` " +
                  "block; the app serves an ES module whose exports are " +
                  "those functions and names its URL in " +
                  "`FrustrateWeb.init(hostImports: {'my_ns': 'my_ns.js'})`. " +
                  "That module may also export " +
                  "`attach({exports, memory, actor})`, which the runtime " +
                  "awaits on every instance before any of the bridge's code " +
                  "runs; `actor` is `{bridge, type}` for the actor the " +
                  "instance serves (its bridge's crate name and its Rust " +
                  "type name), null on the page and in pool workers. " +
                  "Every realm that instantiates the module (the page, each " +
                  "actor's Worker, each pool worker) imports its own copy of " +
                  "the JS module, so its state is per realm.\n\n" +
                  "The build fails on an import namespace that is not the " +
                  "runtime's own, listed here, or the one the `bindgen` " +
                  "post-pass writes, so list exactly the namespaces the app " +
                  "supplies.",
        ),
        "wasm_opt": attr.label(
            executable = True,
            cfg = "exec",
            doc = "The binaryen `wasm-opt` binary. Naming it runs the " +
                  "optimizer over the module that ships — after the " +
                  "wasm-bindgen post-pass, when there is one.\n\n" +
                  "**This is a size pass, not a speed one.** How much size " +
                  "depends on the graph; " +
                  "//tests/bazel_rules:wasm_opt_shrinks_the_module prints the " +
                  "before/after pair for its fixture, and no figure is copied " +
                  "into this tree because a copied figure drifts from the " +
                  "build that produced it. Speed it does not buy: rustc's own " +
                  "LLVM pass has already done that work. Reach for it to cut " +
                  "download and instantiate cost, not throughput.\n\n" +
                  "frustrate does not supply the binary, for a weaker reason " +
                  "than the one that applies to `bindgen`: nothing couples " +
                  "wasm-opt's version to your crate graph, so a default " +
                  "*could* ship — but a binaryen release is a large download, " +
                  "and pulling one into every consumer's " +
                  "graph, web-targeting or not, decides something that is not " +
                  "this module's to decide. `//bazel/wasm_opt:extensions.bzl` " +
                  "ships an optional, lazy fetch — two lines in MODULE.bazel " +
                  "and `wasm_opt = \"@binaryen//:wasm-opt\"` here.\n\n" +
                  "The bridge ABI survives the pass: checked at `-O` and " +
                  "`-Oz` against the module rustc emitted, same exports and " +
                  "same imports, name for name.",
        ),
        "opt_level": attr.string(
            default = "-O",
            values = ["-O", "-O1", "-O2", "-O3", "-O4", "-Os", "-Oz"],
            doc = "The wasm-opt level, when `wasm_opt` is named. Ignored " +
                  "otherwise.\n\n" +
                  "`-O` is the default because a bigger number was not " +
                  "better on the modules tried here: on Rust output LLVM has " +
                  "already optimized, `-O2`, `-O3` and `-O4` each spent " +
                  "longer and produced a *larger* module than plain `-O`, and " +
                  "only `-Oz` came out smaller. Measure your own module rather " +
                  "than taking that ordering on trust — it is a property of " +
                  "the input, not of binaryen.",
        ),
        "wasm_opt_args": attr.string_list(
            doc = "Extra flags for wasm-opt, appended after `opt_level`.\n\n" +
                  "Feature flags are **not** normally needed: rustc writes a " +
                  "`target_features` custom section and binaryen reads it, so " +
                  "a `+atomics` module from //bazel:wasm32_threads is " +
                  "optimized with threads enabled without this rule knowing " +
                  "which platform produced it. This is the escape hatch for " +
                  "the cases that fall outside that — a `--enable-*` binaryen " +
                  "does not infer, or a pass you want by name.",
        ),
        "std_check": attr.string(
            default = "error",
            values = ["error", "off"],
            doc = "Whether to fail the build when the module reaches a std " +
                  "facility its platform cannot serve.\n\n" +
                  "`wasm32-unknown-unknown` has no OS behind it, so std links " +
                  "`sys/pal/unsupported/*` for anything that needs one: " +
                  "`SystemTime::now()` compiles and then traps at run time, " +
                  "and `println!` accepts the write and discards it — no " +
                  "output, no trap, no error. Both work on " +
                  "//bazel:wasm32_wasip1, and the build error names it.\n\n" +
                  "The check reads the module's own import section rather " +
                  "than comparing platform labels, so a platform() you " +
                  "declare yourself is judged by what its std actually " +
                  "serves. Set `off` for a module where the reaching code is " +
                  "genuinely never executed, and say why in a comment.",
        ),
        "std_check_allow": attr.string_list(
            doc = "Facilities to waive for this module, by the name the check " +
                  "prints. Use it when the call is in a dependency, on a path " +
                  "this module never takes: the waived facility stops failing " +
                  "the build and every other one keeps biting, which " +
                  "`std_check = \"off\"` does not. Naming a facility the check " +
                  "does not cover is an error, not a no-op, and so is " +
                  "combining it with `std_check = \"off\"`. Say in a comment " +
                  "which call site you looked at — a waiver is a claim about " +
                  "reachability, and nothing else records it.",
        ),
        "_std_check": attr.label(
            default = Label("//bazel/wasm_std_check"),
            executable = True,
            cfg = "exec",
        ),
        "_import_check": attr.label(
            default = Label("//bazel/wasm_import_check"),
            executable = True,
            cfg = "exec",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)

def _frustrate_wasm_opt_impl(ctx):
    return [DefaultInfo(files = depset([_run_wasm_opt(
        ctx,
        ctx.file.module,
        ctx.actions.declare_file(ctx.label.name),
    )]))]

frustrate_wasm_opt = rule(
    implementation = _frustrate_wasm_opt_impl,
    cfg = non_dart_transition,
    doc = "Run binaryen's `wasm-opt` over an existing wasm module.\n\n" +
          "`frustrate_wasm_module(wasm_opt = ...)` is the integrated spelling " +
          "and the one to reach for first — it optimizes in the right place " +
          "in the pipeline (after the wasm-bindgen post-pass) and keeps the " +
          "import contract checked over the optimized bytes. This rule is for " +
          "the cases that sit outside it: a module from somewhere else, a " +
          "second artifact at a different level, or a size experiment you " +
          "want as its own target.\n\n" +
          "It takes any wasm file, so it will also accept one this rule set " +
          "did not produce. One shape is worth knowing before you try: " +
          "**dart2wasm output does not survive it.** Those modules are WasmGC " +
          "and carry no `target_features` section, so wasm-opt cannot infer " +
          "the features and rejects them outright (`array.new requires gc " +
          "[--enable-gc]`). It fails the build rather than miscompiling, " +
          "which is the right outcome, but the flags are yours to supply via " +
          "`wasm_opt_args` if that is genuinely what you want.\n\n" +
          "See the `wasm_opt`, `opt_level` and `wasm_opt_args` attributes on " +
          "frustrate_wasm_module for what the pass costs and buys — the same " +
          "action runs here.",
    attrs = {
        "module": attr.label(
            mandatory = True,
            allow_single_file = [".wasm"],
            doc = "The wasm module to optimize.",
        ),
        "wasm_opt": attr.label(
            mandatory = True,
            executable = True,
            cfg = "exec",
            doc = "The binaryen `wasm-opt` binary. Mandatory here, where the " +
                  "whole rule is the pass — unlike on frustrate_wasm_module, " +
                  "where naming it is what opts in.",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
        "opt_level": attr.string(
            default = "-O",
            values = ["-O", "-O1", "-O2", "-O3", "-O4", "-Os", "-Oz"],
            doc = "The wasm-opt level. See frustrate_wasm_module.opt_level " +
                  "for why the default is `-O` and not a higher number.",
        ),
        "wasm_opt_args": attr.string_list(
            doc = "Extra flags, appended after `opt_level`.",
        ),
    },
)

frustrate_no_block_check = _frustrate_no_block_check
