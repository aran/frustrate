"""A bridge library built with the idle barrier, for a test harness.

`frustrate_idle_barrier_library` builds `library` with `//bazel:idle_barrier`
on. In that build `FrustrateNative.idleBarrier()` tells a harness how much Rust
work is queued or running and when none is, so it can wait for exactly that
instead of a stretch of stillness.

A transition rather than a flag on the command line, so the flag reaches only
the library a test loads: the app built in the same invocation carries no count.
"""

_IDLE_BARRIER_FLAG = str(Label("//bazel:idle_barrier"))

def _idle_barrier_transition_impl(_settings, _attr):
    return {_IDLE_BARRIER_FLAG: True}

_idle_barrier_transition = transition(
    implementation = _idle_barrier_transition_impl,
    inputs = [],
    outputs = [_IDLE_BARRIER_FLAG],
)

def _impl(ctx):
    library = ctx.attr.library[0]
    return [DefaultInfo(
        files = library[DefaultInfo].files,
        runfiles = library[DefaultInfo].default_runfiles,
    )]

frustrate_idle_barrier_library = rule(
    implementation = _impl,
    doc = """`library`'s files, built with the idle barrier.

Name it in a native test's `data`, or as a `frustrate_code_asset`'s `library`.
The files keep their usual runfiles paths, so do not also depend on `library`
itself: both would claim the same path.""",
    attrs = {
        "library": attr.label(
            mandatory = True,
            cfg = _idle_barrier_transition,
            doc = "The bridge cdylib, e.g. a `frustrate_bridge_library`.",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
