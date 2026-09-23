/// A handle crossing a **channel**: delivered items are ordinary owned
/// handles, and an item that is never delivered gives its Rust object back.
///
/// `chitsLive()` is the whole instrument. Nothing else can see this: a minted
/// object with no Dart wrapper is invisible to `openChannelCount` (which counts
/// channels) and to the leak terminal (which names an abandoned channel, not an
/// abandoned object). So every test here ends by asserting the count is back to
/// zero, and a reclaim that silently did not happen fails as a number.
///
/// **The cancel case is deterministic, not a race.** The producers are
/// `#[bridge(sync)]`, so the whole burst is posted before the call returns; the
/// consumer cancels in that same turn, before anything drains. Every item then
/// arrives for an id the router has tombstoned and takes the absorb path, which
/// is exactly the path the reclaim exists for. A test that streamed and hoped
/// to catch a live producer mid-flight would only *probably* exercise it.
///
/// Portable: native and both web variants. The Rust-side half — a post the
/// consumer isolate refuses — has no web counterpart (there are no isolates;
/// the consumer is the page) and lives in `isolate_death_test.dart`.
library;

import 'dart:async';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  /// No test may start owing objects to another, and none may leave any.
  setUp(() => expect(chitsLive(), 0, reason: 'a previous test leaked a Chit'));
  tearDown(() => expect(chitsLive(), 0, reason: 'this test leaked a Chit'));

  group('delivered', () {
    test('a streamed handle is an ordinary owned handle', () async {
      final c = StreamController<Chit>();
      final got = <Chit>[];
      final done = c.stream.listen(got.add).asFuture<void>();
      chitsTo(n: 3, sink: c);
      await done;

      expect(got.map((h) => h.n()), [
        0,
        1,
        2,
      ], reason: 'each item is a live Rust object the call can reach');
      expect(chitsLive(), 3, reason: 'and Rust still owns all three');
      for (final h in got) {
        h.dispose();
      }
      expect(got.every((h) => h.isDisposed), isTrue);
    });

    test('a Vec item transfers element by element', () async {
      final c = StreamController<List<Chit>>();
      final got = <List<Chit>>[];
      final done = c.stream.listen(got.add).asFuture<void>();
      chitBatches(batches: 2, per: 3, sink: c);
      await done;

      expect(got.length, 2);
      expect(
        [for (final b in got) b.map((h) => h.n()).toList()],
        [
          [0, 1, 2],
          [3, 4, 5],
        ],
      );
      expect(chitsLive(), 6);
      for (final b in got) {
        for (final h in b) {
          h.dispose();
        }
      }
    });

    test(
      'a handle inside a declared item type arrives with its data',
      () async {
        final c = StreamController<ChitNote>();
        final got = <ChitNote>[];
        final done = c.stream.listen(got.add).asFuture<void>();
        chitNotes(n: 2, sink: c);
        await done;

        expect(got.map((n) => n.note), ['note 0', 'note 1']);
        expect(got.map((n) => n.chit.n()), [0, 1]);
        for (final n in got) {
          n.chit.dispose();
        }
      },
    );

    test('a fire-and-forget closure receives owned handles', () async {
      final got = <Chit>[];
      chitsToClosure(n: 3, cb: got.add);
      await pumpEventQueue();

      expect(got.map((h) => h.n()), [0, 1, 2]);
      for (final h in got) {
        h.dispose();
      }
    });

    test('a returning closure receives an owned handle', () async {
      // The argument is the closure's to dispose; the answer is ordinary data.
      final weight = await weighChit(
        n: 7,
        f: (chit) {
          final n = chit.n();
          chit.dispose();
          return n * 2;
        },
      );
      expect(weight, 14);
    });
  });

  group('absorbed', () {
    /// Cancel before a single item drains. Every one of them was minted, and
    /// the wrapper that would have disposed them is never built — so the
    /// router's reclaim is the only thing standing between this and five leaked
    /// Rust objects.
    test('items dropped by a cancelled subscription are reclaimed', () async {
      final c = StreamController<Chit>();
      final sub = c.stream.listen((_) => fail('nothing may be delivered'));
      chitsTo(n: 5, sink: c);
      expect(chitsLive(), 5, reason: 'the producer minted all five');

      await sub.cancel();
      await pumpEventQueue();
      // tearDown asserts the count; this says which reclaim was under test.
      expect(chitsLive(), 0, reason: 'every absorbed item was given back');
    });

    test('an absorbed Vec item frees every element', () async {
      final c = StreamController<List<Chit>>();
      final sub = c.stream.listen((_) => fail('nothing may be delivered'));
      chitBatches(batches: 2, per: 4, sink: c);
      expect(chitsLive(), 8);

      await sub.cancel();
      await pumpEventQueue();
      expect(chitsLive(), 0, reason: 'the reclaim walked the length prefix');
    });

    test('an absorbed Option item frees only the present ones', () async {
      final c = StreamController<Chit?>();
      final sub = c.stream.listen((_) => fail('nothing may be delivered'));
      chitsOrNone(n: 5, sink: c);
      expect(chitsLive(), 3, reason: 'every second item is None');

      await sub.cancel();
      await pumpEventQueue();
      expect(chitsLive(), 0, reason: 'the reclaim read the presence tag');
    });

    test('an absorbed declared item frees the handle in its field', () async {
      final c = StreamController<ChitNote>();
      final sub = c.stream.listen((_) => fail('nothing may be delivered'));
      chitNotes(n: 4, sink: c);
      expect(chitsLive(), 4);

      await sub.cancel();
      await pumpEventQueue();
      expect(chitsLive(), 0, reason: 'the per-declaration reclaim ran');
    });

    /// A cancel *between* bursts: the first stream's items are delivered and
    /// owned, the second's are absorbed and reclaimed. The two coexist without
    /// either interfering with the other's accounting.
    ///
    /// Deliberately not a cancel *during* one burst. A `StreamController` whose
    /// subscription has been cancelled silently discards whatever is already
    /// handed to `add`, so an item in that window becomes a Dart wrapper the
    /// consumer's own controller dropped — collected by its finalizer, not by
    /// the router, which cannot see the difference (`t.hasListener` is also
    /// false for a controller nobody has listened to *yet*, where buffering is
    /// correct). How that window falls is the controller's timing and differs
    /// between native and web, so a test that pinned a split would be pinning
    /// the platform.
    test(
      'a delivered stream and an absorbed one do not confuse each other',
      () async {
        final kept = StreamController<Chit>();
        final got = <Chit>[];
        final done = kept.stream.listen(got.add).asFuture<void>();
        chitsTo(n: 3, sink: kept);
        await done;
        expect(chitsLive(), 3);

        final dropped = StreamController<Chit>();
        final sub = dropped.stream.listen((_) => fail('nothing may arrive'));
        chitsTo(n: 4, sink: dropped);
        expect(chitsLive(), 7, reason: 'both bursts minted');
        await sub.cancel();
        await pumpEventQueue();

        expect(chitsLive(), 3, reason: 'only the absorbed burst was reclaimed');
        expect(got.map((h) => h.n()), [
          0,
          1,
          2,
        ], reason: 'and the delivered handles are still usable');
        for (final h in got) {
          h.dispose();
        }
      },
    );
  });
}
