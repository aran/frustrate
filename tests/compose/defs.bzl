"""A code asset under an id of the test's choosing."""

load("@rules_dart//dart:providers.bzl", "DartCodeAssetInfo")

def _code_asset_as_impl(ctx):
    libraries = [f for f in ctx.files.library if f.extension in ("so", "dylib", "dll")]
    if len(libraries) != 1:
        fail("%s: expected one shared library from %s" % (ctx.label, ctx.attr.library.label))
    return [
        DefaultInfo(files = depset(libraries)),
        DartCodeAssetInfo(
            asset_id = ctx.attr.asset_id,
            link_mode = "dynamic_loading_bundle",
            file = libraries[0],
            uri = "",
        ),
    ]

code_asset_as = rule(
    implementation = _code_asset_as_impl,
    doc = "Binds `library` to `asset_id`, so a test can load a second library " +
          "beside the one a bridge's own id resolves to.",
    attrs = {
        "asset_id": attr.string(mandatory = True),
        "library": attr.label(mandatory = True),
    },
    provides = [DartCodeAssetInfo],
)
