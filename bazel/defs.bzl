"""frustrate bridge rules.

Tier 2: `frustrate_bridge` runs the codegen as a declared action over
explicitly declared bridge sources, producing the Rust glue module and the
Dart bindings as separate output groups.

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

load("@rules_rust//rust:defs.bzl", "rust_common", "rust_library", "rust_shared_library")

def _frustrate_bridge_impl(ctx):
    if len(ctx.attr.srcs) != len(ctx.attr.module_paths):
        fail("srcs and module_paths must have the same length")
    # Declared at exactly src/frustrate_generated.rs (no target-name prefix):
    # rules_rust unifies generated and checked-in sources by path, so this
    # lines up with the crate's src/lib.rs `mod frustrate_generated;`. One
    # frustrate_bridge per package, by construction.
    rust_out = ctx.actions.declare_file("src/frustrate_generated.rs")
    # Likewise lib/<crate>.frustrate.dart: dart_library resolves package files
    # by lib_root (= this package) prefix, so the bindings dart_library must
    # live in this same package and the file must sit directly under lib/.
    # The entry is a conditional export of the per-platform surfaces under
    # lib/src/ (web omits native-only members).
    stem = ctx.attr.crate_name + ".frustrate"
    dart_out = ctx.actions.declare_file("lib/" + stem + ".dart")
    dart_native_out = ctx.actions.declare_file("lib/src/" + stem + ".native.dart")
    dart_web_out = ctx.actions.declare_file("lib/src/" + stem + ".web.dart")
    dart_outs = [dart_out, dart_native_out, dart_web_out]
    ir_out = ctx.actions.declare_file(ctx.label.name + "/interface.frustrate.json")

    # The `#[bridge(no_block)]` claim census: which claims want a check
    # artifact and which are already settled by placement. `frustrate_block_check`
    # reads it, and it is a separate output from the IR rather than a field in
    # it because the IR JSON is the wire-schema fingerprint — see
    # `emit_rust::claim_census`.
    claims_out = ctx.actions.declare_file(ctx.label.name + "/no_block.claims")

    args = ctx.actions.args()
    args.add("--crate-name", ctx.attr.crate_name)
    for src, module_path in zip(ctx.files.srcs, ctx.attr.module_paths):
        args.add("--src", "{}:{}".format(src.path, module_path))
    args.add("--rust-out", rust_out)
    args.add("--dart-out", dart_out)
    args.add("--ir-out", ir_out)
    args.add("--claims-out", claims_out)

    ctx.actions.run(
        executable = ctx.executable._codegen,
        inputs = ctx.files.srcs,
        outputs = [rust_out, ir_out, claims_out] + dart_outs,
        arguments = [args],
        mnemonic = "FrustrateCodegen",
        progress_message = "frustrate codegen for %{label}",
    )
    return [
        DefaultInfo(files = depset([rust_out, ir_out, claims_out] + dart_outs)),
        OutputGroupInfo(
            rust = depset([rust_out]),
            dart = depset(dart_outs),
            ir = depset([ir_out]),
            claims = depset([claims_out]),
        ),
    ]

frustrate_bridge = rule(
    implementation = _frustrate_bridge_impl,
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
        "_codegen": attr.label(
            default = Label("//codegen:frustrate_codegen_bin"),
            executable = True,
            cfg = "exec",
        ),
    },
)

def frustrate_bridge_outputs(name, visibility = None, **kwargs):
    """Tier 1 convenience: bridge target plus per-language filegroups.

    Declares `<name>`, `<name>.rs` (Rust glue), `<name>.dart` (Dart bindings)
    and `<name>.ir` (the interface JSON), all with the same visibility.

    `<name>` itself carries every output, so depending on it to reach one of
    them drags in the rest. These exist so a consumer can name exactly the one
    it needs.

    `.ir` rather than `.json`, which would name the encoding rather than the
    content and would collide with any second JSON output.
    """
    frustrate_bridge(name = name, visibility = visibility, **kwargs)
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

    # The wire schema as a dependable label. `--output_groups=ir` already
    # reaches this file, but only from the command line; a rule that wants it
    # as an input needs a label, and depending on `<name>` would pull in the
    # Rust glue and both Dart surfaces to get it.
    #
    # What makes it worth depending on is that it is the *input* the schema
    # fingerprint is computed over (`hash::schema_hash`), not a report about
    # the bindings: two builds whose IR bytes match generate identical
    # bindings, so byte equality is a sound answer to "can code built against
    # the old one still call this?" without parsing the file at all.
    native.filegroup(
        name = name + ".ir",
        srcs = [":" + name],
        output_group = "ir",
        visibility = visibility,
    )

def frustrate_bridge_library(name, crate_name = None, visibility = None, **kwargs):
    """The bridge crate, declared twice: as the shipped cdylib and as a linkable rlib.

    Use this instead of a bare `rust_shared_library` for a bridge crate. It
    declares two targets over **the same sources and the same attributes**:

    * `<name>` — `rust_shared_library` (crate-type `cdylib`). Byte-for-byte
      what a bare `rust_shared_library(name = "<name>", …)` produced, and the
      target every consumer already names: `frustrate_wasm_module(crate = ":<name>")`,
      `flutter_plugin(native_deps = [":<name>"])`, `lib<name>.dylib` /
      `<name>.framework` / `lib<name>.so`.
    * `<name>_lib` — `rust_library` (crate-type `rlib`). Same crate, same
      crate *name*, in the one form another Rust crate can link.

    **Why the second target exists.** rustc cannot `extern crate` a cdylib.
    That single fact left a bridge crate with unit tests only: the only test
    form available was `rust_test(crate = ":<name>")`, which recompiles the
    crate's own sources under `--test` and *explicitly forbids* adding `srcs`
    (rules_rust `rust_test.crate` and `rust_test.srcs` are mutually
    exclusive), so a file under `tests/` had to be `#[path = "../tests/…"]`
    smuggled into some module of the crate behind `#[cfg(test)]` to run at
    all. With the rlib, both ordinary forms work:

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

    **The trap, and why the two targets share one crate name.** Both crates
    are named `crate_name` (default: `name`), so a test writes
    `use my_bridge::…` rather than some derived spelling. That is only safe
    because the two are **never linked together** — nothing can depend on a
    cdylib as a Rust crate, so the rlib and the cdylib never meet. Do not
    "improve" this into a thin cdylib that depends on the rlib: rustc rejects
    that outright with E0519 ("the current crate is indistinguishable from one
    of its dependencies: it has the same crate-name … so this will result in
    symbol conflicts"), and buying past it by renaming one of them puts the
    rename in every test's import line forever.

    The thin-shim shape was measured before being declined, because the
    interesting question is whether frustrate's `#[no_mangle] pub extern "C"`
    exports survive a trip through an rlib at all — nothing in a shim
    references them, and a silently export-less module is far worse than an
    untestable one. They do survive: rustc emits a synthetic object file of
    undefined references to every exported symbol and passes it ahead of the
    rlibs, which forces the defining archive members in, then names the same
    symbols to the linker (`-exported_symbols_list` on Mach-O,
    `--version-script` plus `--no-undefined-version` on ELF, `--export=` on
    wasm — the last two make a lost export a link *error*, so the silent-drop
    fear is real only on Mach-O). Confirmed on aarch64-apple-darwin,
    aarch64-apple-ios, wasm32-unknown-unknown and wasm32-wasip1, at `-O0`,
    `-O3`, thin LTO, fat LTO and with `-dead_strip`.
    This repo relies on it already: every `frustrate_*` export a module
    declares except the three in the generated glue comes from the
    `//runtime/rust:frustrate` **rlib**, not from the bridge crate.

    So the shim route works. It was still declined, for two reasons that have
    nothing to do with symbols: it would change the bytes of the shipped
    artifact for a test-only benefit, and it forces the crate-name rename
    above. The price paid instead is that the crate's own sources compile
    twice in the native configuration (once per crate type). Dependencies are
    shared, and a bridge crate is thin by design, so this is a few seconds.

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
    rust_shared_library(
        name = name,
        crate_name = crate_name,
        visibility = visibility,
        **kwargs
    )

# Build-setting labels for frustrate_block_check's transition, canonicalized in
# frustrate's own repo mapping. A transition's `outputs` and its returned dict
# take plain strings, and these two must be the same string in both places; the
# `Label()` round-trip makes them name these packages unambiguously, from any
# module, the same reason custom_std.bzl canonicalizes the labels it writes into
# generated BUILD files.
_BLOCK_CHECK_FLAG = str(Label("//bazel:block_check"))
_EXTRA_RUSTC_FLAGS = str(Label("@rules_rust//rust/settings:extra_rustc_flags"))

# The check configuration's linker: rust-lld with the export list cut back to
# the check roots. Read //bazel/wasm_block_check/link_export_filter.py for what
# it does and what it cannot break.
_LINK_FILTER = Label("//bazel/wasm_block_check:link_export_filter.py")

# Its execroot-relative source path, which is where rustc runs and so how
# `-Clinker=` must spell it. Composed from the label rather than read off a
# File, because a transition returns strings and runs before analysis.
# `workspace_root` is "" in frustrate's own repo and `external/<canonical>` in a
# consumer's, so one spelling serves both. `_require_link_filter` checks the
# composition against the File Bazel stages.
_LINK_FILTER_PATH = "/".join([
    p
    for p in [_LINK_FILTER.workspace_root, _LINK_FILTER.package, _LINK_FILTER.name]
    if p
])

def _wasm_platform_transition_impl(settings, attr):
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

def _block_check_transition_impl(settings, attr):
    # Four settings, each load-bearing; see frustrate_block_check's doc for the
    # contract this states to a user.
    return {
        # Same open-vocabulary reasoning as the wasm transition above: the
        # platform is the attribute's, not a table in here. It has to be a
        # +atomics one or there is no wait instruction to look for, which is why
        # the default is //bazel:wasm32_threads.
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
        # `-Clinker`: the export filter. rustc hands wasm-ld one explicit
        # `--export` argument per exported symbol, dependency rlibs' included,
        # so the cfg above cannot reach a `#[wasm_bindgen]` export and the
        # filter cuts it off the link line instead. Part of the configuration
        # rather than per-target, and unconditional rather than only where a
        # graph needs it, so the filter's fail-closed check runs on every check
        # build.
        _EXTRA_RUSTC_FLAGS: [
            "--cfg=frustrate_block_check",
            "-Adead_code",
            "-Clinker=" + _LINK_FILTER_PATH,
        ],
        # Turns //runtime/rust's `wasm-threads` feature off while the platform
        # above keeps the +atomics std on.
        # //bazel:BUILD.bazel has the full argument; the short version is that
        # this is what saves the design a second platform, a second toolchain
        # and a second custom_std flavour.
        _BLOCK_CHECK_FLAG: True,
    }

_block_check_transition = transition(
    implementation = _block_check_transition_impl,
    inputs = [],
    outputs = [
        "//command_line_option:platforms",
        "//command_line_option:compilation_mode",
        _EXTRA_RUSTC_FLAGS,
        _BLOCK_CHECK_FLAG,
    ],
)

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
    files = ctx.attr.crate[0][DefaultInfo].files.to_list()
    wasm = [f for f in files if f.extension == "wasm"]
    if len(wasm) != 1:
        fail("frustrate_wasm_module: expected exactly one .wasm output from %s, got [%s]" % (
            ctx.attr.crate[0].label,
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
        # rewrites them — measured on e2e/iroh_demo, whose processed module
        # carries 47,212 names and not one mangled symbol among them.
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
        post_pass = True
    else:
        post_pass = False

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
    # Two questions, one tool, chosen by whether the post-pass ran:
    #
    # `--post-pass`: the CLI rewrote the module, its foreign namespace is now
    # the sidecar's, and what is left to check is the shape the CLI's thread
    # transform produced — it fires whenever the memory is shared and cannot be
    # switched off, rewrites the memory import, deletes the bootstrap symbols
    # the pool pump used to call, and hands per-thread setup to
    # `__wbindgen_start`. frustrate drives that shape, and the CLI version is
    # the *app's* pin rather than frustrate's, so those facts are checked here
    # rather than discovered in a browser. A no-op on a single-threaded module.
    #
    # Plain: no post-pass, so rustc's output (optimized or not) is the module
    # that ships, and every namespace it imports has to be one the runtime puts
    # in the import object. Without it, a graph that needed `bindgen` and did
    # not get it builds green and dies at `WebAssembly.instantiate`, in a
    # browser, with 126 unsatisfiable imports. Once the post-pass HAS run the
    # foreign namespace is `./<stem>_bg.js`, which the runtime satisfies by
    # pointing *any* foreign namespace at the loaded glue module
    # (`bindgenImports` in runtime/dart/lib/src/js/frustrate.js) — so there is
    # no namespace-level fact left to check, and pinning wasm-bindgen's naming
    # convention would only be a way to fail a valid build.
    #
    # A validation action for the same reasons the std check is one, and in the
    # same output group.
    check_marker = ctx.actions.declare_file(
        ctx.label.name + (".post_pass_check" if post_pass else ".import_check"),
    )
    ctx.actions.run(
        executable = ctx.executable._import_check,
        inputs = [module],
        outputs = [check_marker],
        arguments = (["--post-pass"] if post_pass else []) +
                    [module.path, str(ctx.label), check_marker.path],
        mnemonic = "FrustrateWasmPostPassCheck" if post_pass else "FrustrateWasmImportCheck",
        progress_message = ("checking the wasm-bindgen post-pass contract in %{label}" if post_pass else "checking unsatisfiable imports in %{label}"),
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
    },
)

frustrate_wasm_module = rule(
    implementation = _frustrate_wasm_module_impl,
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
        "crate": attr.label(
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
                  "  * //bazel:wasm32_wasi — triple wasm32-wasip1, stable " +
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
                  "//bazel:wasm32_wasi, and the build error names it.\n\n" +
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

def _require_link_filter(ctx):
    """The crate carries the export filter as a compile-time input.

    The transition points `-Clinker=` at it but cannot add an action input, so
    the crate declares it in `compile_data`. Checked here because otherwise the
    failure is a linker-not-found from inside rustc, naming a path and no
    reason — and because comparing the staged File's path to the composed
    `_LINK_FILTER_PATH` is what keeps that composition honest.
    """
    # `TestCrateInfo` rather than `CrateInfo`: rules_rust withholds `CrateInfo`
    # from a cdylib (nothing can link one) and wraps the same struct here
    # instead. It is what `rust_test(crate = ":a_cdylib")` reads.
    crate = ctx.attr.crate[0]
    if rust_common.test_crate_info in crate:
        info = crate[rust_common.test_crate_info].crate
    elif rust_common.crate_info in crate:
        info = crate[rust_common.crate_info]
    else:
        fail("frustrate_block_check: %s is not a rules_rust crate — it " % crate.label +
             "provides neither CrateInfo nor TestCrateInfo. `crate` wants the " +
             "bridge rust_shared_library.")
    compile_data = info.compile_data.to_list()
    if [f for f in compile_data if f.path == _LINK_FILTER_PATH]:
        return
    fail(
        ("frustrate_block_check: %s does not carry the check configuration's " % ctx.attr.crate[0].label) +
        "linker as a compile-time input.\n\n" +
        "Add to that target:\n\n" +
        ("    compile_data = [\"%s\"],\n\n" % _LINK_FILTER) +
        "This rule's transition links the check module with " +
        ("`-Clinker=%s` —\n" % _LINK_FILTER_PATH) +
        "rust-lld with the export list cut back to the check roots. A " +
        "transition cannot add an\naction input, so the crate declares it. " +
        "rustc never opens the file, and it is inert in\nevery other " +
        "configuration.\n\n" +
        "If that path is not where Bazel stages the file, the mismatch is the " +
        "bug rather than\nthe missing line." +
        ("\nThis crate's compile_data: [%s]" % ", ".join([f.path for f in compile_data])),
    )

def _frustrate_block_check_impl(ctx):
    census = ctx.attr.bridge[OutputGroupInfo].claims.to_list()
    if len(census) != 1:
        fail("frustrate_block_check: %s is not a frustrate_bridge target — it " % ctx.attr.bridge.label +
             "has no single `claims` output. Point `bridge` at the " +
             "frustrate_bridge that generated this crate's glue.")
    census = census[0]

    # `<name>.wasm`, with a trailing `.wasm` in the target name absorbed rather
    # than doubled — so both `:foo_block_check` and `:foo_block_check.wasm` name
    # a file `wasm-tools` will open without being told what it is.
    stem = ctx.label.name
    if stem.endswith(".wasm"):
        stem = stem[:-len(".wasm")]

    # No `crate`: the author's declaration that every claim here is settled by
    # placement, so there is nothing to build and nothing to scan. The census is
    # what keeps that honest — the action refuses, by name, any claim that
    # wants a root. Nothing +atomics is reached in this shape, which is why such
    # a target needs no `manual` tag.
    if not ctx.attr.crate:
        marker = ctx.actions.declare_file(stem + ".block_check")
        ctx.actions.run(
            executable = ctx.executable._block_check,
            inputs = [census],
            outputs = [marker],
            arguments = ["--claims-only", census.path, str(ctx.label), marker.path],
            mnemonic = "FrustrateWasmBlockCheck",
            progress_message = "checking no_block claims in %{label}",
        )
        return [
            DefaultInfo(files = depset([])),
            OutputGroupInfo(_validation = depset([marker])),
        ]

    _require_link_filter(ctx)

    files = ctx.attr.crate[0][DefaultInfo].files.to_list()
    wasm = [f for f in files if f.extension == "wasm"]
    if len(wasm) != 1:
        fail("frustrate_block_check: expected exactly one .wasm output from %s, got [%s]" % (
            ctx.attr.crate[0].label,
            ", ".join([f.basename for f in files]),
        ))
    module = wasm[0]

    out = ctx.actions.declare_file(stem + ".wasm")
    ctx.actions.symlink(output = out, target_file = module)

    # A *validation* action, exactly as //bazel:defs.bzl's std_check above and
    # for the same reasons: it produces nothing anyone consumes, and validations
    # run for every target that builds this one without the rule threading a
    # marker through DefaultInfo. `--run_validations` defaults to true, so this
    # needs no .bazelrc entry — which is the point, since diagnostic flags do
    # not live there.
    #
    # Run over rustc's own output. There is no post-pass to choose between here
    # (wasm-bindgen is not in this rule at all), but the reason it could not be
    # is worth stating: the scan decodes the code section and reads mangled Rust
    # symbols out of the name section, and it *whitelists* the export set, so
    # anything that rewrites names or adds an export would make the answer
    # meaningless rather than wrong-and-loud.
    marker = ctx.actions.declare_file(stem + ".block_check")
    ctx.actions.run(
        executable = ctx.executable._block_check,
        inputs = [module, census],
        outputs = [marker],
        arguments = ["--claims", census.path, module.path, str(ctx.label), marker.path],
        mnemonic = "FrustrateWasmBlockCheck",
        progress_message = "checking no_block claims in %{label}",
    )

    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(_validation = depset([marker])),
    ]

frustrate_block_check = rule(
    implementation = _frustrate_block_check_impl,
    doc = "Settle every `#[bridge(no_block)]` claim in a bridge crate: the ones " +
          "that need artifact evidence by building a throwaway wasm module " +
          "that exports nothing else and failing if a wait instruction " +
          "survives lld's GC, and the ones settled by placement by not " +
          "building anything.\n\n" +
          "`#[bridge(no_block)]` is the author's claim that main-thread Dart " +
          "calling this member cannot be stalled. " +
          "Codegen checks what it can see; it cannot see through a call into a " +
          "dependency, so the rest is settled here. Under " +
          "`--cfg frustrate_block_check` the **only** exports are one root per " +
          "artifact-settled member, so lld deletes everything those roots " +
          "cannot reach and //bazel/wasm_block_check asks whether a " +
          "`memory.atomic.wait` is left. Read that binary's header for the " +
          "decision procedure, the panic-path exemption, and what green does " +
          "and does not claim.\n\n" +
          "**Two shapes, and which one a crate gets is a declaration in this " +
          "file.** With `crate`, the module is built and scanned. Without it, " +
          "the target says every claim in the crate is settled by *placement* " +
          "— an actor member's body runs on the actor's own executor, never " +
          "the caller's thread, so no module could add to the claim. A " +
          "claims-only target builds no wasm at all, needs no +atomics std, " +
          "and therefore takes no `manual` tag. It is not a promise taken on " +
          "trust: the codegen-written claim census is an input either way, and " +
          "a claim that wants a root is refused by name, so a project that " +
          "adds one finds out rather than keeping a green that no longer " +
          "covers it.\n\n" +
          "**Nothing this rule builds ships, and nothing production builds " +
          "changes.** The artifact is scanned, never executed or served: its " +
          "roots call the generated glue with an empty request buffer, which " +
          "would fail at run time and does not matter, because what is being " +
          "asked is which code is *linked*. Tag a target that has a `crate` " +
          "`manual` — it needs the locally built +atomics std (run " +
          "`bazel run //toolchain/custom_std:build` first), and a `manual` " +
          "target's own build_test must be tagged too.\n\n" +
          "**The transition overwrites `--@rules_rust//rust/settings:" +
          "extra_rustc_flags` for everything under this target.** Whatever a " +
          "build passed on the command line is gone in this configuration, " +
          "replaced by `--cfg=frustrate_block_check -Adead_code` and the " +
          "`-Clinker=` below. That is " +
          "acceptable here and would not be anywhere else: the setting is a " +
          "list, so a transition can only replace it, and this is a throwaway " +
          "diagnostic artifact — no shipped bytes come out of it, so a flag " +
          "dropped here cannot reach anything. If you need a flag in this " +
          "configuration, put it on the crate's own `rustc_flags`, which the " +
          "transition leaves alone.\n\n" +
          "**Two things to know before pointing this at a real crate graph, " +
          "one with a recipe and one without.**\n\n" +
          "The whole graph must build `+atomics`, and crate_universe resolves " +
          "deps per *triple*, before RUSTFLAGS exist, so it cannot evaluate " +
          "`cfg(target_feature = ...)` and silently drops any dependency " +
          "declared under one. getrandom 0.4.3 needs `js-sys` only under " +
          "atomics and so loses it; the error is an `E0433` inside a " +
          "third-party crate, naming neither frustrate nor this rule. " +
          "(getrandom 0.2 in the same hub wires the same dep correctly, " +
          "because its cfg key is triple-only.) **The fix is one annotation**, " +
          "scoped by version and triple so it cannot reach anything else:\n\n" +
          "    crate.annotation_select(\n" +
          "        crate = \"getrandom\",\n" +
          "        version = \"0.4.3\",\n" +
          "        deps = [\"@your_crates//:js-sys\"],\n" +
          "        repositories = [\"your_crates\"],\n" +
          "        triples = [\"wasm32-unknown-unknown\"],\n" +
          "    )\n\n" +
          "Measured on e2e/iroh_demo: that clears it and the whole iroh graph " +
          "builds under `+atomics`, with the shipped single-threaded module " +
          "byte-identical under a genuine recompile — getrandom's non-atomics " +
          "arm never names `js_sys`. uuid carries the identical pattern and " +
          "wants the identical annotation, but only if its `js` feature is on.\n\n" +
          "The second: the crate must carry the check configuration's linker " +
          "as `compile_data = [\"@frustrate//bazel/wasm_block_check:" +
          "link_export_filter.py\"]`. The transition points `-Clinker=` at " +
          "that file but cannot add an action input, so the crate declares it; " +
          "rustc never opens it, and it is inert in every other " +
          "configuration. Omit it and this rule says so at analysis time.\n\n" +
          "That linker is what makes the rule work on a graph containing " +
          "wasm-bindgen. The cfg suppresses *frustrate's* `#[no_mangle]`s and " +
          "cannot reach a `#[wasm_bindgen]` export inside a dependency rlib, " +
          "so it cuts the `--export` arguments rustc collected for them off " +
          "the link line instead — restoring \"the only exports are the " +
          "roots\" before lld's GC runs. Read that file for what it can and " +
          "cannot break.",
    attrs = {
        "bridge": attr.label(
            mandatory = True,
            providers = [OutputGroupInfo],
            doc = "The frustrate_bridge target that generated this crate's " +
                  "glue. Read for its `claims` output group: the census of " +
                  "every `#[bridge(no_block)]` claim and how each is settled. " +
                  "It is what lets this rule tell three silences apart — a " +
                  "claim set with nothing to prove, one whose roots are " +
                  "missing, and no claims at all — and it costs only the " +
                  "codegen action, never rustc.",
        ),
        "crate": attr.label(
            cfg = _block_check_transition,
            doc = "The bridge rust_shared_library target — the same one " +
                  "frustrate_wasm_module is given. Nothing about the *claims* " +
                  "is declared here: the roots come from the `no_block` " +
                  "annotations in the crate's own sources, which is where the " +
                  "claim belongs (a BUILD-level claim would make one source " +
                  "generate different Rust under cargo and Bazel). " +
                  "The crate's one obligation " +
                  "is mechanical, `compile_data = [\"@frustrate//bazel/" +
                  "wasm_block_check:link_export_filter.py\"]`, and the rule " +
                  "doc says why.\n\n" +
                  "Omit it when every claim in the crate is settled by " +
                  "placement. Nothing under the transition is then built, so " +
                  "the target needs no +atomics std and no `manual` tag.",
        ),
        "platform": attr.label(
            default = Label("//bazel:wasm32_threads"),
            doc = "The target platform. The default is the only one that " +
                  "answers the question: `memory.atomic.wait32` exists only " +
                  "in a `+atomics` build, so on a single-threaded platform a " +
                  "body that locks a `Mutex` produces no wait instruction and " +
                  "the check is green for a reason that has nothing to do " +
                  "with the claim. Override only for another platform whose " +
                  "std is genuinely `+atomics`.\n\n" +
                  "The platform carries //bazel:wasm_threads_enabled and the " +
                  "transition sets //bazel:block_check, which is what turns " +
                  "frustrate's own `wasm-threads` feature off underneath it: " +
                  "+atomics from the platform, feature from the flag. Without " +
                  "that split this rule would need a second threaded " +
                  "platform, and a second `threads = \"enabled\"` custom_std " +
                  "flavour is constraint-ambiguous (bazel/custom_std.bzl).",
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
