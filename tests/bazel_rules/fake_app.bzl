"""An app's shape, for frustrate_no_block_check's `shipped`: it builds its
library under a platform transition of its own, as a mobile app's rules do."""

def _app_transition_impl(_settings, attr):
    return {
        "//command_line_option:platforms": [str(attr.platform)],
        str(Label("@rules_rust//rust/settings:default_allocator_library")): str(Label("//bazel:no_block_check_allocator")),
    }

_app_transition = transition(
    implementation = _app_transition_impl,
    inputs = [],
    outputs = [
        "//command_line_option:platforms",
        str(Label("@rules_rust//rust/settings:default_allocator_library")),
    ],
)

def _fake_app_impl(_ctx):
    # Nothing to build: linking the library would need the platform's C
    # toolchain, and the analysis reads only its IR.
    return [DefaultInfo()]

fake_app = rule(
    implementation = _fake_app_impl,
    attrs = {
        "library": attr.label(cfg = _app_transition, mandatory = True),
        "platform": attr.label(mandatory = True),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
