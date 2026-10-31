"""An analysis test that a target pairing a bridge with another crate fails to analyze."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")

def _mispaired_bridge_test_impl(ctx):
    env = analysistest.begin(ctx)
    asserts.expect_failure(env, "generated bindings for crate `compose_beta`, but the library it is paired with")
    return analysistest.end(env)

mispaired_bridge_test = analysistest.make(
    _mispaired_bridge_test_impl,
    expect_failure = True,
)
