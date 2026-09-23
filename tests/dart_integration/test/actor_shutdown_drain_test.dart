/// Does `dispose()` abandon work that was already submitted?
///
/// It was once believed that web's shutdown is abrupt where native's drains: `frustrate_actor_shutdown` sends a Stop marker the host
/// thread reaches only after its FIFO queue (actor.rs), while
/// `_WebActorHost.shutdown` calls `terminate()` and then fails whatever is
/// still pending. Stated that way it reads as a divergence in what a caller
/// observes — work that completes on native and does not on web.
///
/// This suite asks whether a caller can actually reach it. It runs on every
/// fixture — native, single-threaded web, threaded web — and asserts the
/// contract the generated `dispose()` documents: *"In-flight calls complete
/// first (the executor is a FIFO)."*
///
/// It holds on web despite web having no drain, for three reasons that
/// compose:
///
///  - An actor member cannot be a Rust `async fn` (codegen/src/check.rs
///    rejects it), so the worker's wasm dispatch arm runs the body and posts
///    its response in a single statement. A dequeued call is a finished call;
///    there is no dequeued-but-outstanding state for `terminate()` to cut.
///  - `postMessage` is FIFO in both directions, so every earlier call's
///    response is already on the wire ahead of the drop call's.
///  - The generated `dispose()` clears its handle *before* issuing the drop
///    call, so the in-flight set cannot grow after that point (pinned below).
///
/// Together those make `_pending` empty by the time `terminate()` runs, which
/// makes `_WebActorHost.shutdown`'s `failAll` a backstop against a bridge bug
/// rather than a path generated code can take.
///
/// What this suite does *not* cover, because the fixture surface cannot
/// produce it: an actor that stores a `StreamSink` clone in its instance's
/// globals, outliving the drop. That clone dies with the worker's memory on
/// web and survives on native — a real asymmetry, but one that runs the
/// opposite direction and follows from per-instance memory rather than from
/// `terminate()`.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  tearDown(() {
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason:
          'a disposed actor must leave no channel registered. Open: '
          '${Frustrate.instance.openChannelLabels}',
    );
  });

  test('calls submitted before dispose all complete', () async {
    // The claim under test, in its strongest form: the first call is slow
    // enough to still be running when `dispose()` is called, so this is not
    // merely a queue that happened to be empty.
    final m = await Miner.new_(label: 'drain');
    final inflight = [
      m.nthPrime(n: 2000),
      m.nthPrime(n: 100),
      m.nthPrime(n: 10),
    ];

    await m.dispose();

    final results = await Future.wait(inflight);
    expect(results[1], 541);
    expect(results[2], 29);
    expect(
      results[0],
      greaterThan(541),
      reason:
          'the slow call must have produced a real answer, not been '
          'cut short by the executor going away',
    );
  });

  test(
    'the drop call is FIFO-last, so it observes every earlier call',
    () async {
      // The mechanism behind the test above, asserted directly: `calls()` is
      // enqueued after the batch and before dispose, and reads the effect of
      // all of them. If shutdown could overtake the queue on any platform, this
      // is where it would show.
      final m = await Miner.new_(label: 'fifo-drop');
      final inflight = [for (var i = 0; i < 8; i++) m.nthPrime(n: 50)];
      final counted = m.calls();

      await m.dispose();

      await Future.wait(inflight);
      expect(await counted, 8);
    },
  );

  test('a call after dispose is refused rather than queued', () async {
    // Why the in-flight set is closed: `dispose()` nulls the handle before
    // issuing the drop call, so nothing can join the queue behind it. Without
    // this the two tests above would only describe a race they happened to
    // win.
    final m = await Miner.new_(label: 'refused');
    await m.dispose();
    expect(() => m.nthPrime(n: 10), throwsStateError);
  });

  test('a refused call does no encoding, on either platform', () async {
    // The refusal has to come BEFORE the encoder runs, not after it. Order is
    // observable and it matters: the encoder is the one place a caller's own
    // code runs inside a bridge call (a `BytesCodec.toBytes` hook, a large
    // payload write), so encoding first means a shut-down host still does the
    // caller's work — and, if that work throws, reports the encoder's error
    // instead of the shutdown that actually refused the call.
    //
    // Web has always checked first. Native encoded inside `_issueAsync` and
    // checked in the send closure it calls afterwards, so this asserts the two
    // platforms now agree. `spawnActorHost` is the seam: a generated method
    // builds its own encoder, and this needs one it can watch.
    final host = await Frustrate.instance.spawnActorHost(
      debugName: 'no-encode',
    );
    await host.shutdown();

    var encoded = false;
    await expectLater(
      host.call(146, 16, (w) {
        encoded = true;
        w.writeI64(1);
      }),
      throwsA(isA<StateError>()),
    );
    expect(
      encoded,
      isFalse,
      reason:
          'a host that is going to refuse must refuse before it runs '
          'the caller\'s encoder',
    );
  });

  test(
    'a stream opened by an actor method is retired before dispose returns',
    () async {
      // The other half of `shutdown`'s cleanup. `mine_progress` streams from
      // inside the executor and drops its sink when the method returns, so the
      // consumer sees `done` on its own — the teardown ledger then proves
      // shutdown had nothing left to retire.
      final m = await Miner.new_(label: 'streaming');
      final items = <int>[];
      final done = Completer<void>();
      final c = StreamController<int>();
      c.stream.listen(items.add, onDone: done.complete);

      await m.mineProgress(rounds: 3, n: 50, sink: c);
      await done.future;

      expect(items, hasLength(3));
      await m.dispose();
    },
  );
}
