"""Link flags that link with the Rust toolchain's own lld.

A C compiler driver picks its linker by name: `-fuse-ld=lld` makes it run the
first `ld.lld` it finds on its `-B` prefixes and then `PATH`. The Rust toolchain
ships one, `gcc-ld/ld.lld` (a wrapper around `rust-lld`), and rules_rust stages
it in the toolchain's sysroot next to `rust_toolchain.linker`. Pointing `-B` at
that directory means the link needs no lld on the host.
"""

load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")

def _rust_lld_link_flags_impl(ctx):
    toolchain = ctx.toolchains[str(Label("@rules_rust//rust:toolchain_type"))]
    if not toolchain.linker:
        fail((
            "%s: links with the Rust toolchain's lld, and this toolchain declares no " +
            "`linker`. rules_rust's `rust.toolchain` declares `rust-lld` by default."
        ) % ctx.label)
    flags = ["-fuse-ld=lld", "-B" + toolchain.linker.dirname + "/gcc-ld"] + ctx.attr.linkopts
    linker_input = cc_common.create_linker_input(
        owner = ctx.label,
        user_link_flags = depset(flags),
    )
    return [CcInfo(linking_context = cc_common.create_linking_context(
        linker_inputs = depset([linker_input]),
    ))]

rust_lld_link_flags = rule(
    implementation = _rust_lld_link_flags_impl,
    doc = "`linkopts` for a link that must use lld, taking it from the Rust toolchain.",
    attrs = {
        "linkopts": attr.string_list(doc = "Further flags for the same link."),
    },
    toolchains = [str(Label("@rules_rust//rust:toolchain_type"))],
    provides = [CcInfo],
)
