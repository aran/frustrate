# frustrate Flutter demo — running it

One portable app (`lib/main.dart`) with its state and logic in Rust
(`bridge/src/api.rs`), reached through generated bindings. Everything is built
by Bazel — the shipped bundles, the tests, and the development loop alike.

## The development loop

```sh
bazel run @rules_flutter//tools/dev_tool:flutter_bazel -- run -t //:app_web -d chrome
```

That is the whole thing. It builds `//:app_web`, serves it, launches Chrome and
attaches DWDS, so hot reload (`r`) and hot restart (`R`) cover every Dart edit
in `lib/` and `bridge/lib/` alike.

The loop compiles your Dart with DDC, to JavaScript — that is what hot reload
runs on — not with the dart2wasm the shipped bundle uses. There `int` is a
53-bit JavaScript number and `Vec<i64>` cannot cross, so the collections card
says it is unavailable. `--wasm` runs the dart2wasm build instead, with restart
but no reload.

**On the web, a change under `bridge/src/` needs `R`.** Both keys rebuild
through Bazel, so the regenerated bindings and the rebuilt module are current on
disk either way — but only a restart reaches the page. Hot reload never re-runs
`main()`, so the page keeps the wasm instance it already has. If the edit
changed the bindings, injecting them over that instance would call a module that
cannot serve them, so `r` refuses, naming the module and the contract it was
generated from, and compiles and sends nothing. If it changed only a function
body, `r` reloads your Dart and says the rebuilt module is not running yet.
Either way, press `R`, which resets Dart's statics so `initBridge()` runs as a
first init and loads the rebuilt module.

Both answers are what `native_modules` buys. Each web bundle names its module
through a `flutter_native_library` wrapper carrying `//bridge:codegen.ir` as the
`binding_contract`, which is the only way the dev tool can tell a module apart
from Flutter's own `main.dart.wasm` (which moves on every Dart edit). A module
listed in `web_assets` instead is served identically and watched not at all, so
`r` would report success over an instance that cannot serve what it injected.

**On a native device `r` delivers a Rust edit into the running app.** A process
cannot replace a library it has `dlopen`ed, so instead the edit is compiled into
a patch — a second library holding the functions that changed — and the running
library's entry points are redirected into it. State is kept: a `Counter` handle
goes on counting from where it was, and the statics behind `install_logging`
keep their sinks. `bridge/BUILD.bazel` asks for this by wrapping the library in
`frustrate_hot_patchable` and naming the result as `flutter_native_library`'s
`library` and `hot_patch`; `codegen.ir` — the interface both halves are
generated from — stays its binding contract.

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
never linked. `R` relaunches on the new library. Work already in flight keeps
the code it started with — an actor job, an executor task, a closure Rust is
holding — the same way a Dart closure created before a reload does.

Nothing is staged into the source tree, and nothing needs to be. The dev server
serves `demo_rust.wasm` and `frustrate.js` straight out of `bazel-bin`, which is
what makes "the loop and the shipped bundle ran the same bytes" true by
construction rather than by remembering to re-run something.

Other devices work the same way — `-d macos`, `-d ios-simulator` — swapping
`-t` for that platform's target (`//:app`, `//:app_ios`, `//:app_linux`).
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

## Do not run `flutter pub get` by hand here

`pubspec.lock` is **Bazel's** pin, not pub's scratch space: `MODULE.bazel` reads
it with `flutter.pub(lock = "//:pubspec.lock")` to decide which hosted packages
`@deps` is built from. The two path dependencies in `pubspec.yaml`
(`demo_bridge`, `frustrate`) mean a bare `flutter pub get` would add three
entries to that file — the two path deps and `ffi`.

If you do dirty the lock, restore it (`git checkout -- pubspec.lock`). To change
the app's *hosted* dependencies, edit `pubspec.yaml` and regenerate the lock
with the SDK Bazel pins — `bazel run @rules_flutter//flutter:pub -- get` — then
drop the path and `ffi` entries pub adds, since Bazel takes those from
`BUILD.bazel` instead.

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
`cd playwright && npx playwright test std_facilities.spec.js`.

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
