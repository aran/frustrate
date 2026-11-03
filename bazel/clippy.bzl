"""Clippy over Rust targets, as build targets that fail on a finding.

`bazel test //...` builds every target it matches, so a `frustrate_clippy`
that finds anything fails it. Warnings are errors: `.bazelrc` passes
`-Dwarnings` to clippy actions only.
"""

load("@rules_rust//rust:defs.bzl", "rust_clippy_aspect")
load("//bazel:defs.bzl", "wasm_platform_transition")

def _impl(ctx):
    return [DefaultInfo(files = depset(
        [],
        transitive = [
            dep[OutputGroupInfo].clippy_checks
            for dep in ctx.attr.crates
            if "clippy_checks" in dir(dep[OutputGroupInfo])
        ],
    ))]

frustrate_clippy = rule(
    implementation = _impl,
    doc = "Runs clippy on `crates`, built for the wasm `platform`: the " +
          "configuration `frustrate_wasm_module` builds them in, so the " +
          "`cfg(target_family = \"wasm\")` code that a host build never " +
          "compiles is the code analyzed. The host build is analyzed by " +
          "the aspect `bazel test` applies (.bazelrc).",
    attrs = {
        "crates": attr.label_list(
            cfg = wasm_platform_transition,
            aspects = [rust_clippy_aspect],
            doc = "Rust targets to run clippy on.",
        ),
        "platform": attr.label(
            mandatory = True,
            doc = "The target platform, as for `frustrate_wasm_module`.",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
