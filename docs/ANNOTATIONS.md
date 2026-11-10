# Annotation contracts

Most `#[bridge(...)]` options are **decisions**: a mapping or a spelling with no
safety consequence, where a mistake costs a rename. `macros/src/lib.rs` lists
them, and `parse_bridge_option` in `codegen/src/parse.rs` is the authority for
what is accepted. Misuse of any of them is refused by a named diagnostic.

A few are **contracts**: they let you do something codegen cannot prove safe,
because the fact that makes it safe is about your program, your dependencies or
the Dart side, and codegen cannot see it. This page is those contracts — what
each one asserts, why it cannot be checked, and what happens when the assertion
is false. The build-level opt-ins at the end carry the same kind of contract
one level up.

## `on_contention = "error" | "block"`

Required on a `#[bridge(sync)]` member that touches a `locked` handle (FR0007).
`"error"` is a try-lock that throws `ContentionException`; it works on every
target, and is the only synchronous way to touch shared state from the browser
main thread. `"block"` waits for the lock and is native-only (FR0008).

**You assert**, for `"block"`: the calling thread can afford to wait, and
nothing the body calls back into reaches the same object. For `"error"`: the
caller handles `ContentionException`.

**Why it cannot be checked:** codegen does not know which isolate calls the
member or what the body does while it holds the lock. Locks are not re-entrant,
and a call takes every lock it needs up front in a global order, so the only
deadlock left is re-entry: the body calls into Dart, and that Dart calls another
`on_contention` member on the same object. Under `"block"` that waits on itself;
under `"error"` it throws. A wrong `"block"` is a UI stall no check will find.

## `no_block` (and `bridge_file!(no_block)`)

**You assert** that main-thread Dart calling this member cannot be stalled:
nothing the caller runs reaches a wait, a function the check target names as
slow, or IO it names. The claim covers the body
together with where it runs, so a member whose body runs on another thread
satisfies it whatever the body does. `bridge_file!(no_block)` claims every item
in the file; per-item options cannot opt out.

**What is checked:** codegen never sees bodies, so it catches only the two
contradictions visible in a signature — `on_contention = "block"` (FR0048) and a
value-returning `DartFunction` on a non-`async fn` (FR0049).
`frustrate_no_block_check` proves the rest, once per target the bridge ships
on: no claimed member reaches a wait, a function under a path in its `slow`
(your own `#[inline(never)]` slow-work marker, or a third-party function), or
IO in a class in its `io` (`files`, `network`, `process`, which frustrate maps
to each target's calls). For each native platform (Android, iOS, Linux with
glibc, macOS) it reads that platform's build's whole-program LLVM IR and fails a
member that reaches a C or OS function its C library is not known to return
from without waiting as "cannot verify"; IO calls are among those already, so there an `io`
class only makes the finding red and names the class. A `slow` path that names
no function in the native program is refused as a likely misspelling. On
threaded web it scans the wasm module for `memory.atomic.wait`; on wasip1, for
`poll_oneoff`. Red can be a false alarm.
A call through a value loaded from a static (a logger installed with
`log::set_logger`, a `Box<dyn Fn>` behind a static) or returned by a C function
is "cannot verify", naming the function that makes it, unless the static is one
only the program can name and holds only the program's own functions (a
runtime CPU dispatch, as memchr's), whose calls are then followed. The panic
path is not checked. To hold every sync member of a bridge to this, put
`bridge_file!(no_block)` in each of its files. Each analysis sees only its own
target's `#[cfg]` arm.

**Silencing a finding:** `no_block(allow(name, ...))` is part of the
claim — on the member, its impl block, or `bridge_file!(no_block(allow(...)))`
— and silences a finding that names `name` for the claims in that scope; the
check target's `allow` does the same for every claim it checks, for what a
dependency or the build decides. `name` is
what the finding says to silence it as. Each target names a finding its own
way: natively a wait is the C function that waits (`pthread_mutex_lock` on
Apple platforms; on Android and Linux std's locks wait in a futex,
`syscall::SYS_futex::FUTEX_WAIT_BITSET`), on threaded web the function holding
the wait instruction, so a finding on two targets wants a name for each. Each analysis lists the silences it applied, and
the check target's `.silences` test fails on a silence that none of its
analyses applied.

**Silencing a whole function:** `#[frustrate::no_block(allow)]` on a function
of your own — free, a method or generic — means a claimed member that reaches
it is fine there: no analysis follows into it, so nothing it calls is a
finding, on any target. Natively it also takes the function out of the bounded
lock's section proof. Use it for your own code, including a small wrapper that narrows a
third-party call to one call site:

```rust
#[frustrate::no_block(allow)]
fn lock_uncontended_here(m: &Mutex<State>) -> MutexGuard<'_, State> { m.lock().unwrap() }
```

Use the check target's `allow` for code you cannot annotate, and the member's
`allow` as a convenience when the name is enough. In an ordinary build the
function is unchanged; the attribute costs nothing in release. The attribute
fails `.silences` when a claimed member reaches the function but no analysis
finds anything under it; a marked function no claim reaches is ignored.

## `native_only`

**You assert** that this member, block or handle type cannot exist on web,
because it or a dependency does not compile for `wasm32`. Codegen parses source
and never runs rustc for any target, so whether a crate builds for wasm32 is
exactly what it cannot see.

**What you get:** the member is left out of the web Dart surface and its
dispatch arm is `#[cfg]`'d out of the wasm build. Members that must block on
the caller (`on_contention = "block"`, a value-returning `DartFunction` on a
non-`async fn`) are native-only without the annotation. Either way, calling a
native-only member from portable code is a **compile error on the web build**,
the same way importing `dart:io` is. A surface that compiled everywhere and
threw on web would turn a build-time fact into a runtime failure and ship API
that can never work. Where you want that trade anyway, `web = "runtime_fail"`
puts one member back on the web surface as a stub that throws
`UnsupportedError`.

**If you get it wrong:** leaving it off gives you a raw cross-compile error from
deep in your dependency graph that names no bridge member. Writing it where it
was not needed only costs portability.

## `bytes(dart = "…", import = "…", encode = "…", decode = "…")`

Lets a type that already has codecs in both languages, such as a protobuf
message, cross as one length-prefixed byte payload. Signatures stay typed with
your own types in both languages, and the bridge never looks inside the payload.

```rust
#[bridge(bytes(dart = "Plan", import = "package:app/gen/plan.pb.dart"))]
pub struct PlanMsg(pub proto::Plan);

impl frustrate::BytesCodec for PlanMsg {
    fn to_bytes(&self) -> Vec<u8> { self.0.encode_to_vec() }
    fn from_bytes(b: &[u8]) -> Self { PlanMsg(proto::Plan::decode(b).expect("Plan")) }
}
```

The Dart side of the mapping is declared here rather than registered at
runtime: a registry would turn a static fact into a call you have to make
before first use, and the signature would still need to name the Dart type.

**You assert** that both codecs agree on the format, because they come from one
schema or because you keep them in sync. Codegen only emits calls into them.
`from_bytes` that fails to parse panics, and the panic reaches Dart attributed
to the call. A payload that decodes to a *different valid value* cannot be
detected.

## `take()` and `Consumed<T>`

A Rust member that takes `self`, `self: Box<Self>` or a handle by value consumes
the object. The opt-in is on the Dart side, because "nothing uses this handle
after the call" is a fact about the Dart program: `doc.take()` gives a
`Consumed<Doc>`, and consuming members accept only that. There is no Rust
attribute for this.

**You assert**, on a `frozen` or `locked` type only, that no other call on that
handle is still running when the consuming call is made — including a stream
one of its members is still producing. On `confined`, `resident` and `actor`
nothing can be in flight, so there is nothing to assert.

**If you get it wrong:** the runtime detects it; it is not a race. The call
throws `ContentionException`, and the object is released when the running call
finishes rather than handed back. The token is spent either way, so there is
nothing to retry with.

## `resident`

**You assert** that a `resident` object is only reached from the thread that
built it, and that either its leak at isolate exit is acceptable or you call
`dispose()` before the isolate ends.

The first half is what lets a resident type skip the `Send` bound. Codegen
closes the glue's own ways to break it (the constructor runs on the caller;
the handle is freed through a `dart:core` `Finalizer` on the isolate that
attached it). It cannot control your scheduling: **a Dart isolate is not pinned
to an OS thread.** Within one event-loop turn the thread is fixed, so a
synchronous stretch is always safe; across turns it depends on the isolate, and
an `Isolate.spawn`ed one does move.

The second half is the price of `!Send`: only the owning thread may drop the
value, so an isolate that exits while holding one leaks it for the life of the
process. A Flutter hot restart is the usual way to get there.

**The thread rule is checked, not trusted.** A call from another thread throws
`BridgePanicException`; a `dispose()` from another thread throws `StateError`
and strands the object rather than running its `Drop` on the wrong thread.
Stranded objects are counted in the resident leak report.

## Build-level opt-ins

These are Bazel platform or compiler-flag choices, not attributes.

**Threaded wasm** (`platform = "@frustrate//bazel:wasm32_threads"`). You assert
that your deployment serves COOP/COEP headers and that the custom-std builder
has been run for the `atomics` flavour against the pinned nightly. Bazel cannot
see response headers or what the builder left on disk. Both failures are loud:
a missing flavour fails at Bazel load time naming the build command; missing
headers fail when shared memory is instantiated. Nothing falls back to
single-threaded.

**wasi or the browser std** (`wasm32_wasip1` / `wasm32_browser_std`). You assert
that the facilities you named are the ones your dependency graph reaches, which
only an attempted build can reveal. `bazel/wasm_std_check` catches some misses
afterwards but is a tripwire, not a proof — its source says which facilities it
covers and why `Instant::now` is not one. An uncovered miss aborts at runtime.

**dart2js/DDC development loop** (`-Dfrustrate.allowJsNumbers=true`). Those
backends make `int` a JS double. The flag lets `i64`, `Vec<i64>`, `[i64; N]`
and handles cross there through a fallback that is exact up to 2^53 and throws
beyond it. You assert that you have checked your program's numeric behaviour on
a real 64-bit backend, because nothing can tell you whether your own arithmetic
went wrong before reaching the bridge (`1 << 62` is `0` there). `Vec<i64>` and
`[i64; N]` are `I64List` in Dart, a type the build chooses for the compiler:
`Int64List` on the VM and dart2wasm, `List<int>` on dart2js and DDC, which cannot
construct an `Int64List`. Under Bazel the compiler is the target platform's
`@rules_dart//dart/constraints:web_compiler`, which is `js` or `wasm` on the
platforms `dart_js_binary` and `dart_wasm_binary` build for. A platform without
it gets `Int64List`, dart2js included, and a
`Vec<i64>` then throws there. `toI64List(list)` makes an `I64List` of a
`List<int>`: the same list if it is one already, a copy if not. A
`bytes(...)` codec that calls `getInt64` itself cannot work there.
`u64` is unaffected; it crosses as `BigInt` everywhere.

## Why the rest are decisions

The six representation keywords (`data`, `confined`, `resident`, `frozen`,
`locked`, `actor`) carry no unchecked claim: rustc proves each model's thread
bound, an `actor`'s object never leaves its executor, and `resident`'s rule is
checked on every access. The same holds for `sync`, `getter`,
`dart_identifier`, `web = "runtime_fail"`, `inbound`, `no_eq`,
`dart_interface`, `skip` and the use-site markers (`Locked<T>` and friends).

One non-obvious refusal: a type declaring both `data` and a handle model gets
two Dart classes, but no conversion between them. Reading a handle into a value
would have to pick between a synchronous read, which needs a contention
contract, and an async one; minting a handle from a value would have to assume
`Clone`. Neither is a choice codegen should make for you.
