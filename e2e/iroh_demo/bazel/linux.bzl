"""A one-rule bridge that lets a macOS `bazel test` build the Linux app.

`//:app_linux` can only be analysed with `--platforms=...:linux_x64 -c dbg`, and
a `build_test` has no way to say that: `build_test` builds its targets in
whatever configuration the test itself is in, which on this host is macOS
fastbuild. Wrapping the app in a rule with an outgoing transition moves the
requirement into the graph, where a test can reach it.

Both settings are load-bearing:

  * `platforms` — obviously; without it nothing Linux resolves.
  * `compilation_mode = "dbg"` — less obviously. Flutter's `gen_snapshot` only
    emits native code for the host OS, so an AOT (fastbuild/opt) Linux build
    from macOS fails in analysis with "AOT cross-compilation between desktop
    platforms is not supported". Debug mode is JIT: the Dart code ships as a
    kernel snapshot the engine interprets, which is host-independent. So a
    cross-built Linux bundle is always a debug bundle, and a release Linux
    bundle has to be built on Linux.
"""

def _linux_x86_64_transition_impl(_settings, _attr):
    return {
        "//command_line_option:compilation_mode": "dbg",
        "//command_line_option:platforms": [
            str(Label("@rules_flutter//flutter/platforms:linux_x64")),
        ],
    }

_linux_x86_64_transition = transition(
    implementation = _linux_x86_64_transition_impl,
    inputs = [],
    outputs = [
        "//command_line_option:compilation_mode",
        "//command_line_option:platforms",
    ],
)

def _linux_x86_64_build_impl(ctx):
    return [DefaultInfo(files = depset(
        transitive = [t[DefaultInfo].files for t in ctx.attr.srcs],
    ))]

linux_x86_64_build = rule(
    implementation = _linux_x86_64_build_impl,
    doc = "Builds `srcs` for linux-x86_64 in debug mode, whatever the host is.",
    attrs = {
        "srcs": attr.label_list(
            mandatory = True,
            cfg = _linux_x86_64_transition,
            doc = "Targets to build under the Linux configuration.",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
