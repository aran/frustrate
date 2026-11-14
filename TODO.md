# TODO

Next steps that are out of scope for the work that found them or wait on a
decision. Remove an item when it lands. Defects found while working are fixed,
not listed here.

## Release

The first release publishes no changelog.

- Publish prebuilt custom-std archives on a GitHub release, so
  `wasm32_threads` and the browser-std platform build with no local step. The
  branch that builds and fetches them waits on that release. Building your own
  std with `bazel run //toolchain/custom_std:build` stays supported either way.

