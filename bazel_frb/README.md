# flutter_rust_bridge under Bazel

A working Bazel module that runs flutter_rust_bridge's codegen as a sandboxed,
network-blocked action, with a vendored registry and its generated Dart compiled
by rules_dart. It is standalone — its own `MODULE.bazel`, its own cargo
workspace — and it is in `.bazelignore`, so it never enters this repo's build
graph.

It exists because "can FRB's codegen be a Bazel action?" is a question people
ask and there was no published answer. The answer is **yes**, and this is the
recipe.

## What it establishes

FRB's codegen *can* be a Bazel action. What it cannot be is a **hermetic** one:
the action needs the host's `PATH` and `HOME`, two `cargo install`ed binaries
(`flutter_rust_bridge_codegen` and `cargo-expand`, the second of which FRB will
try to install mid-action if it is missing), a Dart SDK, and a Rust toolchain.
None of those come from the graph, so the cache key is unsound and remote
execution is out. It also re-does the Rust build, because `cargo expand` needs
the crate to compile.

Two things that sound true here and are not:

- **Bazel's local sandbox does not default to no network.**
  `--sandbox_default_allow_network` defaults to *true*; networkless is the
  default shape of *remote* execution.
- **FRB's codegen does not fail merely from having no network.** The failure
  people hit is a cold `CARGO_HOME`, which produces `cargo expand returned
  empty output`. A warm `~/.cargo` succeeds under `block-network`, and a
  vendored tree succeeds with both.

FRB is built around Flutter's standard toolchain rather than Bazel, and its
*build* side shows it: it runs through cargokit inside a CocoaPods
`script_phase` / Gradle / CMake hook, which writes a pubspec into a temp dir and
runs `dart pub get` at build time. Bazel cannot sandbox that step or see it in
its cache keys, so this module does not attempt it.

## Layout

```
bridge/           the FRB bridge crate — one hand-written api module
codegen_action/   the genrule ladder that runs frb_codegen, plus vendor config
lib/              FRB's generated Dart, checked in and compiled by rules_dart
test/             a smoke test that loads the cdylib and calls through it
```

The decisive rung is `//codegen_action:codegen_offline_vendored_blocked`:
sandboxed, `block-network` tagged, cold `CARGO_HOME`, dependencies vendored and
declared as inputs, byte-identical output.

```sh
cd bazel_frb
bazel build //codegen_action:codegen_offline_vendored_blocked \
    --action_env=FRB_HOST_HOME=$HOME --action_env=FRB_HOST_PATH="$PATH"
```

## Formatting inside the sandbox

FRB treats formatting as best-effort: when `rustfmt` cannot load its dylib
inside the sandbox, it logs a warning and completes, and the output differs in
whitespace and import order. `dart fix` and `dart format` behave the same way in
the sandboxed rungs. Outside Bazel that is a sensible default; inside it, the
output bytes depend on which host formatters loaded, so a Bazel wrapper should
make sure they can load (or treat the warning as a failure) before caching the
result.
