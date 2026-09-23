# Threaded wasm

`frustrate_wasm_module(platform = "//bazel:wasm32_threads")` gives the bridge
module real shared-memory threads: `+atomics`, a `SharedArrayBuffer`-backed
memory, and a std rebuilt to match ([custom_std.md](custom_std.md), the
`atomics` facility). It is an opt-in. The single-threaded build stays the
default and the only stable-Rust path.

What it buys: async bridge calls on web really leave the main thread, `Locked`
contention becomes real, and `Frozen` values can be read in parallel without
copying, which Actor cannot express. Actor does not depend on any of it.

What it costs: nightly Rust, a locally built std, and COOP/COEP headers on the
page. The bridge's memory is shared only among its own threads; the dart2wasm
app keeps its own memory.

## What bridged code sees

wasm has no spawn instruction and `wasm32-unknown-unknown` has no libc to
supply one. With `+atomics`, std gets a real futex `sleep` but its `Thread::new`
still returns `Err`. So frustrate creates the pool's Workers itself, and nothing
else can:

- `thread::spawn` panics. It is a run-time failure, so a `spawn` inside a
  dependency is not caught at build time.
- `yield_now` is a no-op, so a spin-then-yield loop only spins.
- `available_parallelism` is `Err`.

Parallelism for your code is a plain `#[bridge]` member or an Actor.

The browser main thread may never wait (`memory.atomic.wait`). Anything the main
thread reaches must spin or never contend; only workers wait.

## Panics restore width, not state

A trap under `panic=abort` runs no `Drop`. The pool replaces the dead worker, so
its width is unchanged, but whatever the panicking job held stays held. A
`locked` object whose holder trapped is dead: later async calls on it wait
forever for a release. `panic=unwind` via wasm exception handling was not
pursued; it needs an experimental toolchain for semantics the loud-error
approach already covers.

## Heap size

Rust's wasm allocator only gets memory through `memory.grow`, so the usable heap
is the shared memory's `maximum − initial`, not `maximum`. `maximum == initial`
would be a heap of zero.

The single-threaded build cannot be capped from the host at all: it exports its
own memory with no maximum and grows until the engine refuses.

## With wasm-bindgen

An app whose Rust reaches a browser API needs the wasm-bindgen post-pass, and
that pass has a thread transform of its own. It runs whenever the module's
memory is shared and cannot be switched off. Hiding the shared bit is not an
option, because the same bit tells the generated shims to copy out of shared
memory instead of viewing it.

The rule: **let the transform own thread bootstrap; never let it own the
memory.** Its bootstrap does what frustrate's does (TLS init, a stack from the
same allocator), so giving it up changes the mechanism, not the semantics. But
the transform also moves the memory import into the generated JS module, which
creates a fresh shared memory in every realm that imports it. Left alone, every
Worker would get its own memory: threads that share nothing, silently. frustrate
builds every import object, so each instantiation hands the module the memory it
means.

Two waits are reachable from `__wbindgen_start` on the main thread: its
temporary-stack lock and lld's passive-data-init barrier. Both are safe because
the main thread's `__wbindgen_start` finishes before any other thread exists on
that memory. Pool workers are only spawned from inside a bridge call, and each
actor is thread 0 of a memory it created. Keep it that way. For the same reason,
never call `__wbindgen_thread_destroy` from the main thread.

Costs: running out of memory while spawning a worker fails worker init rather
than the call that triggered it, and each worker realm's copy of the generated
JS allocates a shared memory nothing uses, which reserves address space per
worker.

## Non-goals

Replacing the single-threaded transport; shared-everything threads (a future
proposal that would give dart2wasm itself real threads); making Actor depend on
threaded wasm.
