"""A bridge library built with the manual scheduler, for a test harness.

`frustrate_manual_scheduler_library` builds `library` with
`//bazel:manual_scheduler` on. In that build a Rust task runs only when the host
drains the run queue (`ManualScheduler` in package:frustrate/manual_scheduler.dart),
so a harness chooses how Rust tasks interleave with its own events and a seeded
run replays exactly.

A transition rather than a flag on the command line, so the flag reaches only
the library a test loads: the app built in the same invocation keeps the pool.
"""

_MANUAL_SCHEDULER_FLAG = str(Label("//bazel:manual_scheduler"))

def _manual_scheduler_transition_impl(_settings, _attr):
    return {_MANUAL_SCHEDULER_FLAG: True}

_manual_scheduler_transition = transition(
    implementation = _manual_scheduler_transition_impl,
    inputs = [],
    outputs = [_MANUAL_SCHEDULER_FLAG],
)

def _impl(ctx):
    library = ctx.attr.library[0]
    return [DefaultInfo(
        files = library[DefaultInfo].files,
        runfiles = library[DefaultInfo].default_runfiles,
    )]

frustrate_manual_scheduler_library = rule(
    implementation = _impl,
    doc = """`library`'s files, built with the manual scheduler.

Name it in a native test's `data`. The files keep their usual runfiles paths,
so do not also depend on `library` itself: both would claim the same path.""",
    attrs = {
        "library": attr.label(
            mandatory = True,
            cfg = _manual_scheduler_transition,
            doc = "The bridge cdylib, e.g. a `frustrate_bridge_library`.",
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
