"""Analysis tests over where the per-compiler `I64List` comes from.

`dart_js_binary` and `dart_wasm_binary` build their `deps` for a platform with
`@rules_dart//dart/constraints:js` or `:wasm`, and `//runtime/dart:frustrate`
picks its variant of package `frustrate_i64_list` from that constraint.
"""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_dart//dart:providers.bzl", "DartAnalyzableInfo")
load("@rules_dart//dart:utils.bzl", "WEB_PLATFORM_SETTINGS", "web_platform_settings")

def _variant_test_impl(ctx):
    env = analysistest.begin(ctx)
    srcs = [
        f.short_path
        for f in analysistest.target_under_test(env)[DartAnalyzableInfo].dart_info.transitive_srcs.to_list()
    ]
    asserts.true(env, ctx.attr.picked in srcs, "%s should be reached: %s" % (ctx.attr.picked, srcs))
    asserts.false(env, ctx.attr.not_picked in srcs, "%s should not be reached: %s" % (ctx.attr.not_picked, srcs))
    return analysistest.end(env)

# A web binary reaches only its own compiler's variant of package
# `frustrate_i64_list`.
variant_test = analysistest.make(
    _variant_test_impl,
    attrs = {
        "picked": attr.string(),
        "not_picked": attr.string(),
    },
)

_TRACKING = "@rules_dart//dart:track_creation_locations"

_Roots = provider(
    doc = "Output root of each file of a target, per Dart build configuration.",
    fields = ["by_compiler"],
)

def _compilers_impl(settings, _attr):
    plain = {s: settings[s] for s in WEB_PLATFORM_SETTINGS}
    branches = {
        "none": plain,
        "js": web_platform_settings(settings, Label("@rules_dart//dart/platforms:js")),
        "wasm": web_platform_settings(settings, Label("@rules_dart//dart/platforms:wasm")),
        # What `flutter_test` turns on.
        "tracking": plain,
    }
    return {
        name: dict(branch, **{_TRACKING: name == "tracking"})
        for name, branch in branches.items()
    }

_compilers = transition(
    implementation = _compilers_impl,
    inputs = WEB_PLATFORM_SETTINGS,
    outputs = WEB_PLATFORM_SETTINGS + [_TRACKING],
)

def _roots_impl(ctx):
    return [_Roots(by_compiler = {
        compiler: {f.short_path: f.root.path for f in target[DefaultInfo].files.to_list()}
        for compiler, target in ctx.split_attr.target.items()
    })]

# `target` reached once under each web compiler and once tracking creation
# locations, to compare where its files are built.
_roots = rule(
    implementation = _roots_impl,
    attrs = {"target": attr.label(cfg = _compilers, mandatory = True)},
)

def _built_once_test_impl(ctx):
    env = analysistest.begin(ctx)
    by_compiler = analysistest.target_under_test(env)[_Roots].by_compiler
    asserts.true(env, by_compiler["none"], "the target has no files")
    for compiler, roots in by_compiler.items():
        asserts.equals(
            env,
            by_compiler["none"],
            roots,
            "built for %s differently from a plain build" % compiler,
        )
    return analysistest.end(env)

_built_once_analysis_test = analysistest.make(_built_once_test_impl)

def built_once_test(name, target, **kwargs):
    """Fails if `target`'s files are built in another configuration by a web compiler or by tracking."""
    _roots(name = name + "_roots", target = target, tags = ["manual"])
    _built_once_analysis_test(name = name, target_under_test = ":" + name + "_roots", **kwargs)
