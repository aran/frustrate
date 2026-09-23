# What may block, and where

1. **Async is free.** Async code that communicates back to the main thread by
   `await` or `Atomics.waitAsync` is safe and needs no opt-in.
2. **Sync is safe when it is closed**: synchronous code that only calls other
   synchronous code and never shares memory with a concurrent thread.
3. **Background threads may wait.**
4. **The main thread may not.** It communicates by message passing and reads
   only owned copies of shared data.

The goal these serve: **unsafe code requires an opt-in, and broken code fails
loudly.** Most violations are loud already, by the platform's doing: a wait on
the browser main thread traps, and std's single-threaded wasm backends panic
rather than block. frustrate's part is to make those failures attributable and
to close, or name, the cases the platform leaves silent.

## What is not caught

Each mechanism is documented where it lives. These are the gaps it leaves:

- **A lock codegen cannot see**, such as a `static Mutex`, contended on the
  threaded-web main thread. It traps; `explainTrap` names the cause when the
  trap message is Chromium's wording and falls back to a bare report
  otherwise.
- **A `RwLock` read/write conflict on single-threaded web.** std takes
  `rtabort!`, which bypasses the panic hook, so nothing can attribute it.
- **A body that calls `thread::park` on single-threaded web.** `park` returns
  immediately there, so the caller spins the only thread forever with nothing
  to observe. `tools/check_no_park.dart` gates frustrate's own module; a
  bridge crate's module is not checked.
- **`thread::sleep` on a worker** under the `thread` facility busy-waits. It
  burns a core; it does not hang.
- **A registered async-runtime context that blocks** stalls the executor's
  drain, and nothing reports it: tokio does not treat `Handle::enter` as a
  blocking region.
- **An actor that blocks on single-threaded web** hangs that actor.
- **`send_wrapper::SendWrapper`.** On threaded web, placement relies on
  wasm-bindgen's `JsValue` being neither `Send` nor `Sync`, so safe Rust
  cannot move JS-touching state onto the pool. `SendWrapper<T>` is
  `Send + Sync` for every `T` and panics when used or dropped off its thread. A
  dependency that uses it turns that compile error into a runtime panic, which
  no artifact scan can see.

## Native lets the main isolate wait, by declaration

Rule 4 has no platform exemption, but `on_contention = "block"` on native lets
the calling isolate wait (on web it is a Dart compile error). It stays: it is
declared, it is absent from web, and it is the only lock model that can wait
where waiting is legal, so removing it would take that model away. A project
that wants rule 4 on native opts in with `no_block`, on a member, a block or a
file. This is [configuration.md](configuration.md)'s principle: the safe reading
is the one a project can turn on.

## What `no_block` means

**Main-thread Dart calling this member cannot be stalled.** Call only
`no_block` members from the main thread and it will never trap on
`memory.atomic.wait32` or spin on a body that parks.

"Never waits on any platform" describes every claim the tree accepts, but it is
not the rule, and reading it as the rule gets one case wrong. A member whose
`executor::spawn` takes an internal lock in the caller's own frame passes the
definition and would fail the description. A sync `on_contention = "error"`
member is the same case with a lock instead of a queue, which is why it keeps
the claim and is checked rather than refused.

Codegen sees signatures, not call graphs, so it cannot infer that a body
blocks. It can see **where the body runs**, and that settles most claims:

- An **actor member** runs entirely on the actor's own executor, parameter
  decode included, so a wait there falls under rule 3 on every configuration.
- A **dispatched member** runs off the caller wherever the wait instruction
  exists. It runs inline on the caller only on single-threaded web, where the
  module contains no wait instruction at all. These are two different
  arguments. Together they cover every configuration only because the
  remaining pairing, `+atomics` without `wasm-threads`, is a `compile_error!`
  in `runtime/rust/src/pool.rs`.
- A **`#[bridge(sync)]` body** runs in the caller's frame. Only these need the
  artifact check, `bazel/wasm_block_check`.

Placement is what makes the claim usable on real dependencies. In the iroh
demo, any fallible iroh call builds an error that asks a `OnceLock` whether
backtraces are enabled, so even parsing a pairing ticket reaches a wait. No
scan could clear that body, and none has to, because the parse never runs on
the isolate that asked for it.

A claim also covers the `Drop` of every opaque handle the member returns,
because freeing a handle runs its `Drop` on the thread that frees it.

## Why the check scans a purpose-built module

Analysing the call graph of the shipped module cannot be made sound.
Monomorphised code shares a handful of wasm function signatures, so filtering
`call_indirect` targets by type still admits nearly every table entry. The check
instead builds a throwaway module whose only exports are the claimed members,
and lets lld's dead-code elimination do the reachability conservatively. Green
is sound; red can be a false alarm.

The predicate is stricter than the hazard on purpose. It asks for zero
reachable waits, not "no wait the main thread could reach", so a claimed body
goes red for a wait that would have been legal where it runs. The relaxed
predicate ("the body may wait, as long as nothing live across an `.await` has a
waiting `Drop`") is a whole-program property. "This body reaches no wait" is
one an author can check by reading the body.
