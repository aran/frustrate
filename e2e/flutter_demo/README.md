# frustrate Flutter demo — running it

One portable app (`lib/main.dart`) with its state and logic in Rust
(`bridge/src/api.rs`), reached through generated bindings. Everything is built
by Bazel — the shipped bundles, the tests, and the development loop alike.

## The development loop

```sh
bazel run @rules_flutter//tools/dev_tool:flutter_bazel -- run -t //:app_web -d chrome
```

That is the whole thing. It builds `//:app_web`, serves it, launches Chrome and
attaches DWDS, so hot reload and hot restart cover every Dart edit in `lib/`
and `bridge/lib/` alike. [rules_flutter](https://github.com/aran/rules_flutter)
documents how to drive them.

The loop compiles your Dart with DDC, to JavaScript — that is what hot reload
runs on — not with the dart2wasm the shipped bundle uses. There `int` is a
53-bit JavaScript number and `Vec<i64>` cannot cross, so the collections card
says it is unavailable. `--wasm` runs the dart2wasm build instead, where a
reload and a restart both rebuild and reload the page, losing its state.

**On the web, a change under `bridge/src/` needs a hot restart.** Both rebuild
through Bazel, so the regenerated bindings and the rebuilt module are current on
disk either way — but only a restart reaches the page. Hot reload never re-runs
`main()`, so the page keeps the wasm instance it already has. If the edit
changed the bindings, injecting them over that instance would call a module that
cannot serve them, so the reload refuses, naming the module and the contract it
was generated from, and compiles and sends nothing. If it changed only a
function body, the reload applies your Dart and says the rebuilt module is not
running yet. Either way, restart, which resets Dart's statics so `initBridge()` runs as a
first init and loads the rebuilt module.

Both answers are what `native_modules` buys. Each web bundle names its module
through a `flutter_native_library` wrapper carrying `//bridge:codegen.interface` as the
`binding_contract`, which is the only way the dev tool can tell a module apart
from Flutter's own `main.dart.wasm` (which moves on every Dart edit). A module
listed in `web_assets` instead is served identically and watched not at all, so
a reload would report success over an instance that cannot serve what it
injected.

**On a native device a hot reload delivers a Rust edit into the running app.** A process
cannot replace a library it has `dlopen`ed, so instead the edit is compiled into
a patch — a second library holding the functions that changed — and the running
library's entry points are redirected into it. State is kept: a `Counter` handle
goes on counting from where it was, and the statics behind `set_log_sink`
keep their sinks. `bridge/BUILD.bazel` asks for this by wrapping the library in
`frustrate_hot_patchable`, binding the result as the bindings' code asset with
`frustrate_code_asset`, and wrapping that in `flutter_native_library` with the
patch builder as `hot_patch`; `codegen.interface` — the interface both halves are
generated from — is its binding contract.

Only a `-c dbg` build is patchable, which is what `flutter_bazel run` builds.
Adding a bridged function, renaming one, or changing its signature is carried:
a patch installs whole new dispatch tables, and a member's dispatch id comes
from its own wire facts, so the members that did not change keep the ids their
callers hold.

What a patch cannot carry, the reload says so and withholds, naming the cause:
a bridged *type* whose shape changed (every member using it keeps its id while
its encoding moves), a new or changed opaque type (Dart binds its drop to the
library it loaded), a type whose layout moved under values the running code
built, a new static, a changed dependency, or a call to code the launch build
never linked, among others. A restart relaunches on the new library. Work already in flight keeps
the code it started with — an actor job, an executor task, a closure Rust is
holding — the same way a Dart closure created before a reload does.

Nothing is staged into the source tree, and nothing needs to be. The dev server
serves `demo_rust.wasm` and `frustrate.js` straight out of `bazel-bin`, which is
what makes "the loop and the shipped bundle ran the same bytes" true by
construction rather than by remembering to re-run something.

Other devices work the same way — `-d macos`, `-d ios-simulator` — swapping
`-t` for that platform's target (`//:app`, `//:app_ios`, and `//:app_linux` on a Linux host).
Android is `-t //:app_android -d android --build-arg=--config=android`: the
android config is what registers the SDK and NDK toolchains (`.bazelrc` says
why), and `.bazelrc.user` must point `ANDROID_HOME` and `ANDROID_NDK_HOME` at
this machine's SDK and *versioned* NDK directory. Every other target needs
neither.

A physical iPhone is `-t //:app_ios_device -d ios`, which needs a provisioning
profile: copy `device.example/` to `device/` and follow its header. `device/` is
gitignored, and the wildcard build never reaches the target, so a checkout
without it is not broken — just simulator-only.

`flutter_bazel run --help` lists the rest; the ones that
matter here are `--web-header NAME=VALUE` and `--cross-origin-isolation`, which
are how you reach the threaded build's COOP/COEP requirement from the loop.

## Changing dependencies

`pubspec.lock` is Bazel's pin: `MODULE.bazel` reads it with
`flutter_pub.from_lock`, mapping the path packages (`demo_bridge`, `frustrate`)
to the targets that build them with `resolve`. After editing `pubspec.yaml`,
regenerate the lock against the SDK Bazel pins with
`bazel run @rules_flutter//flutter/pub -- get`.

That same target is also how you get a `.dart_tool/package_config.json` for the
**editor**: the Dart analyzer resolves `package:demo_bridge/…` from the source
tree, so without one your IDE shows unresolved imports even though every build
and test passes. It is an editor convenience, not a build input — the loop above
needs none of it.

## Building the same app with Bazel

```sh
bazel build //:app_web     # web bundle, dart2wasm, strict CSP
bazel build //:app         # macOS .app
bazel build //:app_ios     # iOS bundle (add -c dbg for a simulator build)
bazel build //:app_linux   # Linux GTK (on a Linux host)
bazel test  //...          # includes the analyze + build tests
```

These are the same targets the loop above builds; running one directly just
skips the serve-and-attach half.

### Checking that the bridge answers on a device

```sh
bazel run //:smoke -- -t //:app_ios -d ios-simulator
bazel run //:smoke -- -t //:app_android --build-arg=--config=android -d android
```

`//:smoke` runs the gallery on the device and waits until the results of a
sync and an async Rust call are on screen. CI runs it on an iOS simulator and an
Android emulator; the same browser and native checks without a device are
`bazel run //tests/dart_integration:web_test -- test/smoke_test.dart` (and
`bazel test //tests/dart_integration:smoke_test`) in the root module.

### The two other web builds

Same `lib/main.dart`, same bridge crate — only the wasm module differs. Both
are `manual` because each needs a std built locally first, in the frustrate
repo, and `//...` must stay green without one:

```sh
bazel run //toolchain/custom_std:build                     # +atomics
bazel build //threaded:app_web_threaded                       # shared memory, COOP/COEP

bazel run //toolchain/custom_std:build -- --facilities=clock,random,stdio,thread
bazel build //custom_std:app_web_custom_std                   # a working std::time, HashMap, println!
```

`//threaded` buys parallelism and costs cross-origin isolation.
`//custom_std` keeps the default build's shape — single-threaded, no COOP/COEP
— and instead replaces std's unsupported stubs with calls to the host, so
`std::time::Instant`, `SystemTime`, `HashMap` seeding, `println!`,
`available_parallelism` and `thread::sleep` work in crates that never heard of
wasm. The gallery's last card reports all five on whichever build you are
running.

Either runs under the loop the default app does — `flutter_bazel run -t
//custom_std:app_web_custom_std -d chrome`, or for the threaded one `-t
//threaded:app_web_threaded -d chrome --cross-origin-isolation`, since its
memory is a SharedArrayBuffer — once its std is built, and gets the same
withheld-reload-on-a-Rust-edit behaviour, because each names its module through
its own `flutter_native_library` wrapper.

Each has a Playwright spec beside `web.spec.js`; run one with
`cd playwright && npx playwright test browser_std.spec.js`.

## Notes

- The Bazel web bundle carries a strict Content-Security-Policy, injected into
  the *bundle's* copy of the page (`web_csp.bzl`) in every mode but `-c dbg`.
  The dev loop builds `-c dbg` and serves that page, and dwds — the hot-restart
  channel — starts the app with an inline script that any policy without
  `'unsafe-inline'` would block. The checked-in `web/index.html` carries none,
  so `flutter run -d chrome` can serve it too.
- The dev loop logs 404s for `favicon.png` and `manifest.json`. Both are
  cosmetic: Bazel generates the manifest for the shipped bundle
  (`//:manifest_json`) and the loop has no use for either.
