"""An analysis test of what a `frustrate_code_asset` says its dSYM is made from."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_dart//dart:providers.bzl", "DartCodeAssetInfo")

def _code_asset_debug_info_test_impl(ctx):
    env = analysistest.begin(ctx)
    asset = analysistest.target_under_test(env)[DartCodeAssetInfo]
    asserts.true(env, asset.link_inputs_gone, "a Rust library's link inputs are gone")
    asserts.equals(env, None, asset.link_inputs)
    if ctx.target_platform_has_constraint(ctx.attr._macos[platform_common.ConstraintValueInfo]):
        asserts.true(env, asset.dsym != None, "macOS: rustc's dSYM is the asset's")
        if asset.dsym != None:
            asserts.equals(env, asset.file.basename + ".dSYM", asset.dsym.basename)
            asserts.equals(env, asset.file.dirname, asset.dsym.dirname)
    else:
        asserts.equals(env, None, asset.dsym)
    return analysistest.end(env)

code_asset_debug_info_test = analysistest.make(
    _code_asset_debug_info_test_impl,
    attrs = {"_macos": attr.label(default = "@platforms//os:macos")},
)
