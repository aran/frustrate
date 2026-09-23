# Streams (Rust → Dart)

A stream is a [Dart-object handle](callbacks.md) whose Dart type is
`StreamController<T>`. A member takes it as an ordinary parameter:

```rust
#[bridge] impl TextDoc {              // Confined; the watch pattern
    #[bridge(sync)]
    pub fn watch(&mut self, sink: StreamSink<TextPatch>) {
        self.watchers.push(sink);      // stored; splice() pushes later
    }
}
```

```dart
void watch({required StreamController<TextPatch> sink})
```

## Why this shape

- **No sugar returning `Stream<T>`.** Rewriting a sink-taking, unit-returning
  member into one returning a stream would give two members that differ only in
  whether they also return a value categorically different Dart surfaces. The
  caller writes the controller directly and so decides when the call is issued,
  and an opening-call failure stays the call's failure.
- **Push, not pull.** frustrate has no background poller, so an `impl Stream`
  return needs a driver it refuses to own, and an `impl Iterator` cannot express
  a stored watcher or a producer fed by callbacks — the main use cases.
- **One port per isolate.** Stream events ride the channel that completes calls.
  Port messages are FIFO per port and unordered across ports, so a second
  channel would lose the ordering of items against their own terminal and
  against the opening call's completion.
- **Incremental producers belong on an Actor.** Single-threaded web runs a body
  inline on the caller, so a producer on the main instance delivers everything
  during the call and cannot interleave with UI work.
- **An `Err` from the opening call is not pushed into the stream.** With several
  sinks legal on one member, which stream got it would depend on declaration
  order. The sink drops and its stream closes cleanly.

## Open streams pin the isolate (native)

While a stream or stored callback is registered, the isolate stays alive, exactly
as it would for a Dart producer — including the root isolate. A spawned isolate
that opens a stream and returns from its entry stays up to consume it. Web does
not need this: its consumer is the page.

**The symptom when this is a bug is a process that passes and never exits.**
`dart test` force-exits and hides it; Bazel's `dart_test` reports a **TIMEOUT
with an empty log**. Treat an empty-log TIMEOUT as a leaked sink until proven
otherwise. Assert `openChannelCount == 0` in teardown; `openChannelLabels` names
what is open.

**`dispose()` is mandatory.** A handle collected without `dispose()` closes each
stored channel with a `LeakedChannelError` naming the holder and the member that
opened it, but that needs a GC — and a pinned isolate that has returned from its
entry allocates nothing, so the collection never comes. `Sink`/`EventSink`
parameters and Rust-stored `DartCallback`s have no Dart-side release at all.

**A dead consumer is not an error.** `Isolate.kill`, `Isolate.exit`, hot restart
and process teardown ignore keep-alive. A post to a dead port is refused: the
channel retires and `add` returns `false`, exactly as after a cancel, so a
producer that honours `add`'s return stops by itself.

## Cancellation

Dart-side cancel is immediate and guaranteed: no further events reach the
listener. Rust sees it as `add` returning `false`, which is cooperative — soon
on native and threaded web, next turn on the single-threaded web main instance,
and only once the executor is idle on a web actor. A web-actor producer that
never returns never sees it. That split — delivery guaranteed, observation not —
matches what both languages already promise. Give each long-lived producer its
own actor.

## Terminals and panics

A failure produces one event. A producer panic on native drops the sink during
unwinding and closes the stream cleanly, with the panic reported on the call.
The rule is that unwinding must not suppress drop-retire, not that a failed call
closes its sinks: a sink handed to a detached thread still owns its stream.

Web leaves the stream open after a panic. Under `panic=abort` no destructor
runs, and closing it from Dart would re-enter an instance whose locks may still
be held, risking a wedge to save some producer cycles. The panic is still
reported on the call.

## Opaque items are freed exactly once

A stream item may carry an opaque handle, and something must free one the
consumer never takes. Exactly one party does: the Dart wrapper when delivered;
the producer's `Drop` if the channel was already cancelled (`add` checks before
encoding); a Rust reclaim if a live-looking post is refused; or a Dart reclaim
when the router absorbs an item for a cancelled or retired id. Bytes accepted by
an isolate that then dies go with that isolate, like every other handle it held.

## Backpressure

`add` never blocks and buffers in the Dart controller while paused. For
backpressure use `send(item).await`, which parks the task until the consumer
resumes; `StreamSink` also implements `futures::Sink`. A producer may overshoot a
pause by the items already in flight. Sync producers poll `is_paused()`.

Not covered: a slow consumer that never pauses, a controller nobody has
listened to yet, the single-threaded web main instance (the producer runs
inline, so `send` never parks), and web actors (same limits as cancel).
Broadcast controllers are rejected at bind, since they never pause.

## Testing a producer

`frustrate::testing::stream::<T>()` returns a real `StreamSink<T>` that records
instead of posting, plus a probe that plays the consumer. Everything but
delivery is the production code. It does not model the Dart side, codecs, or
keep-alive and GC timing, and its `probe.cancel()` takes effect immediately.
