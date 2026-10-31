# frustrate

**This is an experimental project. Use
[flutter_rust_bridge](https://github.com/fzyzcjy/flutter_rust_bridge) instead.**
This project is for integrating Flutter code with Rust code in hermetic Bazel.
Only a tiny number of projects need this. If you want to use
flutter_rust_bridge under Bazel, [`bazel_frb/`](bazel_frb/README.md) is a
working example of how. Note that this route is not hermetic (the codegen needs
host-installed tools, so caching is unsound and remote execution is out);
frustrate is built to avoid that.

frustrate provides Bazel rules and glue code for building Flutter apps that
interoperate with Rust code. It is designed for the kind of project that is
likely to use a build system like Bazel: projects focused on scalable
engineering, correctness, and performance. frustrate delivers pay-as-you-go
complexity and focuses just on core infrastructure for Flutter/Rust integration.
Together with rules_flutter, frustrate allows you to use hot reload and hot
restart for Rust code in a Flutter app.

A Flutter app that uses Rust code must solve problems including data sharing and
serialization, object lifecycle management, and alignment across differences in
language semantics and conventions.

In other words, Dart and Rust are very different, and there are a handful
of problems to solve to get a convenient integration.

To use `frustrate`, you annotate your Rust code a little, opt in to
anything dangerous you are doing, and set up Bazel build rules. Then
your Rust and Dart code work safely and performantly together.

To this end `frustrate` provides:

**Data across the boundary.** Scalars, `String`, byte buffers and typed lists,
`Vec`/`HashMap`/`HashSet`/`BTreeMap`/`VecDeque`/`Option` and their nestings,
named structs, tuples as Dart records, and data-carrying enums as sealed Dart
classes. A data type may be **generic** — `struct Page<T>`. `u64`/`i128`/`u128`
cross as `BigInt` over their full range rather than changing type by magnitude.
Durations and timestamps map to Dart's, with `chrono` and `time` selectable on
the Rust side. Anything without a first-class mapping crosses through a declared
bytes codec (e.g. protobuf) you supply. Generated data classes get value
equality, `copyWith` and `toString`.

**Objects through handles, built from data and calls.** A handle adds no
capability of its own. Rust keeps the object in a table, an id crosses as data,
and a generated Dart class turns each method into a call that carries the id
and frees the object through `dispose()` or a `Finalizer`. Generating it gets
lifetimes, threads and consumption right where hand-written ids would not. The
model you declare picks how the object is held, matched to the complexity of
your scenario:

- **Confined** — `Box<T>`, one owner, used from one isolate. Requires `Send`.
  Sync methods run on the caller; no async.
- **Resident** — `Box<T>` with no thread bound at all, pinned to the OS thread
  that built it. Every acquisition checks that thread. Sync only.
- **Frozen** — `Arc<T>`, immutable after construction. Requires `Send + Sync`.
  Both `&self` sync methods and async, and several threads can read it at once.
- **Locked** — `Arc<RwLock<T>>`, shared mutable. Requires `Send + Sync`. Async
  by default; a sync method needs a declared `on_contention` contract. `error`
  (try-lock, throws) works everywhere; `block` (waits to acquire) is
  native-only, because a sync method runs in the caller's frame and on web that
  is always the main thread.
- **Actor** — no wrapper; the object lives on its own executor (an OS thread
  natively, a Worker on web) and is reached only by message. No bound on `T`;
  its methods are plain Rust `fn`s (or return `Deferred<T>`) and are always
  `async` on the Dart side.

**Object lifecycle.** Live Rust objects cross as opaque handles with methods,
freed by `dispose()`. A Rust method that takes `self` by value is called from
Dart through `take()`, which consumes the handle: `doc.take().finish()`.

**Concurrency on web.** The default web build is stable Rust, single-threaded
wasm. Parallelism comes from placing an object in a worker. Multithreaded wasm,
via Web Workers, is opt-in.

**Calls in both directions.** Rust → Dart streams with backpressure and
non-terminal errors, Dart → Rust callbacks that can be stored and can fail with a
typed error, trait objects across the boundary, and `Result<T, E>` as typed Dart
exceptions.

**Rust `async fn` on every target**, including single-threaded stable wasm, where
concurrent calls multiplex cooperatively on one thread. Calls in flight can be
cancelled.

**Seams for cross-cutting concerns.** A generated typed fake so Dart tests run
without loading a library, a decorator that sees every call, a Rust panic
listener for crash reporting, a declarable call-pool width, and Rust `log`
records delivered to Dart as a stream.

**One declared Bazel target.** Upholds Bazel's promise: "Fast, correct. Choose
two." codegen runs as one action with declared inputs and hermetic outputs; no
cargo or dart build hooks in the build. Bazel gives you lots of knobs to speed up
developer loops and CI. With this project you can use them.

For more detail on how this works, consult the test suite
([`tests/test_api/src/api.rs`](tests/test_api/src/api.rs) for the bridged surface,
[`tests/dart_integration/test/`](tests/dart_integration/test/) for what it
guarantees) and the [e2e examples](e2e/).

## Depending on frustrate from git

To use a commit of frustrate that is not a registry release, add a
`git_override` to your root `MODULE.bazel`, next to the `bazel_dep`:

```starlark
bazel_dep(name = "frustrate", version = "0.1.0")
git_override(
    module_name = "frustrate",
    remote = "https://github.com/aran/frustrate",
    commit = "<sha>",
)
```

Pin a commit, not a branch. Bazel honors overrides only in the root module, so
this belongs in your own `MODULE.bazel` and not in a module that depends on
yours. To build against a local clone instead, pass
`--override_module=frustrate=/path/to/clone`, in a `.bazelrc.user` if it should
reach nested `bazel` invocations.

## Getting started

Start with [`e2e/wasi_demo`](e2e/wasi_demo/), the smallest complete app, in this
order:

| File | What it shows |
|---|---|
| [`bridge/src/api.rs`](e2e/wasi_demo/bridge/src/api.rs) | the Rust you annotate with `#[bridge]` |
| [`bridge/BUILD.bazel`](e2e/wasi_demo/bridge/BUILD.bazel) | `frustrate_bridge_outputs` + a `frustrate_bridge_library` over its output |
| [`lib/main.dart`](e2e/wasi_demo/lib/main.dart) | calling the generated bindings from Flutter |
| [`BUILD.bazel`](e2e/wasi_demo/BUILD.bazel) | wiring the bridge into a Flutter app |
| [`MODULE.bazel`](e2e/wasi_demo/MODULE.bazel) | a working `bazel_dep`/toolchain pin set to copy versions from |

**Bridge sources are a list.** `srcs` and `module_paths` are parallel: each pair
is a file to parse and the Rust path its items are reachable by from the bridge
crate. The demos declare a single pair because one file covers them, not because
the tool takes one.

**The bridge library is a Dart code asset.** `package_name` is the Dart package
the bindings compile into. The generated `<crate>Bridge` (`myBridgeBridge` for
crate `my_bridge`) resolves the library by the asset id `package:<package_name>/<crate>`,
and a `frustrate_code_asset` binds that id to the built library:

```starlark
frustrate_code_asset(
    name = "bridge_asset",
    bridge = ":codegen",
    library = ":my_bridge",
)

dart_library(
    name = "bindings",
    srcs = [":codegen.dart"],
    code_assets = [":bridge_asset"],
    package_name = "my_app",
)
```

A `dart_test` or `dart_binary` depending on `:bindings` gets the library built
for the host it runs on, and an app bundler (rules_flutter) ships it per
platform. In Dart, `FrustrateNative.init(myBridgeBridge)` once, then call the
bindings. `BridgeLibrary.dynamic(DynamicLibrary.open(path), name: path,
bridge: frustrateBridge)` is the way to load a library you built yourself.

**Several bridges, one library.** An isolate (a page, on web) runs one Rust
runtime, so bridges an app uses together are linked into one library.
`frustrate_library` does that, keeping each bridge its own crate, bindings and
asset id, and declares a code asset per bridge that binds its id to the
combined library. Each key is a bridge's Rust library (the `<name>_lib` rlib
`frustrate_bridge_library` declares), each value the `frustrate_bridge` its
glue came from; a pair naming two different crates fails at analysis:

```starlark
frustrate_library(
    name = "app_rust",
    bridges = {
        "//main:main_rust_lib": "//main:codegen",
        "//compute:compute_rust_lib": "//compute:codegen",
    },
)

dart_binary(
    name = "dt",
    code_assets = [":app_rust.main_rust_lib", ":app_rust.compute_rust_lib"],
    ...
)
```

`FrustrateNative.init` each bridge's library: the first builds the transport,
the rest attach to it. A library built separately is refused. On web,
`frustrate_wasm_module(crate = ":app_rust")` builds the one module, and
`FrustrateWeb.init` serves every bridge in it. Generated code reaches its
runtime with `Frustrate.of(frustrateBridge)`; `Frustrate.instance` answers
only while one bridge is active, and `Frustrate.activate` replaces only the
bridge its runtime serves, so a test can fake one bridge and keep another real.

A source need not live in the bridge crate. A domain crate can carry
`#[bridge(...)]` on its own types and be listed directly:

```starlark
frustrate_bridge_outputs(
    name = "codegen",
    srcs = ["//rust/core:src/model.rs", "src/api.rs"],
    crate_name = "my_bridge",
    module_paths = ["my_core::model", "crate::api"],
    package_name = "my_app",
)
```

The attributes are inert and no generated code is compiled into the domain crate,
so the only cost is making `#[bridge(...)]` resolve for rustc. Bazel supplies it:
`proc_macro_deps = ["@frustrate//macros:frustrate_macros"]` on that crate's
`rust_library`, over the `bazel_dep` on `frustrate` the bridge already declares.

**Two bridges can share data types.** A bridge that lists another in `models`
takes that bridge's plain data types (structs and enums that reach no handle)
instead of generating its own Dart copies, so one Dart object crosses both:

```starlark
frustrate_bridge_outputs(
    name = "codegen",
    srcs = ["//rust/core:src/model.rs", "src/compute.rs"],
    crate_name = "my_compute",
    models = ["//bridge:codegen"],
    module_paths = ["my_core::model", "crate::compute"],
    package_name = "my_compute",
)
```

List the shared source in `srcs` as before; a struct or enum declared under the
name of one in a `models` bridge is that bridge's. The generated bindings import
its class and re-export it, so the bindings' `dart_library` must depend on the
other bridge's. Codegen refuses the pair when the two declarations are different
Rust items or have different shapes, and refuses a shared type that reaches a
handle, since a handle belongs to the bridge that minted it. Members of a shared
type stay on its home bridge.

Your `MODULE.bazel` needs a `bazel_dep` on `frustrate`, `rules_flutter`,
`rules_dart` and `rules_rust` (see the demo module above for versions known to
work together). Until frustrate is on BCR, you'll need to use a git or local
path override as well.

```starlark
# Your bridge crate's own cargo dependencies. frustrate's hub resolves
# frustrate's manifests only, so a bridge crate with dependencies needs one of
# its own.
crate = use_extension("@rules_rust//crate_universe:extensions.bzl", "crate")
crate.from_cargo(
    name = "my_crates",
    cargo_lockfile = "//bridge:Cargo.lock",
    manifests = ["//bridge:Cargo.toml"],
)
use_repo(crate, "my_crates")
```

If your bridge forwards Rust `log` records to Dart, also point frustrate's `log`
at your hub, so its runtime compiles against the same one your dependencies
linked:

```
# .bazelrc
common --@frustrate//runtime/rust:log=@my_crates//:log
```

If your Linux builds use a C toolchain whose `libgcc_s` is a stub, as BCR's
`llvm` module's is, point frustrate's `unwinder` at that toolchain's static
libunwind. Otherwise a Rust shared library leaves `_Unwind_*` undefined and
fails to load:

```starlark
# my/cc/BUILD.bazel
cc_import(
    name = "libunwind",
    static_library = "@llvm//runtimes/libunwind:libunwind.static",
)
```

```
# .bazelrc
common --@frustrate//runtime/rust:unwinder=//my/cc:libunwind
```

### Hot reload and hot restart for Rust

Flutter has two moves to support a rapid inner development loop: hot reload
(`r`) swaps edited Dart code into the running app and keeps its state, and hot
restart (`R`) starts the app over.
Under `flutter_bazel run`, Rust edits get the same two moves.

**Hot reload** compiles the edited Rust into a small patch library and redirects
the running library's bridged entry points into it. Rust state survives: a
counter behind a handle keeps counting from where it was. Adding, removing,
renaming or re-signing a bridged function reloads too, and a caller still
holding the old function can never land on a different one. Calls already in
flight finish on the code they started with, as a Dart closure created before a
reload does.

Some edits cannot be patched into a running process. The reload refuses them,
names the reason, and you press `R` instead:

- a bridged data type whose shape changed
- a new or changed opaque type
- a changed layout of a type the running code already holds values of
- a new `static`
- a changed dependency

**Hot restart** relaunches the app on the rebuilt library, so Rust state starts
fresh along with Dart's.

**From an agent or script.** A tool driving `flutter_bazel run --machine` — a
coding agent, an IDE, a test harness — sends `app.hotReload` and `app.restart`
instead of pressing keys, over the JSON-RPC stream or the HTTP control channel.
Machine mode does not reload on save, so an agent can finish an edit across
several files and then reload once. The `app.hotReload` reply says whether the
Rust edit is live, so the tool needn't guess from the screen:

- Patched: `succeeded` is true, and `nativePatched` names each library and the
  functions the patch replaced.
- Not patchable: `succeeded` is false, `nativePatchRestart` carries the reasons
  per library, and `runningCode` is `unchanged` — the Dart half of the edit was
  held back too, so the app is still running what it had. Send `app.restart`.
- A patch that fails to build or load also comes back with `succeeded` false and
  the Dart half unsent.

The protocol, and how to set an agent up to use it, is in rules_flutter's
[dev tool docs](https://github.com/aran/rules_flutter#driving-the-app-from-a-script).

Hot reload needs a native target and a `-c dbg` build. Opt in by wrapping the
bridge library:

```starlark
load("@frustrate//bazel:hot_patch.bzl", "frustrate_hot_patchable")

frustrate_hot_patchable(
    name = "my_bridge_hot",
    binding_contract = ":codegen.ir",
    library = ":my_bridge",
)

flutter_native_library(
    name = "my_bridge_native",
    binding_contract = [":codegen.ir"],
    hot_patch = ":my_bridge_hot.hot_patch",
    library = ":my_bridge_hot",
)
```

Then name `my_bridge_native` in the plugin's `native_deps`, in place of the
library itself.

For a `frustrate_library`, pass the same `bridges` dict instead of a contract.
That also declares each bridge's code asset over the patchable library
(`<name>.<library>`), and every bridge's wrapper names the one patch builder:

```starlark
frustrate_hot_patchable(
    name = "app_rust_hot",
    bridges = BRIDGES,  # what frustrate_library(name = "app_rust") was given
    library = ":app_rust",
)

[flutter_native_library(
    name = "app_rust_native." + bridge,
    binding_contract = ["//%s:codegen.ir" % bridge],
    hot_patch = ":app_rust_hot.hot_patch",
    library = ":app_rust_hot.%s_lib" % bridge,  # the bridge's Rust library's target name
) for bridge in ("ping", "pong")]
```

List those wrappers in the app's `code_assets`. One patch carries an edit to any
of the bridge crates.

A patchable debug library is bigger than a plain one, because it keeps
functions the app never called so that a patch can call them. Release builds
are untouched: outside `-c dbg` the wrapper is the plain library, byte for byte.

## Choosing a web platform

Rust compiled for a browser has to pick a target. It's complicated for
fundamental reasons. Here's a decision procedure for the options `frustrate`
supports. Ask these in order; the first yes is your answer.

**Does your Rust reach a browser API?** `fetch`, WebSocket, WebRTC, IndexedDB,
the DOM — or a crate that reaches them from inside its own manifest. Use
`@frustrate//bazel:wasm32` with a version-matched `wasm-bindgen` CLI, which you
supply as a label because the version belongs to your lockfile. Before answering
yes: Rust can call Dart, so if the browser API is only being reached because the
Rust side happens to own that step, moving the step to Dart drops the whole
wasm-bindgen dependency. This is what we do for new code, or code under our
control. Keep it in Rust when a crate you want declares the dependency
structurally and no feature flag gets you out. And if what you need is a few JS
functions you write yourself, [host imports](#calling-your-own-javascript-from-rust)
reach them from any web build, wasip1 included, without wasm-bindgen.

**Does your Rust need std facilities a browser has no host for?** Reading a clock
(`SystemTime::now`, `Instant::now`), entropy — and so `rand`, `uuid`, `ring` —
`println!`, `std::env`. Note this is about *reading* a clock: `chrono` and `time`
span types are bridged peers that compile on the plain target, so Rust that
receives its instants from Dart needs nothing here. Two answers, and they are
different trades rather than better and worse. `@frustrate//bazel:wasm32_wasi`
supplies a host for all of it and costs module size.
`@frustrate//bazel:wasm32_custom` keeps the plain target, but you build your
own `std`. We have code to help, but it's more work.

```sh
bazel run //toolchain/custom_std:build -- --facilities=clock,random,stdio,thread
```

Take wasi when your graph wants a lot of std; take a facility when it wants one
thing and you cannot give up the target.

**Neither?** `@frustrate//bazel:wasm32`, plain — the default. Smallest module,
stable Rust, no post-pass.

**The first two are mutually exclusive.** "A clock and a WebSocket" is an
ordinary thing to want and you cannot have both: a wasi target cannot also take
the browser-API path. Plan around it rather than discovering it.

**Parallelism.** `@frustrate//bazel:wasm32_threads` is the plain target plus
atomics and a shared memory. It needs a nightly toolchain and a locally built
std. If you want parallelism, you can have it, but it's slightly more work and
risk.

### Calling your own JavaScript from Rust

A bridge module reaches JavaScript through import namespaces. The runtime
supplies `frustrate`, `env` and `wasi_snapshot_preview1`; every other namespace
comes from an ES module your app serves beside the `.wasm`. Declare the
functions in Rust:

```rust
#[link(wasm_import_module = "my_files")]
extern "C" {
    fn read(buf: *mut u8, cap: usize) -> i32;
}
```

Declare the namespace on the module, so the build accepts it (an import from
any namespace that is not the runtime's or declared fails the build):

```starlark
frustrate_wasm_module(
    name = "app.wasm",
    crate = ":app_rust",
    host_imports = ["my_files"],
)
```

Write the module, whose exports are the namespace's functions, and name its URL
at init:

```js
// my_files.js
let memory, handle;

export async function attach({ exports, memory: m, actor }) {
  memory = m;
  if (actor?.bridge === 'app_rust' && actor.type === 'Core') {
    const root = await navigator.storage.getDirectory();
    const file = await root.getFileHandle('db', { create: true });
    handle = await file.createSyncAccessHandle();
  }
}

export function read(ptr, cap) {
  if (!handle) return -1; // unsupported in this realm
  return handle.read(new Uint8Array(memory.buffer, ptr, cap), { at: 0 });
}
```

```dart
await FrustrateWeb.initFromUrl('app.wasm',
    hostImports: {'my_files': 'my_files.js'});
```

Every realm that instantiates the module imports its own copy of the JS module:
the page, each actor's Worker, and each pool worker on threaded wasm. Module
state is per realm and never shared. The optional `attach` runs on each
instance after instantiation and before any of the bridge's code, and is
awaited if it returns a promise. `exports` and `memory` are that instance's.
`actor` is `{bridge, type}` for the actor the instance serves, its bridge's Rust
crate name and its Rust type name, and null on the page and in pool workers. Rust calls the functions synchronously, so an actor's
Worker is where a synchronous browser API such as `FileSystemSyncAccessHandle`
is available.

wasm-bindgen's generated JS is a host module too. Its post-pass names the one
namespace it supplies `./<stem>_bg.js`; `bindgenGlueUrl` names the JS for that
namespace, and its attach step is wasm-bindgen's own start-up handshake.
