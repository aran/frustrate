# TODO

Next steps that are out of scope for the work that found them or wait on a
decision. Remove an item when it lands. Defects found while working are fixed,
not listed here.

## Release

The first release publishes no changelog.

Before pushing `main`:

- Once the pins below move, refresh every workspace's lock to the published
  pins: `bazel run --lockfile_mode=off @overhead//locks:refresh`, which
  leaves `.bazelrc.user` overrides out. The e2e demos get their first locks
  this way: until then they need rules_dart's unreleased `pub.from_lock`
  `resolve`, so no lock of theirs can resolve against published pins.
- Move the `rules_dart` pin to a release that has
  `@rules_dart//dart/constraints:web_compiler`: `Vec<i64>` and `[i64; N]` choose
  their Dart type by it.
  `runtime/dart`'s `runfiles` dev dependency moves to the same version.
- Move the `rules_flutter` pin to a release that builds its web builds for a
  platform with `@rules_dart//dart/constraints:js` or `:wasm`, builds
  Android APKs for `--@rules_android//flags:android_platforms`, and has the
  `@rules_flutter//flutter/pub:extensions.bzl` `pub` extension the e2e demos
  use.
- Move the `overhead` pin (`MODULE.bazel` and the workflows) to the rules_dart
  commit that was pushed, if it differs.

Not part of this push: prebuilt custom-std archives on a GitHub release, so
`wasm32_threads` and the browser-std platform build with no local step. The
branch that builds and fetches them waits on that release. Building your own
std with `bazel run //toolchain/custom_std:build` stays supported either way.

