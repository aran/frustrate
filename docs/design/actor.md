# Actor

An actor is a bridged object that runs on its own executor: a dedicated OS
thread on native, and on web a Worker running its own instance of the shared
wasm module. Callers reach it only by message, and every call is `async`.

## Why actors exist

In stable-Rust wasm, handles belong to one instance and code runs on one
thread, so an `async` member gives no parallelism there. Every body runs inline
on the thread that called into the instance, and for the page's instance that
is the main thread. The only way to run Rust off the web main thread on stable
is to put the whole object in a worker-hosted instance behind a message
channel. That is an actor, and it gives real parallelism on web without
nightly Rust.

The threaded build has a second way to get off the main thread: async bodies
dispatched to pool workers. Actors must keep working on stable, so they never
depend on it.

## Shared mutable state belongs in an actor

Rust-side global state has no bridge model, so the obvious approach is a
`static` behind a lock. Avoid it. On web, each executor has its own linear
memory, so each gets its own copy of the static. On threaded web, a lock that
both the main thread and a pool worker can reach is fatal on the main thread.
Put the state in an actor instead:

```rust
// instead of: static CACHE: Mutex<HashMap<String, i64>> = ...
#[bridge(actor)]
pub struct Cache { map: HashMap<String, i64> }
```

Methods get `&mut self` without a lock, because the actor handles one message at
a time. A lock makes you pay wherever it is contended, and worst on the web main
thread. An actor instead puts the object where only one thread can reach it,
and you pay in message passing.

**One owner.** Handles cannot cross an actor boundary, so two actors cannot
come to share an object. They can still reach the same `static`: native actors
are threads in one process and share it, while each web actor gets its own copy.
If two actors need the same state, one owns it and the others send it messages.
A design that needs memory shared between executors can only work on native.
The cost: every call becomes an `await`, and something must own the actor's
handle and `dispose()` it, where a global had no owner.

## The contract

- **Async only.** A sync call into an actor would block the caller on a round
  trip to another thread, which is the jank the model exists to avoid.
- **One message at a time, in arrival order.** Re-entrancy is impossible. The
  one exception is the completion of a `Deferred` method (below). Its body is
  still processed in order.
- **One instance, one executor.** Constructing an actor gives you a new
  executor. To get more parallelism, create more actors.
- **Only values cross the boundary.** No handle may appear in an actor method's
  parameters or return value, not even one owned by the actor's own instance,
  because a handle does not record which executor owns it. Constructors are the
  one exception. To send a large structure to an actor you send its bytes,
  because each instance has its own memory.

Because only values cross the boundary, spreading work across several actor
instances is always sound. `ActorPool` (`package:frustrate/workers.dart`)
builds on this. Each job leases one instance for its whole closure, not one
call at a time. So a job that makes several calls sees no calls from other jobs
in between, and there is no scheduling policy to tune: a job takes any idle
instance, or waits in a queue. For CPU-bound work this costs nothing, because
an actor runs one call at a time anyway.

## Slow work: `Deferred`

Because an actor handles one message at a time, **a method body that awaits
slow work blocks every later call to that instance until it finishes**. A
`Deferred<T>` return lets a method capture what it needs, hand the slow part to
the executor and free the instance. The mechanics are documented in
`runtime/rust/src/deferred.rs`. The contract an author relies on:

- **Order.** Every method starts in arrival order, and a non-deferred method
  finishes before the next one starts. A deferred completion can happen at any
  time after its method returns: alongside later methods, and in any order
  relative to other deferred completions. So two Dart `await`s may resolve out
  of call order. Access to the actor's own state (`self`) is still one call at a
  time, because the deferred future cannot borrow `self`.
- **On web, `Deferred` frees the actor only while the future is waiting.** On
  native the future runs on the pool, in parallel with the actor's thread. On
  web it runs on the actor's own worker thread, between messages. So a deferred
  future that computes for a long time still blocks a web actor. Put long
  computation in the method body, or in another actor.
- **`dispose()` cancels outstanding deferred calls instead of waiting for them.**
  Waiting would make `dispose()` take as long as the slow work, which is exactly
  the blocking `Deferred` exists to remove. Each cancelled future fails with a
  `StateError` naming the actor type. A call that has already answered keeps
  its answer.

## Long work: `Addr`

A method that must do long work on `self` can do it in slices. An actor
constructor or method may declare an `Addr<Self>` parameter, which the executor
supplies and Dart never sees. `addr.post(|a| ...)` queues a closure as a new
message to the actor; each slice posts the next, and calls that arrive meanwhile
run in between. `addr.call(|a| ...)` does the same and returns a future of the
closure's result, for a `Deferred` future or a spawned task that must apply
something to the actor. The mechanics are in `runtime/rust/src/addr.rs`.

- **Order.** A post is a message of its own: it runs after the message that
  posted it has finished, and posts run in the order they were made. Natively
  it goes on the host's queue with the calls, so it runs after every message
  already there and before every later one. On web each post is a task the
  worker sends itself through a `MessageChannel` of its own, so the worker
  returns to its event loop between posts and the browser runs whatever else
  has arrived. Calls that reach the worker during a slice therefore run
  before the job's next slice or soon after; `addr_test` counts how many
  slices a call waits behind a job that keeps posting. Which of a call and a post runs
  first is not fixed on web, so do not rely on it. (A `MessageChannel` rather
  than `setTimeout(0)`, which browsers clamp to 4 ms once timers nest.)
- **Teardown.** A closure whose turn comes after the actor was disposed or
  consumed is dropped without running; so is one queued behind the stop, and
  one posted to a stopped executor. `call` then resolves to `Err(Gone)`. On web
  the worker's termination takes its waiting closures with it.
- **Panics.** A posted closure, or a task the actor spawns, answers no call.
  If it panics, the panic is an uncaught error in the zone the actor was built
  in, and a `call` waiting on the closure resolves to `Err(Gone)`. The actor
  keeps serving. On web the same goes for a panic in a wasm-bindgen closure the
  browser calls (an event handler, a timer), and a promise rejection nothing
  handles is an uncaught `BridgeException` in that zone. A panic hook of your
  own in place of frustrate's leaves only the trap's text in the message.
- **No waiting on yourself.** Awaiting `call` on the actor's own executor
  (a `block_on` in a method body) panics at once, because the executor that
  would run the closure is the one waiting.

## The web channel: `postMessage` with transferred buffers

A web actor is a Worker running a small JS loop and its own instance of the
module. Requests and replies are `ArrayBuffer`s transferred with `postMessage`.
The Rust code does not know it is running in a worker.

This is the only mechanism. There is no fallback, and no switching by payload
size. A single-producer, single-consumer byte ring in shared memory
(SharedArrayBuffer) was considered and rejected:

- **It is never faster.** For small payloads the ring and `postMessage` are
  equally fast. As payloads grow, the ring falls behind, while a transferred
  buffer costs the same at any size because it moves ownership instead of
  copying. The one case that might have favoured the ring, delivering a reply
  to a busy main thread, does not: a reply through the ring and a `postMessage`
  reply both wait in the same event-loop queue.
- **`postMessage` gives the required guarantees directly.** Transferring the
  request detaches it at once, so the caller cannot touch a request that has
  been sent. Messages arrive in order on each channel, which is the order the
  actor needs. And it works without cross-origin isolation.
- **Shared memory would not remove a copy.** A stable-Rust wasm instance can
  read and write only its own linear memory, so no zero-copy path exists between
  instances. On the threaded build the main thread can view the actor's memory,
  but that still saves nothing. Writing the request directly into shared memory
  turns every scalar write into a JS `DataView` store. No `BinaryReader` may be
  built over a SharedArrayBuffer, so the reply is copied out before it can be
  decoded. The number of copies stays the same; the change is that they happen
  on the main thread, which is the thread the actor exists to protect.

Native uses a dedicated thread with a queue of requests. A queue on the shared
pool would also keep calls in order. A dedicated thread matches the web
behaviour, and it makes blocking inside an actor method harmless.
