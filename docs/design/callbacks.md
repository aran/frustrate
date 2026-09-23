# Dart-object handles (Dart → Rust)

An opaque handle lets Dart hold a Rust object. A **Dart-object handle** is the
reverse: a Rust value standing for a Dart object whose methods Rust can call.
`StreamSink`, `DartCallback`, `DartFunction` and the `dart::` mirrors
(`runtime/rust/src/dart.rs`, `runtime/rust/src/callback.rs`) are all this one
mechanism, so there is no separate stream path or callback path to keep in step.

A handle is data: its Dart-minted id travels inline where the value sits, so it
nests in a struct, rides a `Vec`, appears several times per function and sits
beside a real return value, all through the ordinary codecs. Dart passes a real
`StreamController`, `Sink` or closure.

A handle is argument-only (FR0031). Rust cannot mint a Dart object, and keeping
handles out of returns, stream items and callback results means the Rust-encode
and Dart-decode arms of a handle-bearing type are provably dead, so they are not
generated.

## Contract for users

- **`dispose()` is mandatory** on a Rust handle that stores a sink or callback.
  On native a registered handle keeps its isolate alive until Rust drops it
  ([streams.md](streams.md#open-streams-pin-the-isolate-native)).
- **A mirror method never runs inside the Rust call that posts to it**, on any
  platform. Re-entering the bridge from a callback is legal.
- **A void Dart method that throws** terminates that channel: Rust's next send
  returns `false`, and the error goes to the current zone. There is no reply
  frame to carry it, and swallowing it is not acceptable.
- **A `DartFunction`'s Dart side is `R Function(T)`**: it answers synchronously
  and cannot itself wait. Anything Dart must wait for is a request out on a
  `DartCallback` and a separate bridged call back in.
- **A returning closure is blocking-only outside `async fn`.** `call_async` parks
  a future and works everywhere; `call` blocks a worker thread and is native-only.
  A member is portable exactly when it can `.await`. Actor methods cannot be
  `async fn`, so returning callbacks from actors are native-only: on a web actor
  the blocked worker could never run the `onmessage` that delivers the answer.

## A Dart closure that can refuse

`DartFunction<T, Result<R, E>>` lets the Rust body handle a Dart failure as a
value, which is what retry, fallback and "ask Dart, tolerate refusal" need.

- `E` must be a bridged struct or enum. `Result<R, String>` and
  `anyhow::Result<R>` are refused (FR0043): on the return path Rust authors the
  `Err` string, but here nobody does, and accepting any throw as a message
  would turn every Dart bug into a plausible business value.
- **An undeclared throw stays loud.** Only a throw the generated binding
  recognises as `E` becomes `Err`; anything else, and an isolate that died
  owing an answer, is a panic on the enclosing call — never a plausible refusal.
- **The Dart type says so.** Dart has no checked exceptions, so the parameter is
  a generated alias naming the error (`RefusalErrorFallible<int Function(int)>`
  over `typedef XFallible<F extends Function> = F`), visible on hover and
  wrapping the closure type whole.
- Fallibility is in the schema fingerprint, so a stale binding fails at init
  rather than meeting an error it cannot decode.

## Delivery never runs on the Rust stack

Native delivery is a `Dart_PostCObject` message, drained on a later turn. On wasm
`frustrate.post` is an import the producer calls *inside* its own export, so the
router defers every delivery — items, terminals, void and returning methods — to
one FIFO microtask queue. One queue for everything keeps a close from overtaking
its own items.

Without deferral, a user `Sink.add` that calls back into the same Confined
handle gets a second `&mut` aliasing the first: undefined behaviour, observed as
a use-after-free on both web configs (`tests/dart_integration/test/sink_reentrancy_test.dart`).
Deferral was chosen over refusing re-entry because re-entry is legitimate and
works on native.

**Nothing enforces this.** It rests on every path out of `StreamRouter.deliver`
deferring. A new seam that reaches user Dart from a Rust-initiated post must go
through `deliver` or argue why it cannot re-enter. Generated Dart must not add
its own `scheduleMicrotask` around a user closure either: the router's try/catch
would no longer surround the user's code, and the void-throw policy would stop
applying.

## A multi-method Dart object

A struct whose fields are handles is a multi-method Dart object, with no extra
mechanism. `#[bridge(data, dart_interface)]` emits it as an
`abstract interface class` the caller implements, so a missing method is a
compile error naming it, methods share state, and a top-level tuple argument
spreads into real positional parameters. The encoder tears off each method, and
a tear-off of `void note(String)` *is* a `void Function(String)`, so nothing
below the Dart surface knows the form exists.

Decisions:

- **Closure mirrors only (FR0050).** A `Sink`-shaped field has no method on the
  caller's class to be, and a data field would need a constructor an interface
  lacks.
- **One returning method makes every member taking the interface a potential
  waiter.** The checker cannot see which methods a body calls.
- **One channel per method.** A throwing void method terminates only its own
  channel, and dropping one moved-out field retires only that method. Each field
  is an independent capability; forbidding that would forbid composition.
- **One registration carrying all methods is not built.** It would give
  cross-method ordering and atomic retirement, but the router keeps returning
  and void registrations apart, and merging them means reworking the
  unenforced delivery seam above. No ordering is promised between methods.

## An outstanding invocation ends when its isolate does (native)

There is no deadline: the caller asked for a value only the closure has. But the
wait ends as soon as the isolate that owes the answer is gone. A refused post
sweeps every waiter that isolate owes; an invocation the isolate accepted before
dying is caught because each isolate registers `addOnExitListener` against a
Rust-owned port at init. It is a push rather than a poll because a suspended
future has no thread to poll from.

This is liveness, not readiness: a wedged isolate is alive and still owes its
answer. Two consequences:

- **Dart may not accept an invocation without answering it.** An unknown id is
  answered with the error envelope rather than thrown; a throw would leave a live
  isolate holding a waiter nothing can detect (and Flutter swallows root-zone
  throws).
- **The exit notice is not the only detector, and need not be.** `exit()` ends
  the process with every waiter; an isolate killed before registering has
  allocated no ids; one that never installed the bridge owns nothing.
