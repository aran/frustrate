/// A Rust producer whose consumer isolate is gone is *refused*, not fatal
/// (runtime/rust/src/post.rs).
///
/// This is a defect that stood until 2026-07-24. Native completions
/// were delivered by invoking a `NativeCallable.listener` function pointer, and
/// invoking one whose isolate has been torn down is a hard VM abort:
///
///     runtime_entry.cc: error: Callback invoked after it has been deleted.
///
/// SIGABRT — so the pre-fix signature of every test here is not a failed
/// expectation but a dead test process, which is why this file is its own
/// target: an abort takes the whole runner down with it, and it must not be
/// able to mask sibling suites.
///
/// Native-only by construction. Web has no isolates: the consumer is the page,
/// which cannot go away underneath a producer while the module runs.
@TestOn('vm')
library;

import 'dart:async';
import 'dart:isolate';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  setUpAll(initBridge);

  /// The control. Everything else here asserts that a *dead* consumer is
  /// refused; this asserts the fix does not over-fire and start refusing live
  /// ones. Without it, "always return false" would pass the whole file.
  test('a parked sink whose isolate is alive still accepts', () async {
    final got = <int>[];
    final c = StreamController<int>();
    final sub = c.stream.listen(got.add);
    parkSink(sink: c);

    expect(pushParked(value: 7), 1, reason: 'a live consumer takes the item');
    await Future<void>.delayed(const Duration(milliseconds: 50));
    expect(got, [7]);
    expect(parkedSinkCount(), 1, reason: 'and the sink is kept, not pruned');

    // Cancelling retires it the ordinary way — the same refusal path a dead
    // isolate takes, which is why the two look identical to the producer.
    await sub.cancel();
    expect(pushParked(value: 8), 0, reason: 'a cancelled sink refuses');
    expect(parkedSinkCount(), 0, reason: 'and is pruned');
  });

  test('a sink outliving its isolate is refused and pruned, not fatal', () async {
    // Deterministic by construction: the child parks the sink, we kill it, and
    // we wait for its exit listener before pushing anything. No race between a
    // live producer and a dying consumer — by the time the first post is
    // attempted, the owning isolate is provably gone.
    //
    // Killed rather than left to exit on its own, and the reason is contract
    // (b) doing its job: the parked stream is an open registration, so it pins
    // its isolate and the child would sit there forever. A forced kill is the
    // one way an isolate ends with a live stream still registered — which is
    // also precisely what `Isolate.kill` and Flutter hot restart do.
    final ready = ReceivePort();
    final exited = ReceivePort();
    final child = await Isolate.spawn(_parkAndWait, ready.sendPort);
    child.addOnExitListener(exited.sendPort);
    await ready.first;
    ready.close();
    child.kill(priority: Isolate.immediate);
    await exited.first;
    exited.close();

    expect(
      parkedSinkCount(),
      1,
      reason: 'the dead isolate left its sink behind',
    );

    // Pre-fix this line aborted the process. It must now report the refusal.
    expect(pushParked(value: 1), 0, reason: 'a dead isolate accepts nothing');
    expect(
      parkedSinkCount(),
      0,
      reason: 'and the producer prunes the sink it can no longer reach',
    );
  });

  /// The Rust half of the channel-handle reclaim. Its Dart half — an item the
  /// router absorbs — is in `channel_handle_test.dart`; this is the other
  /// refusal, and the only one with no web counterpart (there are no isolates
  /// on web; the consumer is the page).
  ///
  /// The item is *minted before the post is attempted*, so a refusal that did
  /// nothing else would leave a Rust object alive with its only Dart wrapper
  /// never built. `chitsLive()` is what sees that; nothing else can.
  test('a handle item refused by a dead isolate is given back', () async {
    final ready = ReceivePort();
    final exited = ReceivePort();
    final child = await Isolate.spawn(_parkChitAndWait, ready.sendPort);
    child.addOnExitListener(exited.sendPort);
    await ready.first;
    ready.close();
    child.kill(priority: Isolate.immediate);
    await exited.first;
    exited.close();

    expect(chitsLive(), 0, reason: 'parking a sink mints nothing');
    expect(
      pushParkedChit(value: 1),
      0,
      reason: 'a dead isolate accepts nothing',
    );
    expect(
      chitsLive(),
      0,
      reason: 'the refused item freed the object it had already minted',
    );
    expect(clearParkedChitSinks(), 0, reason: 'the refusal pruned the sink');
  });

  /// The same refusal on the **reply** path, which is not a channel at all.
  ///
  /// A dispatched member's answer is posted rather than returned in the
  /// caller's frame, so the isolate that asked for it can be gone by the time
  /// it arrives — and the handle in that answer was minted while encoding,
  /// which is before the post. Only a `#[bridge(sync)]` return is exempt: its
  /// reply travels back inside the call.
  ///
  /// Deterministic by construction, like the sink cases above, but the gate is
  /// the other way round: `gatedChit` parks *before* it mints, so the isolate
  /// is provably dead before a single object exists, and the mint provably
  /// happens afterwards.
  test('a returned handle refused by a dead isolate is given back', () async {
    final before = chitsLive();

    final ready = ReceivePort();
    final exited = ReceivePort();
    final child = await Isolate.spawn(_callGatedChitAndWait, ready.sendPort);
    child.addOnExitListener(exited.sendPort);
    await ready.first;
    ready.close();

    // The call is in flight on a pool thread and has minted nothing yet.
    while (chitGateParked() == 0) {
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    expect(chitsLive(), before, reason: 'the mint is behind the gate');

    child.kill(priority: Isolate.immediate);
    await exited.first;
    exited.close();

    // Now let the body finish. It mints, encodes into the reply, and posts
    // into an isolate that is gone.
    openChitGate();
    while (chitGateParked() != 0) {
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    // The gate counter clears *after* the mint, so the object provably
    // existed; nothing but the reclaim can take the count back down, because
    // the isolate that would have wrapped and disposed it is gone. A deadline
    // poll rather than a fixed sleep: a leak never converges, so this cannot
    // go green early, and a slow machine cannot make it go red.
    final deadline = DateTime.now().add(const Duration(seconds: 10));
    while (chitsLive() != before && DateTime.now().isBefore(deadline)) {
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    expect(
      chitsLive(),
      before,
      reason: 'the refused reply freed the handle it had already minted',
    );
  });

  test('killing an isolate mid-stream stops its producer instead of aborting', () async {
    // The forced-exit window, and the one that matters most in practice: this
    // is what Flutter hot restart does to an app with an open watch stream.
    // Unlike the parked-sink test this one does catch a producer in flight —
    // `streamUntilCancelled` posts on a millisecond cadence for 20k items — so
    // the kill lands squarely mid-stream.
    final ready = ReceivePort();
    final exited = ReceivePort();
    final child = await Isolate.spawn(_streamThenWait, ready.sendPort);
    child.addOnExitListener(exited.sendPort);
    await ready.first;
    ready.close();

    child.kill(priority: Isolate.immediate);
    await exited.first;
    exited.close();

    // Surviving is the assertion. Give the orphaned producer room to post into
    // the isolate it has just lost — pre-fix, one such post is fatal.
    await Future<void>.delayed(const Duration(seconds: 1));

    // And it stopped rather than spinning out its remaining ~20k iterations:
    // the refusal retires the channel, so `add` returns false and the producer
    // takes its cancelled branch (test_api's `stream_until_cancelled`).
    expect(
      cancelObserved(),
      isTrue,
      reason: 'a refused post must stop the producer, not just spare it',
    );
  });
}

/// Opens a stream, parks it where another isolate can push, and waits to be
/// killed. It cannot simply return: the registration pins it.
Future<void> _parkAndWait(SendPort reply) async {
  await initBridge();
  parkSink(sink: StreamController<int>());
  reply.send('parked');
  await Completer<void>().future;
}

/// [`_parkAndWait`] for a sink whose items carry handles.
Future<void> _parkChitAndWait(SendPort reply) async {
  await initBridge();
  parkChitSink(sink: StreamController<Chit>());
  reply.send('parked');
  await Completer<void>().future;
}

/// Starts a dispatched call that will answer with a handle, and waits to be
/// killed while it is still parked in Rust.
///
/// The `Chit` never arrives here — that is the point — so the `then` is only
/// what a real caller would have written; it is unreachable.
Future<void> _callGatedChitAndWait(SendPort reply) async {
  await initBridge();
  unawaited(gatedChit(n: 1, maxWaitMs: 10000).then((c) => c.dispose()));
  reply.send('calling');
  await Completer<void>().future;
}

/// Starts a real producer and stays alive until killed.
Future<void> _streamThenWait(SendPort reply) async {
  await initBridge();
  final c = StreamController<int>();
  var seen = 0;
  c.stream.listen((_) => seen++);
  unawaited(streamUntilCancelled(sink: c));
  while (seen < 3) {
    await Future<void>.delayed(const Duration(milliseconds: 5));
  }
  reply.send('streaming');
  await Completer<void>().future; // killed from the parent
}
