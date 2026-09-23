# Rust `async fn`

A bridged Rust `async fn` gives Dart a `Future` on every configuration: native,
single-threaded web, and threaded web. Its body can suspend, so something has to
drive the future to completion. frustrate's cooperative executor does that. A
suspended future is kept as heap data, not as a parked thread, so the
executor works even where no thread may block.

## The invariant: the browser main thread never blocks

On web, synchronous waiting is confined to pool workers; on native it is also
allowed wherever a member declares it ([blocking.md](blocking.md)). What a
violation looks like depends on the build:

- **Threaded web (`+atomics`).** The spec bars `Atomics.wait` on the main
  thread, so a contended wait traps. That is loud, and it is attributed to the
  call that caused it.
- **Single-threaded web.** There is no wait instruction at all. Most of std's
  blocking primitives refuse, but `thread::park` returns immediately, so a body
  that parks in a loop spins the only thread forever. Nothing reports it, and
  `tools/check_no_park.dart` exists to catch it at link time.

Whether shared memory (SharedArrayBuffer) is available depends on cross-origin
isolation, not on threads. So single-threaded-with-shared-memory is a real
configuration, and nothing may assume that shared memory implies worker threads.

## One ABI on every platform

The call path, the codegen, the Dart completion path and the executor core are
the same everywhere. The only per-configuration piece is the executor's
`Scheduler`, which decides how a woken task gets re-polled on the right thread.

A separate web ABI was rejected. Async needs one thing specific to web: a way
to schedule a re-poll, and that fits behind the `Scheduler` interface without a
new calling convention. A second ABI would bring back what building without
wasm-bindgen avoids: an extra build step, a JS marshalling layer, and code paths
that differ between native and web. Rich JS interop does not need a second ABI
either: a bridge crate can depend on `web-sys` directly.

## Bridging a body that needs tokio

The cooperative executor has no reactor. A tokio or async-std leaf needs that
runtime's reactor to wake it. You have two options, and both work only on native:

- **Register the runtime's context** with `frustrate::runtime::register`, then
  write a bridged `async fn` that `.await`s the leaf directly. No thread is
  parked, and cancelling the call drops the future
  ([async_runtime_hook.md](async_runtime_hook.md)).
- **Block on your own runtime** inside a plain (non-`async`) bridged function.
  Codegen makes it an async member, so Dart still gets a `Future`:

  ```rust
  #[bridge(native_only)]
  pub fn fetch_len(url: String) -> usize {
      RT.block_on(async { … })
  }
  ```

  This parks one pool thread for every call in flight, and cancelling from
  Dart cannot get that thread back.

**Write `#[bridge(native_only)]` yourself; the compiler will not make you.**
What web lacks is tokio's reactor, not the crate. Most of tokio compiles for
`wasm32-unknown-unknown`, so no link error keeps the member out of the web
build. If it gets in, it fails at run time in a different way on each web
build. Codegen sees the signature, not the calls a body makes through a
third-party runtime, so it cannot infer that a body blocks. Declaring it is
your job.
