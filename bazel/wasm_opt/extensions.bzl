"""An optional binaryen (`wasm-opt`) fetch, for apps that want one from the graph.

frustrate's wasm rules take `wasm_opt` as a label rather than supplying the
binary, so nothing here is required to use them — see the `wasm_opt` attribute
on `frustrate_wasm_module`. This extension exists so that an app that has no
opinion about which binaryen it wants can get a pinned, checksummed one in
three lines instead of writing its own `http_archive` and hunting the sha256s:

    binaryen = use_extension("@frustrate//bazel/wasm_opt:extensions.bzl", "binaryen")
    use_repo(binaryen, "binaryen")

and then `wasm_opt = "@binaryen//:wasm-opt"` on the module target.

**It is not registered by frustrate and never fetches unless something asks
for it.** A binaryen release is a large download, and much larger on some
hosts than others; pulling one into the graph of every frustrate consumer —
including the ones that never build for the web — is not this module's
decision to make, which is the same
reason `frustrate_wasm_module` does not default the `wasm_opt` attribute.

Pin a different release with `binaryen.toolchain(version = "...")` and the four
checksums; the defaults below are version 132.
"""

# The release these checksums are for. Naming any other version means
# supplying its checksum too — see the fail() in the extension below.
_DEFAULT_VERSION = "132"

# One entry per host binaryen publishes a release for. Keyed by the pair
# `repository_ctx.os` reports, because that is what the repository rule can
# actually see — not by a Bazel platform, since this is a host tool fetched
# during the loading phase, before any platform exists to resolve against.
_HOSTS = {
    ("mac os x", "aarch64"): ("arm64-macos", "98aad827847af7ef990ed7098d885725c8e5b5aae75073403635617ae4e259aa"),
    ("mac os x", "x86_64"): ("x86_64-macos", "40c3de90bb3766bd0282a895e139a6f50253dba49b4f5bb89e66faca162d832e"),
    ("linux", "aarch64"): ("aarch64-linux", "c58562417836c5d0493d89bdefc434933bdc097db641b483df86bcfa557a107f"),
    ("linux", "x86_64"): ("x86_64-linux", "195ddc94f9bc89f45abdabb0b9eea86023d727ba90eac8b35b80f2544fc30572"),
}

# The layout below is load-bearing rather than tidy, and the reason differs by
# host — both checked against the published version_132 archives:
#
#   macOS ships `lib/libbinaryen.dylib` and a `wasm-opt` that resolves it
#   through an `LC_RPATH` of `@loader_path/../lib` (read with `otool -l`). The
#   binary therefore cannot be lifted out of the archive on its own.
#
#   Linux ships `lib/libbinaryen.a` — a static archive — so its `wasm-opt` has
#   nothing to resolve at run time and would tolerate any layout.
#
# So the constraint is macOS's, and the layout satisfies it everywhere rather
# than branching: the archive is extracted with its `binaryen-version_N/`
# prefix stripped, putting `lib/` at the repository root, and `native_binary`
# copies the tool to `host/wasm-opt` — exactly one directory deep, so `../lib`
# from the copy lands on that `lib/`. The runfiles tree mirrors the package
# layout, so the same relative path resolves at action time. A copy to the
# repository root instead would put `../lib` outside the repository and the
# tool would die on a missing dylib the first time an action ran it.
#
# The cost of not branching is that a Linux build carries a `libbinaryen.a` it
# will never open into the action's runfiles — a static archive nothing reads,
# for one fewer conditional, and the conditional would have to be written
# against a host the author cannot test.
#
# `native_binary` (bazel_skylib, already a direct dependency) rather than
# `sh_binary`: Bazel 9 moved `sh_binary` out of the native rule set into
# rules_shell, and a launcher script would mean a `bazel_dep` every frustrate
# consumer inherits for the sake of two lines of exec.
_BUILD = """\
load("@bazel_skylib//rules:native_binary.bzl", "native_binary")

filegroup(
    name = "lib",
    srcs = glob(["lib/**"], allow_empty = True),
)

native_binary(
    name = "wasm-opt",
    src = "bin/wasm-opt",
    out = "host/wasm-opt",
    data = [":lib"],
    visibility = ["//visibility:public"],
)
"""

def _binaryen_repo_impl(repository_ctx):
    os_name = repository_ctx.os.name.lower()
    arch = repository_ctx.os.arch.lower()
    if arch == "arm64":
        arch = "aarch64"
    if arch == "amd64":
        arch = "x86_64"

    key = (os_name, arch)
    if key not in _HOSTS:
        fail(
            "binaryen: no published release for host %s/%s. Binaryen ships " % (os_name, arch) +
            "macOS and Linux on arm64/x86_64 (and Windows, which frustrate " +
            "has never built). Supply your own wasm-opt and name it in the " +
            "`wasm_opt` attribute instead of using this extension.",
        )
    slug, sha = _HOSTS[key]
    version = repository_ctx.attr.version

    repository_ctx.download_and_extract(
        url = "https://github.com/WebAssembly/binaryen/releases/download/version_{v}/binaryen-version_{v}-{s}.tar.gz".format(v = version, s = slug),
        sha256 = repository_ctx.attr.sha256 or sha,
        stripPrefix = "binaryen-version_{v}".format(v = version),
    )
    repository_ctx.file("BUILD.bazel", _BUILD)

_binaryen_repo = repository_rule(
    implementation = _binaryen_repo_impl,
    attrs = {
        "version": attr.string(default = _DEFAULT_VERSION),
        "sha256": attr.string(
            doc = "Override the checksum for this host's archive. Required " +
                  "when `version` names a release this file has no entry for.",
        ),
    },
)

_toolchain = tag_class(attrs = {
    "version": attr.string(
        default = "132",
        doc = "The binaryen release to fetch. The built-in checksums are for " +
              "132; naming another version needs `sha256` for the host you " +
              "build on.",
    ),
    "sha256": attr.string(
        doc = "Checksum of this host's archive, when `version` is not 132.",
    ),
})

def _binaryen_impl(module_ctx):
    version = _DEFAULT_VERSION
    sha256 = ""
    root_chose = False
    for mod in module_ctx.modules:
        for tag in mod.tags.toolchain:
            # Root wins outright rather than by version comparison: unlike a
            # language SDK there is no compatibility relation between binaryen
            # releases that would make "newest" the safe merge, and the app is
            # what ships the artifact, so the app picks. Tracked as a flag
            # rather than by comparing the version against the default — a root
            # module that pins the default value explicitly still means to have
            # chosen it, and a sentinel comparison would let a dependency
            # override that.
            if root_chose and not mod.is_root:
                continue
            version = tag.version
            sha256 = tag.sha256
            if mod.is_root:
                root_chose = True

    if version != _DEFAULT_VERSION and not sha256:
        fail(
            "binaryen: version %r has no built-in checksum (only %s does). " % (version, _DEFAULT_VERSION) +
            "Pass the archive's sha256 for the host you build on:\n\n" +
            "    binaryen.toolchain(version = %r, sha256 = \"...\")\n\n" % version +
            "It is published beside the release as " +
            "binaryen-version_%s-<host>.tar.gz.sha256. Without it the fetch " % version +
            "would be attempted with %s's checksum and fail as a corrupt " % _DEFAULT_VERSION +
            "download, which is a much worse way to learn this.",
        )

    _binaryen_repo(name = "binaryen", version = version, sha256 = sha256)
    return module_ctx.extension_metadata(reproducible = True)

binaryen = module_extension(
    implementation = _binaryen_impl,
    tag_classes = {"toolchain": _toolchain},
    doc = "Fetches a pinned binaryen release and exposes `@binaryen//:wasm-opt`.",
)
