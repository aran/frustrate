/// The async issue-path contract both transports share through
/// [PendingCalls]:
///
/// - a `Future`-returning bridge API never throws synchronously — every
///   failure, including one raised before Rust ever saw the call, arrives
///   through the returned future;
/// - a failed issue leaves no trace: nothing pending, and the `onChanged`
///   hook fires so the native transport can drop its keep-alive pin (a leaked
///   pin there is a process that never exits).
///
/// Tested here rather than end-to-end because the seam is pure Dart: no
/// dylib, no wasm, no browser — the same reason `stream_router_test` exists.
@TestOn('vm')
library;

import 'dart:async';
import 'dart:typed_data';

import 'package:frustrate/src/binary_codec.dart';
import 'package:frustrate/src/exceptions.dart';
import 'package:frustrate/src/pending_calls.dart';
import 'package:test/test.dart';

/// An ok envelope (status 0) carrying one i64 — the shape a completion posts.
Uint8List okEnvelope(int value) =>
    (BinaryWriter()
          ..writeU8(0)
          ..writeI64(value))
        .takeBytes();

/// An error envelope (status 1) carrying its message.
Uint8List errorEnvelope(String message) =>
    (BinaryWriter()
          ..writeU8(1)
          ..writeString(message))
        .takeBytes();

/// A typed-error envelope (status 8) carrying an i32 discriminant — the shape
/// a `Result<T, E>` with bridged `E` posts. The payload's meaning is the
/// generated binding's business; this seam only has to route it there.
Uint8List typedErrorEnvelope(int tag) =>
    (BinaryWriter()
          ..writeU8(8)
          ..writeI32(tag))
        .takeBytes();

/// Stands in for a generated exception class.
class FakeTypedException implements Exception {
  final int tag;
  FakeTypedException(this.tag);
}

/// Ids from a plain counter; the real allocators are isolate-tagged (native)
/// or the page-wide sequence (web), neither of which this seam cares about.
int Function() counter([int start = 1]) {
  var next = start;
  return () => next++;
}

void main() {
  test(
    'a failed issue rejects the future and never throws synchronously',
    () async {
      var changes = 0;
      final calls = PendingCalls(counter(), onChanged: () => changes++);

      // Deliberately NOT wrapped in try/catch: a synchronous throw here fails
      // the test as an error rather than as an expectation, which is the point.
      late final Future<BinaryReader> f;
      expect(
        () => f = calls.issue((_) => throw StateError('no such export')),
        returnsNormally,
      );

      await expectLater(
        f,
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('no such export'),
          ),
        ),
      );
      expect(
        calls.isEmpty,
        isTrue,
        reason: 'a failed issue must leave nothing pending',
      );
      expect(
        changes,
        2,
        reason:
            'onChanged fires on register and again on rollback, so the '
            'native keep-alive is recomputed off the pin',
      );
    },
  );

  test(
    'a successful issue stays pending until its completion arrives',
    () async {
      var changes = 0;
      final calls = PendingCalls(counter(), onChanged: () => changes++);

      int? issued;
      final f = calls.issue((callId) => issued = callId);
      expect(issued, 1, reason: 'the id comes from the channel sequence');
      expect(calls.isEmpty, isFalse);
      expect(changes, 1);

      expect(calls.complete(issued!, okEnvelope(42)), isTrue);
      expect((await f).readI64(), 42);
      expect(calls.isEmpty, isTrue);
      expect(changes, 2);
    },
  );

  test('a non-ok envelope completes with the envelope exception', () async {
    final calls = PendingCalls(counter());
    late final int id;
    final f = calls.issue((callId) => id = callId);
    calls.complete(id, errorEnvelope('rust said no'));
    await expectLater(
      f,
      throwsA(
        isA<BridgeException>().having(
          (e) => e.message,
          'message',
          'rust said no',
        ),
      ),
    );
  });

  test(
    'a typed error is decoded by the decoder the call was issued with',
    () async {
      final calls = PendingCalls(counter());
      late final int id;
      final f = calls.issue(
        (callId) => id = callId,
        typedError: (r) => FakeTypedException(r.readI32()),
      );
      calls.complete(id, typedErrorEnvelope(3));
      await expectLater(
        f,
        throwsA(isA<FakeTypedException>().having((e) => e.tag, 'tag', 3)),
      );
    },
  );

  test('a typed error with no decoder is loud, not a mis-decode', () async {
    // Unreachable in a consistent pair — the error type is in the IR the
    // schema fingerprint covers — but the alternative on the impossible path
    // is reading the payload as whatever the next case expects.
    final calls = PendingCalls(counter());
    late final int id;
    final f = calls.issue((callId) => id = callId);
    calls.complete(id, typedErrorEnvelope(3));
    await expectLater(
      f,
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('rebuild both from the same api.rs'),
        ),
      ),
    );
  });

  test(
    'a decoder that throws rejects the future instead of orphaning it',
    () async {
      // The decoder runs while *building* the error to complete with, and by
      // then the completer has already left the maps. A throw there used to
      // escape into a bare port callback — root zone, so the isolate aborts —
      // and leave the caller's future unsettled forever. A corrupt payload must
      // fail the call, like every other corrupt envelope.
      final calls = PendingCalls(counter());
      late final int id;
      final f = calls.issue(
        (callId) => id = callId,
        typedError: (r) => throw StateError('invalid variant index 9'),
      );
      calls.complete(id, typedErrorEnvelope(9));
      await expectLater(
        f,
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            allOf(contains('typed error'), contains('invalid variant index 9')),
          ),
        ),
      );
      expect(calls.isEmpty, isTrue);
      expect(calls.retainedDecoders, 0);
    },
  );

  test('a decoder does not outlive the call that owned it', () async {
    // The two maps are keyed the same, so every drain of one must drain the
    // other; otherwise a long-lived transport accumulates decoders for calls
    // that ended long ago.
    final calls = PendingCalls(counter());
    late final int id;
    final f = calls.issue(
      (callId) => id = callId,
      typedError: (r) => FakeTypedException(r.readI32()),
    );
    calls.complete(id, okEnvelope(1));
    await f;
    expect(calls.isEmpty, isTrue);
    expect(
      calls.retainedDecoders,
      0,
      reason:
          'a completed call left its decoder behind; ids are never '
          'reused, so nothing would ever read or evict it',
    );

    // Every other drain path, for the same reason.
    late final int id2;
    final f2 = calls.issue(
      (callId) => id2 = callId,
      typedError: (r) => FakeTypedException(r.readI32()),
    );
    calls.fail(id2, StateError('boom'), StackTrace.current);
    await expectLater(f2, throwsA(isA<StateError>()));
    expect(calls.retainedDecoders, 0, reason: 'fail() left a decoder behind');

    final f3 = calls.issue(
      (_) {},
      typedError: (r) => FakeTypedException(r.readI32()),
    );
    calls.failAll(StateError('worker died'), StackTrace.current);
    await expectLater(f3, throwsA(isA<StateError>()));
    expect(calls.retainedDecoders, 0, reason: 'failAll() left decoders behind');
  });

  test(
    'fail attributes an error, and reports when there is nothing to attribute',
    () async {
      final calls = PendingCalls(counter());
      late final int id;
      final f = calls.issue((callId) => id = callId);

      expect(
        calls.fail(id, BridgePanicException('trapped'), StackTrace.current),
        isTrue,
      );
      await expectLater(f, throwsA(isA<BridgePanicException>()));

      // The web executor's mid-poll trap handling depends on this distinction:
      // an unattributable trap must stay loud instead of vanishing into a
      // settled future.
      expect(calls.fail(id, StateError('again'), StackTrace.current), isFalse);
      expect(calls.complete(999, okEnvelope(1)), isFalse);
    },
  );

  test('failAll drains every call in flight', () async {
    final calls = PendingCalls(counter());
    final futures = [for (var i = 0; i < 3; i++) calls.issue((_) {})];
    calls.failAll(StateError('worker terminated'), StackTrace.current);
    expect(calls.isEmpty, isTrue);
    for (final f in futures) {
      await expectLater(f, throwsA(isA<StateError>()));
    }
  });

  test(
    'a throw after the completion already landed is reported out of band',
    () async {
      // Single-threaded web runs the whole async body inline during
      // `frustrate_call_async`, so `post` can settle the future *before* a trap
      // unwinds out of the same export. Completing twice would be a "Future
      // already completed" state error; swallowing the trap would be silent.
      final calls = PendingCalls(counter());
      final reported = <Object>[];
      Future<BinaryReader>? f;
      runZonedGuarded(() {
        f = calls.issue((callId) {
          calls.complete(callId, okEnvelope(7));
          throw StateError('trapped on the way out');
        });
      }, (e, st) => reported.add(e));

      expect(
        (await f!).readI64(),
        7,
        reason: 'the completion that landed inline stands',
      );
      expect(reported, hasLength(1));
      expect(reported.single, isA<StateError>());
      expect(calls.isEmpty, isTrue);
    },
  );
}
