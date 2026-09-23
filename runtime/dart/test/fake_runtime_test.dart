/// The byte-level fake transport, driven by hand-encoded requests.
///
/// Everything here is written the way a *generated* harness would write it —
/// requests decoded field by field, answers framed as response envelopes — but
/// by hand, so the transport's own contracts are what is under test rather
/// than an emitter's idea of them: the never-throw-synchronously rule, cancel
/// claiming, actor FIFO ordering and deferred release, stream backpressure and
/// cancel, the callback round trip, and the handle registry.
///
/// The typed harness that normally sits in the `FakeBridge` slot is generated
/// and lives with the bindings; `tests/dart_integration/test/fake_harness_test.dart`
/// is where that is exercised against a real interface.
library;

import 'dart:async';
import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/testing.dart';
import 'package:test/test.dart';

final BigInt _hash = BigInt.parse('0123456789abcdef', radix: 16);

/// A [FakeBridge] whose answers are installed per test as closures, so each
/// test spells out only the member it cares about. A generated harness has a
/// `switch` here instead.
final class _ProbeBridge extends FakeBridge {
  _ProbeBridge() : super(_hash);

  final Map<int, Uint8List Function(BinaryReader r)> sync = {};
  final Map<int, Future<Uint8List> Function(BinaryReader r)> async = {};

  /// Every fn id this bridge was asked to answer, in order — the check that a
  /// claimed call really never reached the harness.
  final List<int> seen = [];

  @override
  Uint8List answerSync(int fnId, BinaryReader request) {
    seen.add(fnId);
    final answer = sync[fnId];
    if (answer == null) return fakeThrown(UnimplementedError(), 'fn#$fnId');
    return answer(request);
  }

  @override
  Future<Uint8List> answerAsync(int fnId, BinaryReader request) async {
    seen.add(fnId);
    final answer = async[fnId];
    if (answer == null) return fakeThrown(UnimplementedError(), 'fn#$fnId');
    return answer(request);
  }
}

/// `[statusOk][payload…]`, the way a generated arm builds it: the status byte
/// goes in first and the return value is written after it, so the envelope is
/// one buffer and nothing is prepended.
Uint8List _ok(void Function(BinaryWriter w) encode) {
  final w = BinaryWriter(16)..writeU8(statusOk);
  encode(w);
  return w.takeBytes();
}

void main() {
  late _ProbeBridge bridge;
  late FakeRuntime rt;

  setUp(() {
    bridge = _ProbeBridge();
    rt = FakeRuntime(bridge);
  });

  group('calls', () {
    test('a sync call round-trips the request and the answer', () {
      bridge.sync[7] = (r) {
        final a = r.readI32();
        final b = r.readString();
        r.assertConsumed();
        return _ok((w) => w.writeString('$b:${a * 2}'));
      };
      final r = rt.callSync(7, 16, (w) {
        w.writeI32(21);
        w.writeString('x');
      });
      expect(r.readString(), 'x:42');
      r.assertConsumed();
    });

    test('every failure status arrives as the exception it names', () {
      bridge.sync[1] = (_) => fakeThrown(BridgeException('nope'), 'm');
      bridge.sync[2] = (_) => fakeThrown(BridgePanicException('boom'), 'm');
      bridge.sync[3] = (_) => fakeThrown(ContentionException('busy'), 'm');
      // Anything the member did not declare crosses as a panic that names it.
      bridge.sync[4] = (_) => fakeThrown(UnimplementedError(), 'Thing.method');

      expect(
        () => rt.callSync(1, 0, (_) {}),
        throwsA(isA<BridgeException>().having((e) => e.message, 'm', 'nope')),
      );
      expect(
        () => rt.callSync(2, 0, (_) {}),
        throwsA(isA<BridgePanicException>()),
      );
      expect(
        () => rt.callSync(3, 0, (_) {}),
        throwsA(isA<ContentionException>()),
      );
      expect(
        () => rt.callSync(4, 0, (_) {}),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'm',
            contains('Thing.method'),
          ),
        ),
      );
    });

    test('a typed error reaches the call-site decoder', () {
      bridge.sync[9] = (_) {
        final w = BinaryWriter(8)
          ..writeU8(statusTypedError)
          ..writeI32(404);
        return w.takeBytes();
      };
      expect(
        () => rt.callSync(
          9,
          0,
          (_) {},
          typedError: (r) => ArgumentError('code ${r.readI32()}'),
        ),
        throwsA(isA<ArgumentError>().having((e) => e.message, 'm', 'code 404')),
      );
    });

    test('an async call rejects its future rather than throwing at the caller', () async {
      // The encoder runs inside the transport, which is what makes a throwing
      // one — a stale handle, an out-of-range u64 — reject the returned future
      // instead of escaping `unawaited(...)` and `Future.wait([...])`.
      late Future<BinaryReader> f;
      expect(() {
        f = rt.callAsync(5, 8, (w) => throw StateError('bad param'));
      }, returnsNormally);
      await expectLater(f, throwsA(isA<StateError>()));
      expect(bridge.seen, isEmpty, reason: 'nothing was dispatched');
    });

    test('a response is never delivered synchronously with the call', () async {
      bridge.async[6] = (_) async => _ok((w) => w.writeI32(1));
      var settled = false;
      final f = rt.callAsync(6, 0, (_) {}).whenComplete(() => settled = true);
      expect(settled, isFalse);
      await f;
      expect(settled, isTrue);
    });

    test('cancelling a token claims the call in flight', () async {
      final released = Completer<void>();
      bridge.async[8] = (_) async {
        await released.future;
        return _ok((w) => w.writeI32(1));
      };
      final token = FrustrateCancelToken();
      final f = rt.callAsync(8, 0, (_) {}, cancel: token);
      expect(rt.inFlightCallCount, 1);
      token.cancel();
      await expectLater(f, throwsA(isA<CancelledCallException>()));
      expect(rt.inFlightCallCount, 0);
      // The body still finishes; its answer lands on a call nobody holds and
      // is dropped rather than double-settling anything.
      released.complete();
      await pumpEventQueue();
    });

    test(
      'an already-cancelled token refuses before anything is encoded',
      () async {
        final token = FrustrateCancelToken()..cancel();
        var encoded = false;
        await expectLater(
          rt.callAsync(8, 0, (_) => encoded = true, cancel: token),
          throwsA(isA<CancelledCallException>()),
        );
        expect(encoded, isFalse);
      },
    );
  });

  group('identity and schema', () {
    test('a fake is its own bridge', () {
      expect(rt.bridgeIdentity, same(rt));
    });

    test('it accepts exactly the hash its harness was generated with', () {
      expect(() => rt.checkSchemaHash(_hash), returnsNormally);
      expect(
        () => rt.checkSchemaHash(_hash + BigInt.one),
        throwsA(isA<StateError>()),
      );
    });

    test('one bridge answers for one runtime', () {
      expect(() => FakeRuntime(bridge), throwsA(isA<StateError>()));
    });

    test(
      'a bridge with no runtime behind it says so rather than half-working',
      () {
        expect(() => _ProbeBridge().wire, throwsA(isA<StateError>()));
      },
    );

    test('async is not parallel and hardware parallelism is one', () {
      // Fixed answers, not configurable: nothing here runs on a second thread,
      // so a knob could only let a test assert something the fake cannot do.
      expect(rt.asyncIsParallel, isFalse);
      expect(rt.hardwareParallelism, 1);
    });

    test('a drop hook is memoized per symbol', () {
      expect(
        rt.handleDrop('frustrate_drop_A'),
        same(rt.handleDrop('frustrate_drop_A')),
      );
      expect(
        rt.handleDrop('frustrate_drop_A'),
        isNot(same(rt.handleDrop('frustrate_drop_B'))),
      );
    });
  });

  group('handles', () {
    test('a minted handle resolves, and a drop retires it', () {
      final thing = Object();
      final raw = rt.mintHandle(thing);
      expect(rt.resolveHandle(raw), same(thing));
      rt.retireHandle(raw);
      expect(() => rt.resolveHandle(raw), throwsA(isA<StateError>()));
    });

    test('a raw the fake never minted is a loud failure, not a null', () {
      expect(() => rt.resolveHandle(999), throwsA(isA<StateError>()));
    });

    test('dispose()ing a handle retires the entry through its drop hook', () {
      final raw = rt.mintHandle(Object());
      final drop = rt.handleDrop('frustrate_drop_Thing');
      final owner = Object();
      drop.attach(owner, raw);
      drop.detach(owner);
      drop.drop(raw);
      expect(() => rt.resolveHandle(raw), throwsA(isA<StateError>()));
    });
  });

  group('actors', () {
    test('plain calls run one at a time, in arrival order', () async {
      final order = <String>[];
      final gate = Completer<void>();
      bridge.async[10] = (_) async {
        order.add('enter 10');
        await gate.future;
        order.add('leave 10');
        return _ok((_) {});
      };
      bridge.async[11] = (_) async {
        order.add('enter 11');
        return _ok((_) {});
      };
      final host = await rt.spawnActorHost(debugName: 'Miner');
      final a = host.call(10, 0, (_) {});
      final b = host.call(11, 0, (_) {});
      await pumpEventQueue();
      expect(order, ['enter 10'], reason: '11 waits its turn');
      gate.complete();
      await Future.wait([a, b]);
      expect(order, ['enter 10', 'leave 10', 'enter 11']);
    });

    test(
      'a deferred call releases the executor as soon as its body suspends',
      () async {
        final order = <String>[];
        final never = Completer<void>();
        bridge.async[12] = (_) async {
          order.add('enter 12');
          await never.future;
          return _ok((_) {});
        };
        bridge.async[13] = (_) async {
          order.add('enter 13');
          return _ok((_) {});
        };
        final host = await rt.spawnActorHost(debugName: 'Miner');
        final slow = host.call(12, 0, (_) {}, deferred: true);
        final quick = host.call(13, 0, (_) {});
        await quick;
        expect(order, ['enter 12', 'enter 13']);
        // The deferred completion is still outstanding; dispose cancels it.
        await host.shutdown();
        await expectLater(
          slow,
          throwsA(
            isA<StateError>().having((e) => e.message, 'm', contains('Miner')),
          ),
        );
      },
    );

    test(
      'a deferred call claimed while queued never reaches the harness',
      () async {
        final gate = Completer<void>();
        bridge.async[14] = (_) async {
          await gate.future;
          return _ok((_) {});
        };
        bridge.async[15] = (_) async => _ok((_) {});
        final host = await rt.spawnActorHost(debugName: 'Miner');
        final blocker = host.call(14, 0, (_) {});
        final token = FrustrateCancelToken();
        final queued = host.call(15, 0, (_) {}, deferred: true, cancel: token);
        await pumpEventQueue();
        token.cancel();
        await expectLater(queued, throwsA(isA<CancelledCallException>()));
        gate.complete();
        await blocker;
        await pumpEventQueue();
        expect(bridge.seen, [
          14,
        ], reason: 'the claimed job was released, not dispatched');
      },
    );

    test('a call after shutdown is refused through the future', () async {
      final host = await rt.spawnActorHost(debugName: 'Miner');
      await host.shutdown();
      late Future<BinaryReader> f;
      expect(() => f = host.call(16, 0, (_) {}), returnsNormally);
      await expectLater(
        f,
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'm',
            contains('shut down'),
          ),
        ),
      );
    });

    test(
      'host calls count toward the transport-wide in-flight tally',
      () async {
        final gate = Completer<void>();
        bridge.async[17] = (_) async {
          await gate.future;
          return _ok((_) {});
        };
        final host = await rt.spawnActorHost(debugName: 'Miner');
        final f = host.call(17, 0, (_) {});
        expect(rt.inFlightCallCount, 1);
        gate.complete();
        await f;
        expect(rt.inFlightCallCount, 0);
      },
    );

    test('an actor host is the same bridge as its runtime', () async {
      final host = await rt.spawnActorHost();
      expect(host.bridgeIdentity, same(rt.bridgeIdentity));
    });
  });

  group('streams', () {
    /// Register a consumer the way a generated `StreamController` parameter
    /// binding does, and return the channel id the request would carry.
    (int, StreamController<int>) openController() {
      final c = StreamController<int>();
      final id = rt.openObject(
        [
          (r) => c.add(r.readI32()),
          (r) => c.addError(BridgeException(r.readString())),
        ],
        (e, st) {
          c.addError(e, st);
          c.close();
        },
        c.close,
        label: 'probe.watch',
      );
      c.onCancel = () => rt.cancelStream(id);
      c.onPause = () => rt.pauseStream(id);
      c.onResume = () => rt.resumeStream(id);
      return (id, c);
    }

    FakeStreamSink<int> sinkFor(int id) => FakeStreamSink<int>(
      FakeRequest(rt, 'probe.watch'),
      id,
      (w, v) => w.writeI32(v),
      hasAddError: true,
    );

    test(
      'items and the close arrive in order, never inline with add',
      () async {
        final (id, c) = openController();
        final sink = sinkFor(id);
        final got = <int>[];
        c.stream.listen(got.add);
        expect(sink.add(1), isTrue);
        expect(got, isEmpty, reason: 'delivery is never synchronous with add');
        expect(sink.add(2), isTrue);
        sink.close();
        await pumpEventQueue();
        expect(got, [1, 2]);
        expect(rt.openChannelCount, 0);
      },
    );

    test('a cancelled subscription stops the producer', () async {
      final (id, c) = openController();
      final sink = sinkFor(id);
      final sub = c.stream.listen((_) {});
      expect(sink.add(1), isTrue);
      await sub.cancel();
      expect(sink.isCancelled, isTrue);
      expect(sink.add(2), isFalse, reason: 'the cooperative flag, as in Rust');
      expect(rt.openChannelCount, 0);
    });

    test(
      'a paused subscription shows as backpressure to the producer',
      () async {
        final (id, c) = openController();
        final sink = sinkFor(id);
        final sub = c.stream.listen((_) {});
        expect(sink.isPaused, isFalse);
        sub.pause();
        await pumpEventQueue();
        expect(sink.isPaused, isTrue);
        // A non-blocking add is unaffected by a pause, exactly as Rust's `add`
        // is: only a producer awaiting `StreamSink::send` parks.
        expect(sink.add(1), isTrue);
        sub.resume();
        await pumpEventQueue();
        expect(sink.isPaused, isFalse);
        await sub.cancel();
      },
    );

    test(
      'a terminal error reaches the consumer as a BridgeException',
      () async {
        final (id, c) = openController();
        final sink = sinkFor(id);
        Object? err;
        c.stream.listen((_) {}, onError: (Object e) => err = e);
        sink.fail('the body returned Err');
        await pumpEventQueue();
        expect(err, isA<BridgeException>());
        expect(sink.isCancelled, isTrue);
        expect(rt.openChannelCount, 0);
      },
    );

    test('a request ends the channels it opened, and keeps the retained one', () async {
      // The mirror image of `FrustrateOpenScope`: on the real bridge a channel
      // ends when the Rust body drops the sink, and Dart has no drop, so the
      // request ends what it opened unless the fake said it kept one.
      final (a, ca) = openController();
      final (b, cb) = openController();
      var aDone = false;
      var bDone = false;
      ca.stream.listen((_) {}, onDone: () => aDone = true);
      cb.stream.listen((_) {}, onDone: () => bDone = true);

      final req = FakeRequest(rt, 'probe.pair');
      req.track(a);
      req.track(b);
      FakeStreamSink<int>(
        req,
        b,
        (w, v) => w.writeI32(v),
        hasAddError: true,
      ).retain();
      req.retire();
      await pumpEventQueue();

      expect(aDone, isTrue, reason: 'the fake let this one go');
      expect(bDone, isFalse, reason: 'the fake said it kept this one');
      expect(rt.openChannelCount, 1);
      rt.cancelStream(b);
    });

    test('addError on a mirror that has none is refused at the fake', () {
      final (id, _) = openController();
      final plain = FakeStreamSink<int>(
        FakeRequest(rt, 'probe.fill'),
        id,
        (w, v) => w.writeI32(v),
        hasAddError: false,
      );
      // Refused here rather than dispatched: a selector-1 event on a
      // one-method registration throws inside the router, on a microtask,
      // where nothing can attribute it.
      expect(
        () => plain.addError('x'),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'm',
            allOf(contains('probe.fill'), contains('addError')),
          ),
        ),
      );
      rt.cancelStream(id);
    });
  });

  group('callbacks', () {
    test('a returning closure round-trips its value', () async {
      // The consumer side of a `DartFunction<int, String>` parameter.
      final id = rt.openFunction((r) {
        final n = r.readI32();
        return BinaryWriter(8)..writeString('n=$n');
      }, label: 'probe.transform');
      final reply = decodeEnvelope(
        await rt.invokeChannel(id, BinaryWriter(4)..writeI32(7)),
      );
      expect(reply.readString(), 'n=7');
      rt.cancelStream(id);
    });

    test('a declared refusal comes back as its typed payload', () async {
      final id = rt.openFunction(
        (r) {
          r.readI32();
          throw ArgumentError('refused');
        },
        label: 'probe.transform',
        onDeclaredError: (e) {
          if (e is! ArgumentError) return null;
          return BinaryWriter(8)..writeI32(7);
        },
      );
      final reply = await rt.invokeChannel(id, BinaryWriter(4)..writeI32(1));
      expect(
        () => decodeEnvelope(
          reply,
          typedError: (r) => StateError('code ${r.readI32()}'),
        ),
        throwsA(isA<StateError>().having((e) => e.message, 'm', 'code 7')),
      );
      rt.cancelStream(id);
    });

    test('an undeclared throw stays the loud path', () async {
      final id = rt.openFunction((r) {
        r.readI32();
        throw StateError('bug in the closure');
      }, label: 'probe.transform');
      final reply = await rt.invokeChannel(id, BinaryWriter(4)..writeI32(1));
      expect(() => decodeEnvelope(reply), throwsA(isA<BridgeException>()));
      rt.cancelStream(id);
    });

    test(
      'invoking a closure the request never registered is a loud failure',
      () async {
        await expectLater(
          rt.invokeChannel(4242, BinaryWriter(0)),
          throwsA(isA<StateError>()),
        );
      },
    );
  });
}
