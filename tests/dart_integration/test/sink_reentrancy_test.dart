/// A caller-supplied `Sink` implementation must NOT run inside the Rust call
/// that posts to it — on any config.
///
/// On wasm there is no port and no queue: `frustrate.post` is an import the
/// producer calls inside its own export (runtime/rust/src/post.rs), so
/// `_onPost` -> `StreamRouter.deliver` -> the emitted `(r) => t.add(...)`
/// mirror -> user code would all run on the Rust stack. A user `add` that
/// calls back into the handle it was handed then gets a *second*
/// `handle::confined_mut` (codegen/src/emit_rust.rs) aliasing the first — two
/// live `&mut` to one object, silent memory corruption rather than a throw.
///
/// `StreamRouter._deferDelivery` is what prevents it: on web every delivery
/// runs on a microtask. This file pins the property on every config, and pins
/// that legitimate re-entrancy still WORKS — deferred, not refused.
///
/// Native prerequisite: `cargo build -p test_api`.
/// Web prerequisite: `dart run tool/build_web_fixture.dart`.
library;

import 'dart:async';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// A user-written `Sink` — NOT a `StreamController`'s. Its `add` re-enters
/// the probe it was handed to, which the public surface invites a caller to
/// do: any class implementing `Sink` is accepted.
class ReentrantSink implements Sink<int> {
  ReentrantSink(this.probe, {this.limit = 1});

  final ReentrantProbe probe;

  /// How many items may re-enter, so the repro cannot recurse without bound.
  final int limit;

  final List<int> received = [];

  /// What `note()` returned on each re-entry.
  final List<int> innerLens = [];

  /// Anything the re-entrant call threw, kept rather than swallowed: the fix
  /// must DEFER this call, not refuse it, and a refusal would be silent here
  /// otherwise.
  Object? refusal;

  @override
  void add(int data) {
    received.add(data);
    if (received.length > limit) return;
    try {
      innerLens.add(probe.note(v: 1000 + data));
    } catch (e) {
      refusal ??= e;
    }
  }

  @override
  void close() {}
}

void main() {
  setUpAll(initBridge);

  group('a user Sink never runs inside the call that feeds it', () {
    test(
      'its re-entrant call cannot land inside the &mut self borrow',
      () async {
        final probe = ReentrantProbe.new_();
        final sink = ReentrantSink(probe);

        final report = probe.feed(n: 3, out: sink);

        // The whole witness. `before != after` would mean a second `&mut
        // ReentrantProbe` was live inside this one and wrote through it — the
        // aliasing that made `feedFromLog` below return freed memory.
        expect(
          report.after,
          report.before,
          reason:
              'a second &mut wrote to this object inside the first one\'s '
              'borrow: user Sink.add ran on the Rust stack',
        );
        expect(report.before, 0);

        // Deferred, NOT refused: the re-entrant call still happens, just on a
        // later turn. This is what a re-entrancy latch would have broken.
        await pumpEventLoop();
        expect(sink.received, [0, 1, 2], reason: 'every item was delivered');
        expect(
          sink.refusal,
          isNull,
          reason: 'legitimate re-entrancy still works',
        );
        expect(sink.innerLens, [1]);
        expect(probe.log(), [1000], reason: 'the re-entrant note() did land');

        probe.dispose();
      },
    );

    test('and neither can a void closure, which lost its own hop', () async {
      // The closure twin. A void `DartCallback` mirror carries no
      // `scheduleMicrotask` of its own — such a hop would straddle
      // `StreamRouter._dispatch`'s try/catch and disable the
      // throw-terminates-the-channel policy for closures — so
      // `_deferDelivery` is the only guard on this path.
      final probe = ReentrantProbe.new_();
      final calls = <int>[];
      final innerLens = <int>[];
      Object? refusal;

      final report = probe.feedClosure(
        n: 3,
        out: (data) {
          calls.add(data);
          if (calls.length > 1) return;
          try {
            innerLens.add(probe.note(v: 2000 + data));
          } catch (e) {
            refusal ??= e;
          }
        },
      );

      expect(
        report.after,
        report.before,
        reason:
            'a second &mut wrote to this object inside the first one\'s '
            'borrow: the closure ran on the Rust stack',
      );
      expect(report.before, 0);

      await pumpEventLoop();
      expect(calls, [0, 1, 2], reason: 'every invocation was delivered');
      expect(refusal, isNull, reason: 'legitimate re-entrancy still works');
      expect(innerLens, [1]);
      expect(probe.log(), [2000], reason: 'the re-entrant note() did land');

      probe.dispose();
    });

    test('a re-entrant push cannot reallocate under a live iterator', () async {
      final probe = ReentrantProbe.new_();
      // Seed a buffer worth walking. `feedFromLog` shrink_to_fits it, so the
      // re-entrant push is a guaranteed reallocation — which, delivered on the
      // Rust stack, freed the buffer the iterator was walking.
      for (var i = 0; i < 8; i++) {
        probe.note(v: i);
      }
      final sink = ReentrantSink(probe);

      expect(
        probe.feedFromLog(out: sink),
        [0, 1, 2, 3, 4, 5, 6, 7],
        reason:
            'garbage here is a use-after-free: the re-entrant note() '
            'reallocated self.log while feedFromLog was iterating it',
      );

      await pumpEventLoop();
      expect(sink.refusal, isNull);
      expect(probe.log(), [
        0,
        1,
        2,
        3,
        4,
        5,
        6,
        7,
        1000,
      ], reason: 'and the deferred re-entrant note() still landed');

      probe.dispose();
    });
  });
}

/// Drain microtasks (and, on native, the port turn delivery rides) so the
/// deferred re-entrant call has landed before the assertions read it.
Future<void> pumpEventLoop() async {
  for (var i = 0; i < 5; i++) {
    await Future<void>.delayed(Duration.zero);
  }
}
