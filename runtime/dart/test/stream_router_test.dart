/// On web every event — a returning closure's invocation, a sink item, a
/// terminal, a leak report — arrives INSIDE the Rust producer's own
/// synchronous `post` import, so [StreamRouter] must run all of them on a
/// microtask: delivering during that call re-enters the bridge against live
/// borrows, which is a use-after-free
/// (tests/dart_integration/test/sink_reentrancy_test.dart). Native keeps it
/// synchronous. This tests both transport shapes directly.
@TestOn('vm')
library;

import 'dart:async';
import 'dart:typed_data';

import 'package:frustrate/src/binary_codec.dart';
import 'package:frustrate/src/envelope.dart';
import 'package:frustrate/src/stream_router.dart';
import 'package:test/test.dart';

/// One synthetic `STATUS_CALLBACK_CALL` envelope for invocation [invocationId]
/// carrying an i64 argument — the shape Rust's `call`/`call_async` post.
Uint8List invocationEnvelope(int invocationId, int arg) {
  final w = BinaryWriter()
    ..writeU8(statusCallbackCall)
    ..writeU8(0) // method selector: a returning mirror's single method
    ..writeHandle(invocationId)
    ..writeI64(arg);
  return w.takeBytes();
}

void main() {
  test(
    'web (deferDelivery): the closure runs on a microtask, not synchronously',
    () {
      var invoked = 0;
      int? respondedInvocation;
      Uint8List? respondedBytes;
      final router = StreamRouter(
        _ids(),
        deferDelivery: true,
        respond: (id, resp) {
          respondedInvocation = id;
          respondedBytes = resp;
        },
      );
      final cbId = router.openFunction((r) {
        invoked++;
        final w = BinaryWriter()..writeI64(r.readI64() * 2);
        return w;
      });

      // Deliver the invocation — as the web `_onPost` would, mid-poll.
      final consumed = router.deliver(cbId, invocationEnvelope(77, 21));
      expect(consumed, isTrue);
      // The crux: nothing ran synchronously during delivery.
      expect(invoked, 0, reason: 'the closure must not run during the poll');
      expect(respondedInvocation, isNull, reason: 'no response mid-poll');
      expect(respondedBytes, isNull);
    },
  );

  test(
    'web (deferDelivery): after the microtask, the closure ran and responded',
    () async {
      var invoked = 0;
      int? respondedInvocation;
      Uint8List? respondedBytes;
      final router = StreamRouter(
        _ids(),
        deferDelivery: true,
        respond: (id, resp) {
          respondedInvocation = id;
          respondedBytes = resp;
        },
      );
      final cbId = router.openFunction((r) {
        invoked++;
        final w = BinaryWriter()..writeI64(r.readI64() * 2);
        return w;
      });

      router.deliver(cbId, invocationEnvelope(77, 21));
      expect(invoked, 0);
      // Let the scheduled microtask run.
      await Future<void>.microtask(() {});
      expect(invoked, 1, reason: 'the closure ran on the microtask');
      expect(respondedInvocation, 77);
      // Response is [STATUS_OK, i64(42)].
      final r = BinaryReader(respondedBytes!);
      expect(r.readU8(), 0);
      expect(r.readI64(), 42);
    },
  );

  test(
    'native (synchronous): the closure runs and responds during delivery',
    () {
      var invoked = 0;
      int? respondedInvocation;
      Uint8List? respondedBytes;
      // Default deferDelivery:false — the native transport answers a parked
      // worker.
      final router = StreamRouter(
        _ids(),
        respond: (id, resp) {
          respondedInvocation = id;
          respondedBytes = resp;
        },
      );
      final cbId = router.openFunction((r) {
        invoked++;
        final w = BinaryWriter()..writeI64(r.readI64() + 1);
        return w;
      });

      router.deliver(cbId, invocationEnvelope(88, 9));
      // Synchronous: everything happened during deliver.
      expect(invoked, 1);
      expect(respondedInvocation, 88);
      final r = BinaryReader(respondedBytes!);
      expect(r.readU8(), 0);
      expect(r.readI64(), 10);
    },
  );

  group('a declared failure is a value; anything else stays loud', () {
    // The router cannot know the declared error type — only the generated
    // binding does — so the binding supplies `onDeclaredError`, which returns
    // the encoded payload for a throw it recognises and null for one it does
    // not. Null is what keeps an undeclared throw loud: still STATUS_ERROR,
    // which the Rust side turns into an attributable panic.
    int? respondedInvocation;
    Uint8List? respondedBytes;
    StreamRouter recordingRouter() {
      respondedInvocation = null;
      respondedBytes = null;
      return StreamRouter(
        _ids(),
        respond: (id, resp) {
          respondedInvocation = id;
          respondedBytes = resp;
        },
      );
    }

    test('a declared throw becomes STATUS_TYPED_ERROR carrying the value', () {
      final router = recordingRouter();
      final cbId = router.openFunction(
        (r) => throw _Refused(r.readI64()),
        onDeclaredError: (e) =>
            e is _Refused ? (BinaryWriter()..writeI64(e.code)) : null,
      );

      expect(router.deliver(cbId, invocationEnvelope(31, 7)), isTrue);
      expect(respondedInvocation, 31);
      final r = BinaryReader(respondedBytes!);
      expect(
        r.readU8(),
        statusTypedError,
        reason: 'the declared failure crosses as a value, not as prose',
      );
      expect(r.readI64(), 7);
    });

    test('an UNDECLARED throw stays STATUS_ERROR (attributable panic)', () {
      final router = recordingRouter();
      final cbId = router.openFunction(
        (r) => throw StateError('a bug, not a refusal'),
        onDeclaredError: (e) =>
            e is _Refused ? (BinaryWriter()..writeI64(e.code)) : null,
      );

      expect(router.deliver(cbId, invocationEnvelope(32, 7)), isTrue);
      final r = BinaryReader(respondedBytes!);
      expect(r.readU8(), 1, reason: 'STATUS_ERROR: the Rust side panics');
      expect(r.readString(), contains('a bug, not a refusal'));
    });

    test('a throwing onDeclaredError still answers, naming both failures', () {
      final router = recordingRouter();
      final cbId = router.openFunction(
        (r) => throw _Refused(r.readI64()),
        onDeclaredError: (e) => throw StateError('encoder exploded'),
      );

      expect(router.deliver(cbId, invocationEnvelope(33, 7)), isTrue);
      final r = BinaryReader(respondedBytes!);
      expect(r.readU8(), 1, reason: 'a failure to encode the failure is loud');
      final msg = r.readString();
      expect(msg, contains('encoder exploded'));
      expect(msg, contains('Refused'), reason: 'the original throw too');
    });
  });

  group('an accepted invocation is always answered', () {
    // The one death mode the runtime's liveness probe is structurally blind
    // to. A probe detects a *dead* isolate; an isolate that took a callback
    // invocation and then threw instead of responding is very much alive, so
    // the parked Rust caller — a blocked pool worker, or a suspended task —
    // would wait forever. Worse, on Flutter a root-zone throw is swallowed by
    // `PlatformDispatcher.onError`, so the isolate survives and nothing ever
    // fires. Hence: every exit from `_invoke` that has an invocation id
    // answers with it.

    test(
      'an invocation for an unregistered id answers with the error envelope',
      () {
        int? respondedInvocation;
        Uint8List? respondedBytes;
        final router = StreamRouter(
          _ids(),
          respond: (id, resp) {
            respondedInvocation = id;
            respondedBytes = resp;
          },
        );
        // The reachable shape: `fail` retires a function registration while the
        // Rust handle is still live, so a later invocation finds nothing.
        final cbId = router.openFunction((r) {
          final w = BinaryWriter()..writeI64(r.readI64());
          return w;
        });
        router.fail(
          cbId,
          StateError('opening call failed'),
          StackTrace.current,
        );

        expect(
          router.deliver(cbId, invocationEnvelope(99, 5)),
          isTrue,
          reason: 'consumed, not left to the call-completion path',
        );
        expect(
          respondedInvocation,
          99,
          reason:
              'the waiter is identified by the id inside the payload, '
              'which must be parsed even when the lookup misses',
        );
        final r = BinaryReader(respondedBytes!);
        expect(
          r.readU8(),
          1,
          reason: 'STATUS_ERROR, so Rust panics attributably',
        );
        expect(r.readString(), contains('unknown callback'));
      },
    );

    test('the miss is deferred on web, exactly like the closure would be', () async {
      // Responding synchronously here would re-enter the bridge inside the
      // Rust poll's `post` import — the hazard `deferDelivery` exists for. The
      // error answer must take the same microtask the closure takes, and it
      // does so through `deliver` rather than any deferral of its own.
      int? respondedInvocation;
      final router = StreamRouter(
        _ids(),
        deferDelivery: true,
        respond: (id, resp) => respondedInvocation = id,
      );
      final cbId = router.openFunction((r) => BinaryWriter());
      router.fail(cbId, StateError('opening call failed'), StackTrace.current);

      expect(router.deliver(cbId, invocationEnvelope(101, 5)), isTrue);
      expect(respondedInvocation, isNull, reason: 'not during the poll');
      await Future<void>.microtask(() {});
      expect(respondedInvocation, 101, reason: 'answered on the microtask');
    });
  });

  group('one router, many channels', () {
    // One router per isolate (native) or page (web, shared by every actor
    // host), routing by id alone — these are the properties that arrangement
    // rests on.

    test(
      'ids from one sequence route to their own stream, with no cross-talk',
      () {
        final router = StreamRouter(_ids());
        final a = <int>[], b = <int>[];
        var aDone = false, bDone = false;
        // Two streams opened against what are, on web, two different channels
        // (the main instance and an actor worker) — one router, one sequence.
        final idA = router.open(
          (r) => a.add(r.readI64()),
          _unusedError,
          () => aDone = true,
        );
        final idB = router.open(
          (r) => b.add(r.readI64()),
          _unusedError,
          () => bDone = true,
        );
        expect(idA, isNot(idB), reason: 'one sequence never repeats an id');

        router.deliver(idA, itemEnvelope(1));
        router.deliver(idB, itemEnvelope(2));
        router.deliver(idA, itemEnvelope(3));
        expect(a, [1, 3]);
        expect(b, [2], reason: 'B saw only its own events');

        // A terminal on one retires only that registration.
        router.deliver(idA, Uint8List.fromList([statusStreamEnd]));
        expect(aDone, isTrue);
        expect(bDone, isFalse);
        router.deliver(idB, itemEnvelope(4));
        expect(b, [2, 4], reason: 'B is still live after A ended');
      },
    );

    test('an unregistered id is left for the channel\'s completer path', () {
      // The collision guard, stated as a test: call ids must come from the
      // same sequence as stream ids. If a channel minted its own call ids,
      // one could equal a live stream id here and `deliver` would swallow the
      // completion as a stream event — silent misdelivery.
      final router = StreamRouter(_ids());
      final streamId = router.open((r) {}, _unusedError, () {});

      // A call completion (STATUS_OK) for an id the router does not know.
      expect(
        router.deliver(streamId + 1, Uint8List.fromList([0, 0, 0, 0])),
        isFalse,
        reason: 'not ours — the transport completes its future',
      );
      // The same status on a *registered* id would be consumed, which is
      // exactly why the two id spaces must never overlap.
      expect(router.deliver(streamId, itemEnvelope(7)), isTrue);
    });
  });

  group('object dispatch table', () {
    test('a selector picks the method; the same id carries all of them', () {
      final router = StreamRouter(_ids());
      final added = <int>[], errors = <String>[];
      // A two-method mirror, e.g. dart:async EventSink: add / addError.
      final id = router.openObject(
        [(r) => added.add(r.readI64()), (r) => errors.add(r.readString())],
        _unusedError,
        () {},
      );

      router.deliver(id, itemEnvelope(1));
      router.deliver(id, errorEnvelope('bad'));
      router.deliver(id, itemEnvelope(2));
      expect(added, [1, 2]);
      expect(errors, [
        'bad',
      ], reason: 'addError is non-terminal — the object keeps receiving');
    });

    test(
      'an out-of-range selector is a loud bridge bug, not a silent drop',
      () {
        final router = StreamRouter(_ids());
        final id = router.open((r) => r.readI64(), _unusedError, () {});
        // A single-method registration reached by selector 1.
        expect(
          () => router.deliver(id, itemEnvelope(1, selector: 1)),
          throwsA(isA<StateError>()),
        );
      },
    );

    test('a throwing method cancels the channel and reports to the INSTALL zone', () {
      // The zone that matters is the one the router was INSTALLED in, not the
      // one `deliver` happens to be called from — and this test is shaped that
      // way round because production is. Delivery runs from a bare
      // `RawReceivePort` callback, where `Zone.current` is the ROOT zone, so
      // reporting to `Zone.current` made a throwing user closure unhandleable
      // and killed the isolate: `runZonedGuarded` around the whole app never
      // saw it.
      //
      // The previous version of this test constructed the router outside a
      // guard and called `deliver` inside one, which passed against the buggy
      // code and described a situation that cannot occur — nothing in the
      // runtime delivers from application code. Found by a driver that was
      // killed by its own probe while measuring what each bridge does when a
      // callback closure throws.
      final cancelled = <int>[];
      final errors = <Object>[];

      late final StreamRouter router;
      late final int id;
      runZonedGuarded(() {
        router = StreamRouter(_ids(), cancel: cancelled.add);
        id = router.open(
          (r) => throw StateError('user method broke'),
          _unusedError,
          () {},
        );
      }, (e, st) => errors.add(e));

      // Delivered from OUTSIDE that zone, exactly as the port callback does.
      router.deliver(id, itemEnvelope(1));

      expect(
        cancelled,
        [id],
        reason:
            'the producer is told to stop rather than shout into a '
            'target that cannot accept',
      );
      expect(
        errors,
        hasLength(1),
        reason:
            'the install zone must see it — reporting to Zone.current '
            'here would be the root zone, i.e. a dead isolate',
      );
      expect(errors.single, isA<StateError>());
    });
  });

  group('a retired stream absorbs its producer\'s late terminal', () {
    // Cancel and fail are the two ways a stream leaves the table while the
    // Rust producer is still holding its sink, so both race the same way: the
    // sink's drop (or an explicit `error`) posts a terminal for an id the
    // router no longer knows. Neither may fall through to the call-completion
    // path, where an unknown id is a debug assert crash and a silent drop in
    // release — for a race that is entirely legal.

    test('cancelLocal absorbs a late terminal error', () {
      final router = StreamRouter(_ids());
      final id = router.open((r) {}, _unusedError, () {});
      router.cancelLocal(id);
      expect(
        router.deliver(id, terminalErrorEnvelope('late')),
        isTrue,
        reason: 'the tombstone absorbs it',
      );
    });

    test(
      'fail() absorbs a late terminal error, exactly as cancelLocal does',
      () {
        final router = StreamRouter(_ids());
        final errors = <Object>[];
        final id = router.open((r) {}, (e, st) => errors.add(e), () {});
        router.fail(
          id,
          StateError('the opening call failed'),
          StackTrace.current,
        );
        expect(
          errors,
          hasLength(1),
          reason: 'the failure reached the consumer',
        );

        // The producer's sink drops afterwards and posts its own terminal.
        expect(
          router.deliver(id, terminalErrorEnvelope('late')),
          isTrue,
          reason:
              'a stream removed by fail() is tombstoned like one removed '
              'by cancelLocal — the two exits leave the table symmetrically',
        );
        expect(errors, hasLength(1), reason: 'and never re-delivered');
      },
    );

    /// **A producer terminal is not "no more items".** `close()` on one sink
    /// clone takes the terminal and posts it while an `add` on another clone is
    /// already inside its post, so the end event can be enqueued first and the
    /// item arrive after it. That race is legal, and for data it costs the
    /// bytes — but a handle item has been *minted*, and retiring the
    /// registration would throw away the only thing that knew how to free it.
    ///
    /// So a channel that can carry handles leaves its reclaim on the tombstone
    /// when a terminal retires it, exactly as a cancel does. The cost is the
    /// one a cancel already pays and the code already states: one entry for the
    /// session, and only for a channel whose items carry handles.
    test('a terminal keeps the reclaim, so a late item is still freed', () {
      for (final terminal in [
        Uint8List.fromList([statusStreamEnd]),
        terminalErrorEnvelope('the producer failed'),
      ]) {
        var freed = 0;
        final router = StreamRouter(_ids());
        final id = router.open(
          (r) => r.readI64(),
          (e, st) {},
          () {},
          reclaim: (r) {
            r.readI64();
            freed++;
          },
        );

        expect(router.deliver(id, terminal), isTrue);
        // The item its producer posted before the terminal landed.
        expect(
          router.deliver(id, itemEnvelope(7)),
          isTrue,
          reason: 'a late item for a retired id is still the router\'s',
        );
        expect(freed, 1, reason: 'and the handle it carried was freed');
      }
    });

    test('a value channel leaves no tombstone behind', () {
      // The cost is paid only where it buys something: a channel with no
      // reclaim is retired outright, so a long-lived app opening value streams
      // accumulates nothing.
      final router = StreamRouter(_ids());
      final id = router.open((r) {}, (e, st) {}, () {});
      router.deliver(id, Uint8List.fromList([statusStreamEnd]));
      expect(router.openRegistrationCount, 0);
      // A late item for it is absorbed, as it always was, and frees nothing
      // because there was nothing to free.
      expect(router.deliver(id, itemEnvelope(7)), isTrue);
    });

    test('a late STREAM_END after fail() is already absorbed', () {
      // The contrast that shows the inconsistency: an end event for an
      // unknown id is absorbed by the catch-all, an *error* status is not.
      final router = StreamRouter(_ids());
      final id = router.open((r) {}, (e, st) {}, () {});
      router.fail(
        id,
        StateError('the opening call failed'),
        StackTrace.current,
      );
      expect(router.deliver(id, Uint8List.fromList([statusStreamEnd])), isTrue);
    });
  });

  test(
    'a throwing closure becomes the error envelope (deferred, web)',
    () async {
      int? status;
      final router = StreamRouter(
        _ids(),
        deferDelivery: true,
        respond: (id, resp) => status = resp[0],
      );
      final cbId = router.openFunction((r) {
        throw StateError('closure broke');
      });
      router.deliver(cbId, invocationEnvelope(1, 0));
      await Future<void>.microtask(() {});
      expect(status, 1, reason: 'STATUS_ERROR — Rust turns it into a panic');
    },
  );

  group('web (deferDelivery): the void-method path defers too', () {
    // The gap this group exists for: the flag used to cover only `_invoke`,
    // so a sink item — the ONE mirror whose Dart side is arbitrary user code
    // by design — still ran on the Rust stack.

    test('a sink item does not run during the producing call', () async {
      final got = <int>[];
      final router = StreamRouter(_ids(), deferDelivery: true);
      final id = router.open((r) => got.add(r.readI64()), _unusedError, () {});

      expect(
        router.deliver(id, itemEnvelope(1)),
        isTrue,
        reason: 'classified synchronously — the transport needs the answer',
      );
      expect(
        got,
        isEmpty,
        reason:
            'the crux: user `Sink.add` must not run mid-post, where a '
            'call back into the bridge would alias a live &mut',
      );

      await Future<void>.microtask(() {});
      expect(got, [1], reason: 'it ran on the microtask instead');
    });

    test(
      'items and the terminal keep FIFO order across the deferral',
      () async {
        final events = <String>[];
        final router = StreamRouter(_ids(), deferDelivery: true);
        final id = router.open(
          (r) => events.add('item ${r.readI64()}'),
          _unusedError,
          () => events.add('done'),
        );

        // The emit_dart.rs objection to deferring, stated as a test: deferring
        // items but not terminals would reorder a close ahead of its own items.
        // One FIFO microtask queue for both is what answers it.
        router.deliver(id, itemEnvelope(1));
        router.deliver(id, itemEnvelope(2));
        router.deliver(id, Uint8List.fromList([statusStreamEnd]));
        expect(events, isEmpty);

        await Future<void>.microtask(() {});
        expect(events, ['item 1', 'item 2', 'done']);
      },
    );

    test('a cancel inside the deferral window absorbs the item, as native does', () async {
      // The window deferral opens: the producing call returns, its caller
      // runs, and THAT code may cancel before the microtask drains. Native has
      // always had this window (the port queue drains later), and absorbs the
      // item via the tombstone. Web must not deliver what native drops — which
      // is why nothing is captured across the gap but the id and the bytes.
      final got = <int>[];
      final router = StreamRouter(_ids(), deferDelivery: true);
      final id = router.open((r) => got.add(r.readI64()), _unusedError, () {});

      expect(router.deliver(id, itemEnvelope(1)), isTrue);
      router.cancelLocal(id);

      await Future<void>.microtask(() {});
      expect(
        got,
        isEmpty,
        reason:
            'the tombstone absorbed it — a captured registration would '
            'have pushed into a cancelled consumer',
      );
      expect(router.openRegistrationCount, 0);
    });

    test(
      'a leak terminal on a callback id is reported off the Rust stack',
      () async {
        // The seam a `_dispatch`-only deferral would still have missed: the
        // leak report runs a zone handler, which is user code, and it posts
        // inline from the Rust finalizer export.
        final errors = <Object>[];
        final router = StreamRouter(
          _ids(),
          deferDelivery: true,
          respond: (id, resp) {},
          zone: Zone.current.fork(
            specification: ZoneSpecification(
              handleUncaughtError: (self, parent, zone, e, st) => errors.add(e),
            ),
          ),
        );
        final cbId = router.openFunction((r) => BinaryWriter());

        expect(router.deliver(cbId, leakEnvelope('TextDoc.onChange')), isTrue);
        expect(errors, isEmpty, reason: 'not on the Rust stack');

        await Future<void>.microtask(() {});
        expect(errors, hasLength(1));
        expect(router.openRegistrationCount, 0, reason: 'and it retired');
      },
    );
  });
}

/// One `STATUS_LEAKED` terminal — the abandonment report a Rust holder's
/// finalizer posts, carrying the holder's type name.
Uint8List leakEnvelope(String holderType) {
  final w = BinaryWriter()
    ..writeU8(statusLeaked)
    ..writeString(holderType);
  return w.takeBytes();
}

/// One `STATUS_STREAM_ITEM` envelope carrying an i64 item on [selector]
/// (0 = the primary method, i.e. `Sink.add`).
Uint8List itemEnvelope(int value, {int selector = 0}) {
  final w = BinaryWriter()
    ..writeU8(statusStreamItem)
    ..writeU8(selector)
    ..writeI64(value);
  return w.takeBytes();
}

/// One `STATUS_STREAM_ITEM` envelope on selector 1 (`addError`) carrying a
/// message — non-terminal, unlike the error *status*.
Uint8List errorEnvelope(String message) {
  final w = BinaryWriter()
    ..writeU8(statusStreamItem)
    ..writeU8(1)
    ..writeString(message);
  return w.takeBytes();
}

/// One *terminal* error envelope — `[STATUS_ERROR, message]`, what
/// `StreamSink::error` posts and what a producer's late terminal looks like.
/// (`_statusError` is private to envelope.dart, so the byte is spelled out;
/// distinct from [errorEnvelope], which is the non-terminal `addError`.)
Uint8List terminalErrorEnvelope(String message) {
  final w = BinaryWriter()
    ..writeU8(1)
    ..writeString(message);
  return w.takeBytes();
}

/// For streams whose error path is not under test — a terminal error here
/// would be a bridge bug, so fail loudly rather than swallow it.
void _unusedError(Object e, StackTrace st) =>
    fail('unexpected stream error: $e');

/// A trivial monotonic id source for the router.
int Function() _ids() {
  var next = 1000;
  return () => next++;
}

/// The "declared error" a fallible closure throws in these tests, standing in
/// for a generated `EException`.
final class _Refused implements Exception {
  final int code;
  _Refused(this.code);
  @override
  String toString() => 'Refused($code)';
}
