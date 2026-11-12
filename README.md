# frustrate

**This is an experimental project. Use
[flutter_rust_bridge](https://github.com/fzyzcjy/flutter_rust_bridge) instead.**
This project is for integrating Flutter code with Rust code in hermetic Bazel.
Only a tiny number of projects need this. If you want to use
flutter_rust_bridge under Bazel, [`bazel_frb/`](bazel_frb/README.md) is a
working example of how. Note that this route is not hermetic (the codegen needs
host-installed tools, so caching is unsound and remote execution is out);
frustrate is built to avoid that.

### What is `frustrate`?

frustrate provides Bazel rules and glue code for building Flutter apps that
interoperate with Rust code. It is designed for the kind of project that is
likely to use a build system like Bazel: projects focused on scalable
engineering, correctness, and performance. frustrate delivers pay-as-you-go
complexity and focuses just on core infrastructure for Flutter/Rust integration.
Together with rules_flutter, frustrate allows you to use hot reload and hot
restart for Rust code in a Flutter app.


### The problems solved by `frustrate`
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

**Static analysis to avoid blocking the main thread.** Mark your Rust code with
the opt-in `#[bridge(no_block)]` and add a `frustrate_no_block_check` Bazel
target. Then frustrate's static analysis ensures it's safe to call on the main
thread. [Checking main-thread calls](#checking-main-thread-calls) has the
details.

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

## Getting started

### 1. Depend on frustrate

Your `MODULE.bazel` needs a `bazel_dep` on `frustrate`, `rules_flutter`,
`rules_dart` and `rules_rust`; [`e2e/wasi_demo/MODULE.bazel`](e2e/wasi_demo/MODULE.bazel)
has versions known to work together. frustrate is not on BCR yet, so add a
`git_override` next to its `bazel_dep`:

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

If your bridge crate has Cargo dependencies:

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

### 2. Annotate your Rust

```rust
use frustrate::bridge;

#[derive(Clone)]
#[bridge(data)]
pub struct Entry {
    pub label: String,
}

#[bridge(confined)]
pub struct EventLog {
    entries: Vec<Entry>,
}

#[bridge]
impl EventLog {
    #[bridge(sync)]
    pub fn new() -> Self {
        EventLog { entries: Vec::new() }
    }

    #[bridge(sync)]
    pub fn append(&mut self, label: String) -> Entry {
        let entry = Entry { label };
        self.entries.push(entry.clone());
        entry
    }
}
```

The crate root declares your module and the generated glue's:

```rust
pub mod api;

mod frustrate_generated;
```

[ANNOTATIONS.md](docs/ANNOTATIONS.md) lists every annotation.

### 3. Declare the bridge in Bazel

`frustrate_bridge` runs codegen over your sources, and
`frustrate_bridge_library` builds the crate with the generated glue:

```starlark
load("@frustrate//bazel:defs.bzl", "frustrate_bridge_library", "frustrate_bridge", "frustrate_code_asset")

frustrate_bridge(
    name = "codegen",
    srcs = ["src/api.rs"],
    crate_name = "my_bridge",
    module_paths = ["crate::api"],
    package_name = "my_app",
)

frustrate_bridge_library(
    name = "my_bridge",
    srcs = ["src/api.rs", "src/lib.rs", ":codegen.rs"],
    proc_macro_deps = ["@frustrate//macros:frustrate_macros"],
    deps = ["@frustrate//runtime/rust:frustrate"],
)
```

**Bridge sources are a list.** `srcs` and `module_paths` are parallel: each pair
is a file to parse and the Rust path its items are reachable by from the bridge
crate. The demos declare a single pair because one file covers them, not because
the tool takes one.

**The bridge library is a Dart code asset.** `package_name` is the Dart package
the bindings compile into. The generated `<crate>Library` (`myBridgeLibrary` for
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

### 4. Call it from Dart

`FrustrateNative.init(myBridgeLibrary)` once, then call the bindings:

```dart
FrustrateNative.init(myBridgeLibrary);
final log = EventLog.new_();
log.append('hello');
```

A `dart_test` or `dart_binary` depending on `:bindings` gets the library built
for the host it runs on, and an app bundler (rules_flutter) ships it per
platform. On the web, `FrustrateWeb.initFromUrl(myBridgeBridge,
'my_bridge.wasm')` serves the bridge from a `frustrate_wasm_module`; [Choosing a
web platform](#choosing-a-web-platform) says which one to build.
`BridgeLibrary.dynamic(DynamicLibrary.open(path), name: path, bridge:
myBridgeBridge)` loads a library you built yourself.

### A complete example

[`e2e/wasi_demo`](e2e/wasi_demo/) is the smallest complete app. Read it in this
order:

| File | What it shows |
|---|---|
| [`bridge/src/api.rs`](e2e/wasi_demo/bridge/src/api.rs) | the Rust you annotate with `#[bridge]` |
| [`bridge/BUILD.bazel`](e2e/wasi_demo/bridge/BUILD.bazel) | `frustrate_bridge` + a `frustrate_bridge_library` over its output |
| [`lib/main.dart`](e2e/wasi_demo/lib/main.dart) | calling the generated bindings from Flutter |
| [`BUILD.bazel`](e2e/wasi_demo/BUILD.bazel) | wiring the bridge into a Flutter app |
| [`MODULE.bazel`](e2e/wasi_demo/MODULE.bazel) | a working `bazel_dep`/toolchain pin set to copy versions from |

### Several bridges in one app

**Several bridges, one library.** An isolate (a page, on web) runs one Rust
runtime, so bridges an app uses together are linked into one library.
`frustrate_shared_library` does that, keeping each bridge its own crate, bindings and
asset id, and declares a code asset per bridge that binds its id to the
combined library. Each key is a bridge's Rust library (the `<name>_lib` rlib
`frustrate_bridge_library` declares), each value the `frustrate_bridge` its
glue came from; a pair naming two different crates fails at analysis:

```starlark
frustrate_shared_library(
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
`frustrate_wasm_module(name = "app_rust.wasm", library = ":app_rust")` builds the
one module, and
`FrustrateWeb.initFromUrl(main.mainRustBridge, 'app_rust.wasm')` serves a
bridge from it, once per bridge, the same way. Generated code reaches its
runtime with `Frustrate.of(<crate>Bridge)`; `Frustrate.instance` answers
only while one bridge is active, and `Frustrate.activate` replaces only the
bridge its runtime serves, so a test can fake one bridge and keep another real.

A source need not live in the bridge crate. A domain crate can carry
`#[bridge(...)]` on its own types and be listed directly:

```starlark
frustrate_bridge(
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
frustrate_bridge(
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

**Data classes without the bridge.** Code that only uses a bridge's data
classes need not depend on its native library or its bindings, and so need not
rebuild or rerun when the Rust changes. `data_package_name` declares those
classes in a Dart package of their own, which `frustrate_bridge_data` places in
another directory:

```starlark
# bridge/BUILD.bazel
frustrate_bridge(
    name = "codegen",
    ...
    package_name = "my_app",
    data_package_name = "my_app_data",
)

dart_library(
    name = "bindings",
    srcs = [":codegen.dart"],
    code_assets = [":bridge_asset"],
    package_name = "my_app",
    deps = ["//bridge/data", "@frustrate//runtime/dart:frustrate"],
)

# bridge/data/BUILD.bazel
frustrate_bridge_data(
    name = "classes",
    bridge = "//bridge:codegen",
)

dart_library(
    name = "data",
    srcs = [":classes"],
    package_name = "my_app_data",
    deps = ["@frustrate//runtime/dart:frustrate"],
)
```

Import `package:my_app_data/my_bridge.frustrate.data.dart` for the classes
alone. The bindings re-export them, so code importing the bindings sees the
same classes. The data library holds the structs and enums that reach no
handle, have no bridged members, name only such types and are not taken through
`models`. Their codecs, and every other class, stay in the bindings. The
library changes only when a data declaration does.

### Build settings some setups need

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

## What the bridged surface promises

### Actors

An actor's object lives on its own executor: a dedicated OS thread natively, a
Worker running its own instance of the module on web. Every call is `async` on
the Dart side and the actor handles one message at a time, in arrival order, so
methods get `&mut self` without a lock. Each constructed actor is a new
executor; for more parallelism, create more actors. `ActorPool`
(`package:frustrate/workers.dart`) leases one instance to each job for the
whole job, so the calls of one job never interleave with another's.

Only values cross: no handle in an actor method's parameters, and none in its
return except the constructor's. A large structure crosses as bytes, since
each web instance has its own memory.

Put shared mutable state in an actor rather than a `static` behind a lock. On
web each executor has its own copy of a `static`, and on threaded web a lock the
main thread can reach traps there when contended. If two actors need the same
state, one owns it and the others message it.

A method body that awaits slow work holds the actor until it finishes. A
`Deferred<T>` return hands the slow part off and frees the actor
(`runtime/rust/src/deferred.rs`); an `Addr<Self>` parameter lets a long job run
in slices that later calls interleave with (`runtime/rust/src/addr.rs`).

### Streams and callbacks

A `StreamSink<T>` parameter is a Dart `StreamController<T>` argument; Rust
pushes items, errors and the end. `DartCallback<T>` and `DartFunction<T, R>`
are Dart closures Rust can call. The `dart::` mirrors
(`runtime/rust/src/dart.rs`) say which Dart type each takes and what each can
do. All of them are argument-only: Rust cannot mint a Dart object.

- **`dispose()` a Rust handle that stores a sink or callback.** Natively, an
  open stream or stored callback keeps its isolate alive, as a Dart producer
  would. A test that leaks one passes and never exits, which Bazel reports as a
  TIMEOUT with an empty log. Assert `openChannelCount == 0` in teardown;
  `openChannelLabels` names what is open.
- **Delivery never runs inside the Rust call that posts it**, on any platform,
  so a Dart callback may call back into the bridge.
- **A void Dart method that throws ends its channel**: Rust's next send returns
  `false`, and the error goes to the zone frustrate was installed in.
- **A `DartFunction` answers synchronously** (its Dart side is
  `R Function(T)`). Anything Dart must wait for is a request out on a
  `DartCallback` and a separate bridged call back in. `call_async` works on
  every platform inside an `async fn`; the blocking `call` is native-only, and
  so are returning callbacks on actors.
- **`DartFunction<T, Result<R, E>>`** turns a Dart throw of the bridged type `E`
  into `Err`; any other throw, or an isolate that died owing an answer, panics
  the enclosing call.
- **Cancellation**: once the Dart subscription is cancelled no further event
  reaches it, and Rust sees `add` return `false` at its next send. A producer
  on a web actor sees that only when the actor's executor is idle, so give each
  long-lived producer its own actor.
- **Backpressure**: `add` never blocks and buffers in the controller while it
  is paused; `send(item).await` waits for resume, and `is_paused()` serves sync
  producers. It does not apply on the single-threaded web main instance, where
  the producer runs inline, or on web actors. A broadcast controller is
  refused, since it never pauses.
- **A panic in the producer** is reported on the call. Natively the stream
  closes; on web it stays open.

`frustrate::testing::stream::<T>()` gives a real `StreamSink<T>` that records
instead of posting, and a probe that plays the consumer.

### Trait objects

```rust
#[bridge(confined)]
pub trait Store: Send {
    fn get(&self, key: String) -> Option<String>;
}

#[bridge] pub fn open_store(kind: String) -> anyhow::Result<Box<dyn Store>> { … }
#[bridge(sync)] pub fn store_report(store: &dyn Store) -> String { … }
```

The trait declaration is the bridged surface; implementations need not be in a
bridge file. The trait names its model's thread bound as a supertrait, every
implementor shares the trait's model, and actors cannot take part. A
`#[bridge] impl Store for Sqlite` also bridges the concrete class, which then
passes wherever the trait is expected. A Dart class of your own implementing the
trait's interface cannot cross.

### Rust `async fn` and tokio

frustrate's executor has no reactor, so a tokio leaf (`sleep`, a socket)
panics unless the executor is lent tokio's context. Two ways, both native-only:

- Set the runtime with `frustrate_tokio::set_runtime(handle)` (`ext/tokio`),
  or `frustrate::runtime::set_context` for another runtime, and `.await` the leaf
  in a bridged `async fn`. Cancelling the call drops the future.
- Call your runtime's `block_on` inside a plain `#[bridge]` fn, which Dart
  still sees as a `Future`. Each call in flight parks a pool thread, and
  cancelling from Dart does not get it back.

Mark such members `#[bridge(native_only)]` yourself. Most of tokio compiles for
`wasm32`, so nothing at build time keeps them out of the web build, and they
fail there at run time.

### Reporting failures

A Rust panic's location and backtrace reach only a Rust-side listener,
`frustrate::panic::set_listener`. Anything Dart did not catch goes to the zone that
issued the call (or, for a panic no call waits on, the zone its actor was built
in or frustrate was installed in), which is what `runZonedGuarded` and
Flutter's `PlatformDispatcher.onError` see. A returned `Err` is a declared
outcome, not a failure to report. Generated bindings rethrow with their own
stack; a `DelegatingRuntime` (`package:frustrate/intercept.dart`) that captures
the stack when a call is issued gives reports the app's call site.

`frustrate::carry::set` carries a per-thread value of yours (the cause a
write is attributed to, say) from the thread that queues bridge work to every
thread that runs it (`runtime/rust/src/carry.rs`).

## Hot reload and hot restart for Rust

Hot reload swaps edited code into the running app and keeps its state; hot
restart starts the app over. frustrate supports both for Rust edits, under
rules_flutter's [dev tool](https://github.com/aran/rules_flutter), which is
also where to find how to trigger them.

**Hot reload** compiles the edited Rust into a small patch library and redirects
the running library's bridged entry points into it. Rust state survives: a
counter behind a handle keeps counting from where it was. Adding, removing,
renaming or re-signing a bridged function reloads too, and a caller still
holding the old function can never land on a different one. Calls already in
flight finish on the code they started with, as a Dart closure created before a
reload does.

Some edits cannot be patched into a running process. The reload refuses them,
names the reason, and holds back the Dart half of the edit too, so the app keeps
running what it had; a hot restart picks the edit up. Among them:

- a bridged data type whose shape changed
- a new or changed opaque type
- a changed layout of a type the running code already holds values of
- a new `static`
- a changed dependency

**Hot restart** relaunches the app on the rebuilt library, so Rust state starts
fresh along with Dart's.

Hot reload needs a native target and a `-c dbg` build. Opt in by wrapping the
bridge library, binding the code asset to the wrapper, and wrapping the asset
for rules_flutter:

```starlark
load("@frustrate//bazel:hot_patch.bzl", "frustrate_hot_patchable")

frustrate_hot_patchable(
    name = "my_bridge_hot",
    binding_contract = ":codegen.interface",
    library = ":my_bridge",
)

frustrate_code_asset(
    name = "bridge_asset",
    bridge = ":codegen",
    library = ":my_bridge_hot",
)

flutter_native_library(
    name = "my_bridge_native",
    binding_contract = [":codegen.interface"],
    hot_patch = ":my_bridge_hot.hot_patch",
    library = ":bridge_asset",
)
```

Then list `my_bridge_native` in the bindings package's `code_assets`, in place
of the bare asset.

For a `frustrate_shared_library`, pass the same `bridges` dict instead of a contract.
That also declares each bridge's code asset over the patchable library
(`<name>.<library>`), and every bridge's wrapper names the one patch builder:

```starlark
frustrate_hot_patchable(
    name = "app_rust_hot",
    bridges = BRIDGES,  # what frustrate_shared_library(name = "app_rust") was given
    library = ":app_rust",
)

[flutter_native_library(
    name = "app_rust_native." + bridge,
    binding_contract = ["//%s:codegen.interface" % bridge],
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
different trades rather than better and worse. `@frustrate//bazel:wasm32_wasip1`
supplies a host for all of it and costs module size.
`@frustrate//bazel:wasm32_browser_std` keeps the plain target, but you build your
own `std`. We have code to help, but it's more work.

```sh
bazel run //toolchain/custom_std:build -- --facilities=clock,random,stdio,thread
```

Take wasi when your graph wants a lot of std; take the browser std when it wants
one thing and you cannot give up the target.

**Neither?** `@frustrate//bazel:wasm32`, plain — the default. Smallest module,
stable Rust, no post-pass.

**The first two are mutually exclusive.** "A clock and a WebSocket" is an
ordinary thing to want and you cannot have both: a wasi target cannot also take
the browser-API path. Plan around it rather than discovering it.

**Parallelism.** `@frustrate//bazel:wasm32_threads` is the plain target plus
atomics and a shared memory. It needs a nightly toolchain and a locally built
std. If you want parallelism, you can have it, but it's slightly more work and
risk.

A threaded graph with getrandom 0.4.3 fails with `E0433` on `js_sys`. Build on
`@frustrate//ext/getrandom:wasm32_threads`, or add a crate annotation;
[ext.md](docs/ext.md#extgetrandom-on-threaded-web) has both.

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
    library = ":app_rust",
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
  if (actor?.bridge === 'my_bridge' && actor.type === 'Core') {
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
await FrustrateWeb.initFromUrl(appBridge, 'app.wasm',
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

### What each web platform supports

With a stock std:

|  | `wasm32` | `wasm32` + `bindgen` | `wasm32_wasip1` | `wasm32_threads` |
|---|---|---|---|---|
| triple | `wasm32-unknown-unknown` | same | `wasm32-wasip1` | `wasm32-unknown-unknown` |
| Rust channel | stable | stable | stable | nightly + local std |
| `SystemTime::now` | build error | build error | works | build error |
| `Instant::now` | aborts at run time | via `js-sys` | works | aborts at run time |
| `getrandom` crate | compile error | via `wasm_js` | works | compile error |
| `println!` | build error | build error | to the console | build error |
| `web-sys` / `js-sys` | — | yes | panics per call | — |
| real threads | no | no | no | yes |

The build errors come from frustrate's std-facility check
(`bazel/wasm_std_check`), which does not cover `Instant::now`.
`wasm32_browser_std` makes the clock, `println!` and abort cells work on the plain
target. getrandom never asks std, so on either `wasm32` column it needs
`ext/getrandom` ([ext.md](docs/ext.md)).

On wasip1, getrandom refusing its browser backend is the first error you meet
(`` `wasm_js` backend can be enabled only for OS-less WASM targets! ``), and
wasm-bindgen does not refuse at all: every generated body panics when called.
wasip1 has no sockets and no component model (wasip2 builds components, which
no browser instantiates).

Never satisfy a wasm-bindgen import with a hand-written stub: the module
instantiates and returns wrong answers (a stubbed `getRandomValues` returns
zeros). Use the `bindgen` attribute, which runs the real post-pass.

A wasm triple goes in two lists in your `MODULE.bazel`, and each replaces its
default rather than extending it:

```python
rust.toolchain(extra_target_triples = ["wasm32-wasip1", ...])
crate.from_cargo(supported_platform_triples = ["wasm32-wasip1", ...])
```

Missing from the first, analysis fails with "No matching toolchains found".
Missing from the second, the crate resolves to `//:incompatible` and shows up
as a skipped target. A per-platform `build_test` of every artifact you ship
catches both.

Build modules with `-c opt`; `fastbuild` barely optimizes, and wasm-opt does
not make up the difference.

### A custom std

`toolchain/custom_std/` builds a std whose `wasm32-unknown-unknown` stubs
(clock, entropy, stdio, `available_parallelism`, `sleep`) call the host
instead, for dependencies you cannot change. Build the browser std
`//bazel:wasm32_browser_std` links, then select that platform:

```sh
bazel run //toolchain/custom_std:build -- --facilities=clock,random,stdio,thread
```

The std is tied to the nightly that built it, so rebuild after the nightly pin
moves. When it is missing or stale, the first error names the command that
fixes it; the "not declared in package 'impl'" and missing-toolchain errors
after it are the same cause.

- `Instant` resolution is whatever the page gives `performance.now()`, which
  browsers coarsen unless the page is cross-origin isolated.
- `thread::sleep` busy-waits on a worker and throws on the main thread.
- `available_parallelism` reports the machine; `thread::spawn` still fails on a
  single-threaded build.

### Threaded wasm

`wasm32_threads` gives the module shared-memory threads. Async calls on web
leave the main thread, `Locked` contention is real, and `Frozen` values are
read in parallel. It needs nightly Rust, the `atomics` custom std, and COOP/COEP
headers on the page. frustrate creates the pool's Workers itself, so in bridged
code:

- `thread::spawn` panics, at run time, including inside a dependency;
- `yield_now` does nothing, so a spin-then-yield loop only spins;
- `available_parallelism` is `Err`;
- the main thread may never wait, so anything it reaches must not contend.

A panic traps without running `Drop`. The pool replaces the worker, but what
the job held stays held: a `locked` object whose holder trapped never releases,
and later async calls on it wait forever.

The usable heap is the shared memory's `maximum − initial`, because Rust's
allocator only grows memory; `maximum == initial` leaves none.

### A strict Content-Security-Policy

By default the runtime injects its JS glue as an inline `<script>` and starts
workers from `blob:` URLs. Under a strict CSP, serve the glue as a file instead:

```python
frustrate_web_glue(name = "frustrate.js")   # in the app's own package
```

List it in `flutter_web_app`'s `extra_web_assets` and load it from
`index.html` with a plain `<script src="frustrate.js"></script>` (no `async` or
`defer`) before init. Workers are created from the script's URL; if a bundler
renames it, set `globalThis.$frustrateGlueUrl`.
[`e2e/flutter_demo/web_csp.bzl`](e2e/flutter_demo/web_csp.bzl) has the
strictest policy frustrate supports. Hosts your Rust connects to need
`connect-src`, and missing them fails silently. A policy that forbids `blob:`
workers breaks Flutter's skwasm render worker, not frustrate.

## Checking main-thread calls

A sync call into Rust runs on the Dart thread that made it, and in a Flutter
app that is usually the main thread. A wait there freezes the UI, and in a
browser it fails outright. `#[bridge(no_block)]` on a member, on its `impl` block, or
on a whole file with `bridge_file!(no_block)`, promises that a main-thread call
to it cannot be stalled: what the call runs on the caller's thread reaches no
wait, no function you name as slow, and no IO you name.

A `frustrate_no_block_check` target checks every promise in a bridge, for each
target the bridge ships on:

```starlark
load("@frustrate//bazel:defs.bzl", "frustrate_no_block_check")

frustrate_no_block_check(
    name = "no_block_check",
    bridge = ":codegen",
    library = ":my_bridge",
    targets = ["android", "ios", "web_threads"],
    slow = ["my_core::index::rebuild"],
    io = ["files", "network"],
)
```

`web_threads` builds under `@frustrate//bazel:wasm32_threads`. An app that ships on
another threaded platform, such as `@frustrate//ext/getrandom:wasm32_threads`,
names it with `web_threads_platform`, so the check scans what the app ships.

When the target is a `frustrate_shared_library` that links several bridges, `bridge` takes a
list, one `frustrate_bridge` per linked bridge.

`bazel test :no_block_check` runs one analysis per target and CPU, from any
host: natively over the whole linked program, on the web over the wasm
module. Members whose bodies
run off the calling thread, such as an actor's methods, pass without their
bodies being read. What runs on the caller is checked: a `#[bridge(sync)]` body, a
member's argument decoding, and the drop of a handle it returns. A call the
analysis cannot follow, into a C function it does not know or through a
function pointer installed at run time, fails as "cannot verify", naming it.
A promise no check target covers is not checked.

A finding you have judged safe is silenced by name, with `no_block(allow(...))`
on the promise or the check target's `allow`; `#[frustrate::no_block(allow)]`
on one of your own functions vouches for everything under it. A silence that
matches nothing fails the check. [ANNOTATIONS.md](docs/ANNOTATIONS.md#no_block-and-bridge_fileno_block)
has the rules.

Single-threaded web has no other thread, so a dispatched body runs on the main
thread there, slow or not; the remedy is the threaded build. Outside what the
check covers, these stalls are not caught:

- **A lock codegen cannot see** (a `static Mutex`) contended on the
  threaded-web main thread traps; `explainTrap` names the cause when it
  recognises the browser's message.
- **A `RwLock` read/write conflict on single-threaded web** traps with no
  message under stock std, and panics with `rwlock locked for reading` (or
  `writing`) under a custom std without `atomics`.
- **`thread::park` waiting for an `unpark` on single-threaded web**, an actor's
  Worker included, spins forever under stock std, and panics under a custom std
  without `atomics`.
- **An async-runtime context that blocks** stalls the executor
  silently when the context that is set only enters it; `ext/tokio`'s makes
  tokio panic instead.
- **`send_wrapper::SendWrapper`** in a dependency turns the compile error that
  keeps JS state off the threaded-web pool into a run-time panic.
