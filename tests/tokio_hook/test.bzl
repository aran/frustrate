"""Run a test with `//ext/tokio:tokio` set to this module's dev-only tokio."""

_FLAG = str(Label("//ext/tokio:tokio"))
_TOKIO = str(Label("@tokio_test_crates//:tokio"))

def _set_tokio_impl(settings, attr):
    return {_FLAG: _TOKIO}

_set_tokio = transition(
    implementation = _set_tokio_impl,
    inputs = [],
    outputs = [_FLAG],
)

def _impl(ctx):
    test = ctx.attr.test[0]
    binary = test[DefaultInfo].files_to_run.executable
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = out, target_file = binary, is_executable = True)
    providers = [DefaultInfo(
        executable = out,
        runfiles = ctx.runfiles(files = [out]).merge(test[DefaultInfo].default_runfiles),
    )]
    if RunEnvironmentInfo in test:
        providers.append(test[RunEnvironmentInfo])
    return providers

with_dev_tokio_test = rule(
    implementation = _impl,
    attrs = {
        # The whole test graph is built under the transition, not only the ext
        # crate: a transitioned edge to `//ext/tokio:frustrate_tokio` alone would
        # pull in a second copy of the runtime, with its own registration slot.
        "test": attr.label(cfg = _set_tokio, executable = True, mandatory = True),
    },
    test = True,
)
