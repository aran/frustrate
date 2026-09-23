"""Custom wasm std toolchains: nightly rustc + a locally built, patched std.

A **flavour** is one facility set built by `toolchain/custom_std/tool/build.dart`
and staged into `toolchain/custom_std/dist/<key>/`. Each flavour gets its own
`rust_toolchain`, selected by constraints alone, so which std a build links is a
property of the platform rather than of what was staged last.

Repos declared by the `custom_std` module extension below:

- `@frustrate_nightly_tools` — the pinned nightly's host tools (rustc, rust-lld,
  cargo, ...), downloaded through rules_rust's own
  `rust_toolchain_tools_repository`: the sanctioned seam, no forks.
- `@frustrate_nightly_host` — the `toolchain()` declaration for those host tools,
  in one place because every flavour needs the same one (proc-macros and build
  scripts consumed by nightly rustc must themselves be nightly-built).
- `@frustrate_custom_std_<flavour>` — one per entry in `_FLAVOURS`, pairing the
  host tools with that flavour's staged rlibs.

**The directory key is derived, not configured.** `_flavour_key` here and
`flavourKey` in `toolchain/custom_std/tool/patch.dart` implement the same rule —
facility names sorted, joined by `-` — so a toolchain looks for exactly the
directory the builder writes. A std built from a different facility set is not
something this can be handed; it is a directory that does not exist, and the
failure names the builder invocation that would create it.
`patch_test.dart` pins the spellings both sides depend on.

**Flags come from the flavour's own manifest**, never from `rustflags.txt`
directly: a `+atomics` std requires `+atomics` app code, so reading both from
one file is what stops a std staged without atomics being handed to a toolchain
that adds them. A manifest whose flags no longer match `rustflags.txt` is
refused as stale, so an edit to the file cannot silently fail to apply.

Green-without-artifacts contract: toolchain resolution loads only the
`toolchain()` declarations (each repo's root BUILD, always valid). The
`rust_toolchain` lives in the `impl` subpackage, whose BUILD is generated as a
loud `fail("run the builder ...")` when the flavour is absent or stale — and
that package is loaded only when a target actually selects it. `bazel test //...`
never touches one (custom-std targets are tagged `manual`).

The repo rule watches its flavour's `manifest.json` (written last by the
builder), so running the builder is picked up by the next build with no
`bazel fetch --force`.
"""

# rules_rust 0.74.0 deleted the public `rust/repositories.bzl`; this symbol now
# lives under `rust/private/`, where its own `extensions.bzl` loads it from. The
# file carries no `visibility()` restriction, so the load is legal — but it is a
# private API, so a rules_rust bump can move it again without a deprecation
# cycle. If it disappears, the replacement is to build the toolchain repo the
# way `rust/extensions.bzl` does.
load("@rules_rust//rust/private:repositories.bzl", "rust_toolchain_tools_repository")

_WASM_TRIPLE = "wasm32-unknown-unknown"

# rules_rust checksums only the nightlies it lists in
# rust/private/known_shas.bzl, and downloads anything else unverified, so the
# pinned nightly's archives are listed here: what rust_toolchain_tools_repository
# fetches for a host->host nightly (rustc, cargo, clippy, llvm-tools, rust-std)
# for every triple in _HOSTS. Values are the xz_hash entries of
# static.rust-lang.org/dist/<date>/channel-rust-nightly.toml, whose .asc
# verifies against Rust's release key 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE.
# Bumping nightly-pin.txt means replacing these.
_NIGHTLY_TOOLS = ["cargo", "clippy", "llvm-tools", "rust-std", "rustc"]

_NIGHTLY_SHA256S = {
    "2026-08-13/cargo-nightly-aarch64-apple-darwin.tar.xz": "8e36fc7cadc354d11f2801fd323ea38aa2333ffd5a6f9031ad821f90ced89739",
    "2026-08-13/clippy-nightly-aarch64-apple-darwin.tar.xz": "f35e72d66ddc9afe042b21872a916d46c4f7c96980a66414a67f62c11b1f0987",
    "2026-08-13/llvm-tools-nightly-aarch64-apple-darwin.tar.xz": "2c1cdaa9e395b1f43d0581f8a9a7caf2f5968e1e68bb83d79ac92267e7bbb766",
    "2026-08-13/rust-std-nightly-aarch64-apple-darwin.tar.xz": "52e6adcd556ae8b79b90378f3168960966da7982cb148461edf6203b3b7e42d4",
    "2026-08-13/rustc-nightly-aarch64-apple-darwin.tar.xz": "b021f087e40c728171743138bd2d47bf0646e1621d640980d47a1f02ad840d40",
    "2026-08-13/cargo-nightly-x86_64-apple-darwin.tar.xz": "441768ea3ad44c541af15358f901c0b28f7a8cc3ede3cbd64965a821e0ff0a7a",
    "2026-08-13/clippy-nightly-x86_64-apple-darwin.tar.xz": "791493ad5b21ae0e88cd18c31e2a4dfd661e97365de3b726c391b0653a163247",
    "2026-08-13/llvm-tools-nightly-x86_64-apple-darwin.tar.xz": "72e845bfe976105fd07c39c39fc17c63d524242edbad47ee81afa4e967436efd",
    "2026-08-13/rust-std-nightly-x86_64-apple-darwin.tar.xz": "fe2ab43b46ba72e05e041407060f8a365d4acb94a01ba3a9e9e239dc6e23d7f2",
    "2026-08-13/rustc-nightly-x86_64-apple-darwin.tar.xz": "2e812eebe29c7c54b9f602e383c824c9dc870c58e672220cc717c1640fbc7ca7",
    "2026-08-13/cargo-nightly-aarch64-unknown-linux-gnu.tar.xz": "fd0098266815bb5b6551203d0b74bc4039cd5709e2553037f75b6c72fe8bcb7c",
    "2026-08-13/clippy-nightly-aarch64-unknown-linux-gnu.tar.xz": "966a5101c22c917d7e841b68c6d19916d66791cd58e09481f962ee4cb61ed094",
    "2026-08-13/llvm-tools-nightly-aarch64-unknown-linux-gnu.tar.xz": "62d3500eab934d96cb8014283c1315e1d6e82891a762fc8f725235991ee99a24",
    "2026-08-13/rust-std-nightly-aarch64-unknown-linux-gnu.tar.xz": "2918bd8899a513bfaff5e02dae4ab8f5eb037350b567a947d5fcb7c43c051265",
    "2026-08-13/rustc-nightly-aarch64-unknown-linux-gnu.tar.xz": "ca38c5d1f5520709d4c9215ab85aec8c151cbe47bdf1a0d2745d559aa25cb0c0",
    "2026-08-13/cargo-nightly-x86_64-unknown-linux-gnu.tar.xz": "f0330a0ca4e096bd8efc6078c1e6faab60b6262193c3513d95b22e9f0c03e36c",
    "2026-08-13/clippy-nightly-x86_64-unknown-linux-gnu.tar.xz": "1bf3437126afe16c60847e2c4a89d3e66cefefffe2177e57075cf2d85b988e39",
    "2026-08-13/llvm-tools-nightly-x86_64-unknown-linux-gnu.tar.xz": "46e8f9c3ffa0d80633610cb9ad172d7fab7597134f4f5c251ab6e07fb3dcdff3",
    "2026-08-13/rust-std-nightly-x86_64-unknown-linux-gnu.tar.xz": "48b212379a49de9bd9fbbe493b6772956b260dc9df1269f48237ab9780aab437",
    "2026-08-13/rustc-nightly-x86_64-unknown-linux-gnu.tar.xz": "f2b529baf006f847dd1f8fc09c09c1fe292e7bcea5161ee62d2e4451a6b121f3",
}

# module_ctx.os (name, arch) -> (rust host triple, exec constraints).
_HOSTS = {
    ("mac os x", "aarch64"): ("aarch64-apple-darwin", ["@platforms//os:macos", "@platforms//cpu:aarch64"]),
    ("mac os x", "x86_64"): ("x86_64-apple-darwin", ["@platforms//os:macos", "@platforms//cpu:x86_64"]),
    ("linux", "aarch64"): ("aarch64-unknown-linux-gnu", ["@platforms//os:linux", "@platforms//cpu:aarch64"]),
    ("linux", "amd64"): ("x86_64-unknown-linux-gnu", ["@platforms//os:linux", "@platforms//cpu:x86_64"]),
    ("linux", "x86_64"): ("x86_64-unknown-linux-gnu", ["@platforms//os:linux", "@platforms//cpu:x86_64"]),
}

# The flavours this ruleset declares a toolchain for.
#
# `facilities` is what you pass to the builder; `threads` is the value of the
# //bazel:wasm_threads constraint the flavour's toolchain requires. Every
# toolchain also requires //bazel:wasm_std_custom, which is what keeps all of
# them off the plain //bazel:wasm32 platform and its stock stable std.
#
# The three toolchains for this triple are then mutually exclusive on
# constraints alone -- stock (channel:stable), threads (wasm_threads_enabled),
# custom (wasm_threads_disabled + wasm_std_custom) -- so registration order
# decides nothing. That property is worth keeping: these differ in
# extra_rustc_flags, and picking the wrong one is a link error far from its
# cause rather than a resolution error.
_FLAVOURS = {
    "threads": struct(
        facilities = ["atomics"],
        threads = "enabled",
    ),
    "browser": struct(
        facilities = ["clock", "random", "stdio", "thread"],
        threads = "disabled",
    ),
}

def _flavour_key(facilities):
    """The staging directory name: names sorted, joined by `-`.

    Mirrors `flavourKey` in toolchain/custom_std/tool/patch.dart. Both sides
    must agree byte for byte; patch_test.dart pins the spellings.
    """
    return "-".join(sorted(facilities))

_BUILDER = "bazel run //toolchain/custom_std:build --"

# Canonicalized in this file's (frustrate's) repo mapping, so the strings
# embedded in generated BUILD files resolve correctly both when frustrate
# is the root module and when a dependent workspace (e2e/flutter_demo)
# reaches it through local_path_override.
_LIB_DIR = "lib/rustlib/" + _WASM_TRIPLE + "/lib"

def _label(s):
    return str(Label(s))

_ROOT_BUILD = """\
# Generated by @frustrate//bazel:custom_std.bzl for the {flavour} flavour
# ({facilities}). The declaration is always valid -- toolchain resolution
# loads only this package; //impl (the actual rust_toolchain) is loaded only
# when a build selects it.
toolchain(
    name = "toolchain",
    exec_compatible_with = {exec_constraints},
    target_compatible_with = [
        "{cpu_wasm32}",
        "{os_none}",
        "{threads_value}",
        "{std_custom}",
    ],
    toolchain = "//impl:rust_toolchain",
    toolchain_type = "{toolchain_type}",
)
{stdlib}
"""

# The pinned nightly's host->host toolchain, declared once for every flavour
# (they all need the same one). Selected only when the rules_rust channel flag
# is "nightly", which every custom-std platform sets and which propagates into
# exec configurations: proc-macros and build scripts consumed by nightly rustc
# must themselves be nightly-built -- a stable proc-macro dylib does not load
# into nightly rustc.
_HOST_BUILD = """\
# Generated by @frustrate//bazel:custom_std.bzl.
toolchain(
    name = "host_toolchain",
    exec_compatible_with = {exec_constraints},
    target_compatible_with = {exec_constraints},
    target_settings = ["{channel_nightly}"],
    toolchain = "@frustrate_nightly_tools//:rust_toolchain",
    toolchain_type = "{toolchain_type}",
)
"""

# In the root package with the rlib files themselves, twice over: the
# filegroup declares .a symlinks next to the rlibs (same-package by
# construction), and the rlibs must sit at the repo-root lib/rustlib/...
# path — rust_toolchain's generated sysroot preserves file paths, and
# rustc resolves std from $sysroot/lib/rustlib/. Analyzed only when the
# threaded toolchain is actually selected.
_ROOT_STDLIB = """\

load("{toolchain_bzl}", "rust_stdlib_filegroup")

rust_stdlib_filegroup(
    name = "rust_std",
    srcs = glob(["{lib_dir}/*.rlib"]),
    visibility = ["//impl:__pkg__"],
)
"""

_IMPL_BUILD = """\
# Generated by @frustrate//bazel:custom_std.bzl from {manifest}.
load("{toolchain_bzl}", "rust_toolchain")

rust_toolchain(
    name = "rust_toolchain",
    binary_ext = ".wasm",
    cargo = "@frustrate_nightly_tools//:cargo",
    cargo_clippy = "@frustrate_nightly_tools//:cargo_clippy_bin",
    channel = "nightly",
    clippy_driver = "@frustrate_nightly_tools//:clippy_driver_bin",
    default_edition = "2021",
    dylib_ext = ".wasm",
    exec_triple = "{exec_triple}",
    # Every crate in this configuration compiles with the flags this
    # flavour's std was built with, read from its own manifest: a +atomics
    # std requires +atomics app code, so the flags are a toolchain property
    # and must come from the same place the std did.
    extra_rustc_flags = {rustflags},
    iso_date = "{iso_date}",
    linker = "@frustrate_nightly_tools//:rust-lld",
    linker_type = "direct",
    rust_doc = "@frustrate_nightly_tools//:rustdoc",
    rust_std = "//:rust_std",
    rustc = "@frustrate_nightly_tools//:rustc",
    rustc_lib = "@frustrate_nightly_tools//:rustc_lib",
    staticlib_ext = ".a",
    stdlib_linkflags = [],
    target_triple = "{wasm_triple}",
    visibility = ["//visibility:public"],
)
"""

_MISSING = """\
fail(\"\"\"
frustrate custom std: the '{flavour}' flavour has not been staged.

A patched std is built locally, never checked in. Run:

    {builder} --facilities={facilities}

then re-run this build. Looked in:

    toolchain/custom_std/dist/{key}/
\"\"\")
"""

_STALE = """\
fail(\"\"\"
frustrate custom std: the '{flavour}' flavour is stale.

dist/{key}/ was built with {built}, but toolchain/custom_std/nightly-pin.txt
pins {pin}. An rlib from another compiler is rejected outright (E0514), so
re-run:

    {builder} --facilities={facilities}
\"\"\")
"""

_STALE_FLAGS = """\
fail(\"\"\"
frustrate custom std: the '{flavour}' flavour is stale.

dist/{key}/ was built with flags other than toolchain/custom_std/rustflags.txt
now lists. The toolchain takes its flags from the std's manifest, so the
file's edit would otherwise never reach a build. Re-run:

    {builder} --facilities={facilities}
\"\"\")
"""

# The manifest disagrees with the directory it sits in. Only reachable by
# hand-editing, since the builder derives the path from the same list -- but a
# std that is not what its path claims is the one thing this design exists to
# make impossible, so it is checked rather than assumed.
_WRONG = """\
fail(\"\"\"
frustrate custom std: dist/{key}/ contains the wrong flavour.

Its manifest records facilities {built}, but the directory name says
{facilities}. Delete it and re-run:

    {builder} --facilities={facilities}
\"\"\")
"""

def _custom_std_repo_impl(rctx):
    key = _flavour_key(rctx.attr.facilities)
    facilities = ",".join(sorted(rctx.attr.facilities))

    # Anchored to frustrate's own checkout: resolves identically from the
    # nested demo workspace, which reaches frustrate via local_path_override.
    dist = rctx.path(Label("//:MODULE.bazel")).dirname.get_child("toolchain").get_child("custom_std").get_child("dist").get_child(key)
    manifest = dist.get_child("manifest.json")

    # Watch even when absent: the builder writing the manifest (last) is
    # what flips this repo from fail()-ing to real on the next build.
    rctx.watch(manifest)

    def root_build(stdlib):
        return _ROOT_BUILD.format(
            flavour = rctx.attr.flavour,
            facilities = facilities,
            exec_constraints = json.encode(rctx.attr.host_constraints),
            cpu_wasm32 = _label("@platforms//cpu:wasm32"),
            os_none = _label("@platforms//os:none"),
            threads_value = _label("//bazel:wasm_threads_" + rctx.attr.threads),
            std_custom = _label("//bazel:wasm_std_custom"),
            toolchain_type = _label("@rules_rust//rust:toolchain_type"),
            stdlib = stdlib,
        )

    def refuse(body):
        rctx.file("BUILD.bazel", root_build(""))
        rctx.file("impl/BUILD.bazel", body)

    if not manifest.exists:
        refuse(_MISSING.format(
            flavour = rctx.attr.flavour,
            builder = _BUILDER,
            facilities = facilities,
            key = key,
        ))
        return

    m = json.decode(rctx.read(manifest))
    if m["nightly"] != rctx.attr.nightly_pin:
        refuse(_STALE.format(
            flavour = rctx.attr.flavour,
            key = key,
            built = m["nightly"],
            pin = rctx.attr.nightly_pin,
            builder = _BUILDER,
            facilities = facilities,
        ))
        return

    if sorted(m["facilities"]) != sorted(rctx.attr.facilities):
        refuse(_WRONG.format(
            key = key,
            built = ",".join(sorted(m["facilities"])),
            facilities = facilities,
            builder = _BUILDER,
        ))
        return

    if m["rustflags"] != _expected_rustflags(rctx, "atomics" in rctx.attr.facilities):
        refuse(_STALE_FLAGS.format(
            flavour = rctx.attr.flavour,
            key = key,
            builder = _BUILDER,
            facilities = facilities,
        ))
        return

    for name in m["files"]:
        rctx.symlink(
            dist.get_child("lib").get_child("rustlib").get_child(_WASM_TRIPLE).get_child("lib").get_child(name),
            _LIB_DIR + "/" + name,
        )

    rctx.file("BUILD.bazel", root_build(_ROOT_STDLIB.format(
        toolchain_bzl = _label("@rules_rust//rust:toolchain.bzl"),
        lib_dir = _LIB_DIR,
    )))
    rctx.file("impl/BUILD.bazel", _IMPL_BUILD.format(
        manifest = str(manifest),
        toolchain_bzl = _label("@rules_rust//rust:toolchain.bzl"),
        exec_triple = rctx.attr.exec_triple,
        # From the manifest, not rustflags.txt: the std's flags and its
        # consumers' flags then have one source by construction.
        rustflags = json.encode(m["rustflags"]),
        iso_date = rctx.attr.iso_date,
        wasm_triple = _WASM_TRIPLE,
    ))

def _expected_rustflags(rctx, atomics):
    """The flags the builder would record today; mirrors `rustflags` in build.dart."""
    if not atomics:
        return []
    path = rctx.path(Label("//toolchain/custom_std:rustflags.txt"))
    rctx.watch(path)
    flags = []
    for line in rctx.read(path).splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            flags.extend(line.split(" "))
    return flags

_custom_std_repo = repository_rule(
    implementation = _custom_std_repo_impl,
    attrs = {
        "exec_triple": attr.string(mandatory = True),
        "facilities": attr.string_list(mandatory = True),
        "flavour": attr.string(mandatory = True),
        "host_constraints": attr.string_list(mandatory = True),
        "iso_date": attr.string(mandatory = True),
        "nightly_pin": attr.string(mandatory = True),
        "threads": attr.string(mandatory = True, values = ["enabled", "disabled"]),
    },
    # Re-evaluated when the watched manifest changes; cheap either way.
    local = True,
)

def _nightly_host_repo_impl(rctx):
    rctx.file("BUILD.bazel", _HOST_BUILD.format(
        exec_constraints = json.encode(rctx.attr.host_constraints),
        channel_nightly = _label("@rules_rust//rust/toolchain/channel:nightly"),
        toolchain_type = _label("@rules_rust//rust:toolchain_type"),
    ))

_nightly_host_repo = repository_rule(
    implementation = _nightly_host_repo_impl,
    attrs = {"host_constraints": attr.string_list(mandatory = True)},
)

def _custom_std_impl(module_ctx):
    pin = module_ctx.read(Label("//toolchain/custom_std:nightly-pin.txt")).strip()
    if not pin.startswith("nightly-"):
        fail("toolchain/custom_std/nightly-pin.txt must contain a single 'nightly-YYYY-MM-DD' line, got %r" % pin)
    iso_date = pin[len("nightly-"):]

    os_name = module_ctx.os.name
    arch = module_ctx.os.arch
    if (os_name, arch) not in _HOSTS:
        fail("frustrate custom std: unsupported host for the nightly " +
             "toolchain: %s/%s" % (os_name, arch))
    exec_triple, host_constraints = _HOSTS[(os_name, arch)]
    constraints = [_label(c) for c in host_constraints]

    archives = ["%s/%s-nightly-%s.tar.xz" % (iso_date, tool, exec_triple) for tool in _NIGHTLY_TOOLS]
    missing = [a for a in archives if a not in _NIGHTLY_SHA256S]
    if missing:
        fail(("frustrate custom std: no checksums for %s on %s, so its toolchain " +
              "would download unverified. Add to _NIGHTLY_SHA256S in " +
              "bazel/custom_std.bzl the xz_hash of each of\n\n    %s\n\n" +
              "from static.rust-lang.org/dist/%s/channel-rust-nightly.toml, after " +
              "verifying its .asc against Rust's release key.") %
             (pin, exec_triple, "\n    ".join(missing), iso_date))

    rust_toolchain_tools_repository(
        name = "frustrate_nightly_tools",
        version = "nightly/" + iso_date,
        exec_triple = exec_triple,
        sha256s = {a: _NIGHTLY_SHA256S[a] for a in archives},
        # host->host: besides rustc/rust-lld/cargo (referenced by every
        # flavour's toolchain), this yields a complete nightly host toolchain
        # — declared channel-gated in @frustrate_nightly_host for the
        # proc-macros and build scripts nightly rustc consumes.
        target_triple = exec_triple,
        edition = "2021",
    )

    _nightly_host_repo(
        name = "frustrate_nightly_host",
        host_constraints = constraints,
    )

    for flavour, spec in _FLAVOURS.items():
        _custom_std_repo(
            name = "frustrate_custom_std_" + flavour,
            exec_triple = exec_triple,
            facilities = spec.facilities,
            flavour = flavour,
            host_constraints = constraints,
            iso_date = iso_date,
            nightly_pin = pin,
            threads = spec.threads,
        )

# The repos this generates depend on the host's OS and CPU (exec constraints,
# the nightly host tools), so the lockfile must key its recorded result by them.
# Without these two, a lockfile written on one host is replayed on another.
custom_std = module_extension(
    implementation = _custom_std_impl,
    arch_dependent = True,
    os_dependent = True,
)
