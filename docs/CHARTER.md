# frustrate — Charter

Flutter + Rust, under Bazel, without the frustration. (f = Flutter, rust = Rust.)

This states what frustrate is committed to. It is not a reference and defers to
nothing: the codegen and its checker say what is accepted, `tests/` says what is
guaranteed, and `e2e/` says what it looks like in an app.

## Vision

A rules_flutter/rules_dart user adds Rust to their app by writing a crate, annotating a
bridge module, and declaring one target — and gets interop that is safe by construction,
portable across native and web, fast enough to disappear, with a working dev loop. The
Bazel experience must be first-class, not a tolerable port of a cargo workflow.

## Customer

Bazel-Flutter users (rules_flutter / rules_dart adopters). Bazel-optional core is an
architectural discipline (standalone-testable, publishable later), not a v1 marketing
surface. Private — no public pushes, no upstream issue filing — until substantial
progress and initial adoption.

## Commitments

1. **Onboarding stays cheap**: from a stock rules_flutter app, adding Rust is ≤3
   declared steps; `flutter_bazel run` renders a UI calling Rust.
2. **Safety property**: *safe by default, everything expressible, contracts
   loud.*
   - The unannotated path can never hit a concurrency-vocabulary runtime failure.
   - Every hazardous model/platform combination is reachable only through a named,
     contract-bearing opt-in; the build error for an undeclared hazard names the opt-in
     and its contract.
   - Contract violations fail deterministically, loudly, and attributably — the error
     names the object, method, and violated contract. Never UB, never silent.
3. **Portability is real**: identical app code across supported platforms on the
   default transport (stable Rust, single-threaded wasm instances), with Actor
   giving genuine web parallelism — measured, not asserted.
4. **Boundaries held**: rules_flutter contains zero Rust-specific code (frustrate
   *consumes* rules_flutter through public generic seams); the core tool and runtimes
   pass their tests with no Bazel present; two-tier API on the ruleset surface.

## Non-goals (v1)

Migration tooling from flutter_rust_bridge, or matching its full type surface;
Windows, and Linux beyond a proven cross-compile; threaded wasm as anything but a declared opt-in — never the
default; dart2js as a *production* runtime (dart2wasm is the web platform;
dart2js/DDC runs for development only, fenced out of release builds and behind a
declared 53-bit contract, because it is the only web dev loop with hot reload);
non-Bazel onboarding polish; anything public.

## Concurrency vocabulary

Per-type declaration; legality of sync/async derived, checked at codegen time against
target capability facts. The models follow a taxonomy worked out in
flutter_rust_bridge's issue tracker
([#1917](https://github.com/fzyzcjy/flutter_rust_bridge/issues/1917): unwrapped
data, a generated lock, or hand-written synchronization, chosen per piece of
data). frustrate makes that choice a declaration: written per type, and checked
before the program runs.

| Model    | Rust shape        | Bound on `T`   | Sync *Dart* methods     | Async methods | Notes |
|----------|-------------------|----------------|-------------------------|---------------|-------|
| Confined | `Box<T>`          | `Send`         | yes (run on caller)     | no            | single-owner; contract: one isolate |
| Resident | `Box<T>`          | none           | yes (run on caller)     | no            | single-owner, **one thread**: built, used and freed on the caller's thread, checked on every acquisition. An isolate that exits leaks what it held |
| Frozen   | `Arc<T>`          | `Send + Sync`  | yes (`&self` only)      | yes           | immutable after construction |
| Locked   | `Arc<RwLock<T>>`  | `Send + Sync`  | opt-in (`on_contention`) | yes (default) | async acquisition everywhere; **sync** acquisition needs a contract — `error` (try-lock, throws ContentionError) is on every target, `block` (waits to acquire) is native-only |
| Actor    | message passing   | none           | no                      | yes           | the body is a plain sync `fn`, or returns `Deferred<T>` to finish later. Load-bearing: the only web parallelism story; ActorPool fans jobs across instances |

Guidance principle the examples gallery is organized around: **async ≠ parallel.**
The portable async contract is "completes via a future, request consumed before it
returns" — not "runs off the UI thread." On web, long CPU work belongs in an Actor;
Confined/Frozen/Locked/Resident are for work cheap enough to run on the caller.

**Confined, Frozen and Locked carry a uniform thread bound, and deliberately so.**
A `!Send` type used only through `sync` members is semantically fine and still
rejected by all three. The bound is *not* gated per platform: one crate builds for
native and web from one source, so the native build demands it anyway, and gating
would split the identical-app-code commitment for nothing.

**The two answers for a `!Send` type are Resident and Actor, and the choice is
where the work runs.** Resident removes the bound by removing what needed it: the
object is built on the caller and freed on the caller, so nothing ever transfers
it, at the price of a thread affinity and a leak on isolate exit
([ANNOTATIONS.md](ANNOTATIONS.md), `resident`). Actor keeps the bound off by
owning a thread, so its object can be reached from any isolate — at one OS thread
per instance and every member async.

## Architecture

The decisions, not a map of the code.

- **No cargo-expand, ever.** `#[frustrate::bridge]` attributes are inert no-op
  proc-macros — visible to the source parser, accepted by rustc, never expanded by us.
  Extraction is syn parsing over explicitly declared files, and a macro-generated API
  is supported by generating the source with a declared build action instead.
- **The IR represents more than the codegen accepts**, so a new feature extends it
  rather than breaking it. A restriction lives in the checker, which states it, never
  in the IR.
- **One byte codec** (SSE-style), the same implementation contract in Rust and Dart,
  golden-vector tested cross-language. No per-platform codec cliff.
- **Native transport is `dart:ffi`**, with completions posted to the isolate's
  `RawReceivePort` through a hand-written Rust binding of the `dart_api_dl` table —
  no allo_isolate, no C compiled into the build. A raw port rather than a
  `NativeCallable.listener` because Rust producers routinely outlive the isolates
  they feed, and a post to a dead isolate must fail rather than abort the process.
- **No wasm-bindgen in frustrate's own ABI.** The web transport is the bridge crate
  on wasm32-unknown-unknown, whose `extern "C"` byte-buffer ABI maps directly to wasm
  exports, reached via `dart:js_interop` under dart2wasm.
- **COOP/COEP is required by fiat**, because skwasm and threaded wasm need it. The
  Actor channel itself does not.
- **dart2js/DDC is a development-only runtime.** Its `ByteData` lacks the 64-bit
  integer accessors, so `i64` and handles take a two-halves arm that is exact to 2^53
  and throws past it. Release builds on it are refused at init, and its app-level
  arithmetic is 53-bit where dart2wasm's is not — so it is for iterating, never for
  verifying.
- **Layering**: codegen+checker is a Rust library with a thin CLI, usable from
  build.rs and Bazel actions alike; runtimes are a plain Rust crate + Dart
  package; Starlark does only build wiring.
