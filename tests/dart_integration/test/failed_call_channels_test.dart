/// A call that fails *before Rust owns it* must not strand the channels it
/// registered.
///
/// Registration happens during request *encoding* — the generated binding opens
/// the channel and writes its id into the buffer, then dispatches. Every path
/// that rejects a call after that point but before Rust decodes it (a shut-down
/// actor host, `PendingCalls.failAll` on a dead pool, a trap on the hand-off)
/// used to leave the registration in the router for the life of the isolate:
/// the consumer's stream neither errored nor closed, `openChannelCount` stayed
/// inflated, and on native the open registration pinned the isolate on a
/// channel nothing
/// would ever feed.
///
/// This is the half of the leak story that the GC finalizer cannot reach —
/// there is no undisposed handle here, and no collection to notice — so it is
/// pinned separately from `gc_finalizer_test`.
///
/// VM-only: it turns on native actor-host teardown and on the isolate-alive
/// contract, neither of which the web transport shares.
@TestOn('vm')
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  setUpAll(initBridge);

  tearDown(() {
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason:
          'a test must not leave a channel registered — its only other '
          'symptom is an isolate that silently never exits. Open: '
          '${Frustrate.instance.openChannelLabels}',
    );
  });

  test('a rejected second sink does not strand the first', () async {
    // The reachable window, and the one the emitted guard's own comment used to
    // wave at: `a` registers, then `b`'s broadcast check throws. Before this
    // change `a` stayed in the router for the life of the isolate.
    final a = StreamController<int>();
    final b = StreamController<int>.broadcast();
    final events = <String>[];
    Object? terminal;
    a.stream.listen(
      (_) => events.add('item'),
      onError: (Object e) {
        terminal = e;
        events.add('error');
      },
      onDone: () => events.add('done'),
    );

    await expectLater(guardedPair(a: a, b: b), throwsA(isA<StateError>()));
    await Future<void>.delayed(Duration.zero);

    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason: 'the sink registered before the throw must be retired',
    );
    expect(
      terminal,
      isA<StateError>(),
      reason:
          'its consumer must be told, not left waiting on a producer '
          'that never existed',
    );
    expect(events, ['error', 'done']);
  }, timeout: const Timeout(Duration(seconds: 30)));

  test('the caller and the stranded channel see the same failure', () async {
    // One failure, two audiences. They must agree: a consumer told something
    // different from the caller is worse than a consumer told nothing.
    final a = StreamController<int>();
    final b = StreamController<int>.broadcast();
    Object? onStream;
    a.stream.listen((_) {}, onError: (Object e) => onStream = e);

    Object? onCall;
    try {
      await guardedPair(a: a, b: b);
    } catch (e) {
      onCall = e;
    }
    await Future<void>.delayed(Duration.zero);

    expect(onCall, isNotNull);
    expect(onStream, same(onCall));
  }, timeout: const Timeout(Duration(seconds: 30)));

  test('a call Rust rejects still ends its streams exactly once', () async {
    // The control, and the guard against over-firing. An envelope error proves
    // Rust received and ran the call, so the channels are its to end and the
    // rollback must stand down: on native its unwinding drop retires them, and
    // on web `panic=abort` leaves them open by design.
    // Rolling back here would be wrong rather than redundant — a member that
    // stored a sink before failing leaves a live clone a later call may feed —
    // and it would double-terminate the consumer. Found by the web suite: an
    // earlier version of this change retired on every failure and broke the
    // three producer-panic tests.
    final a = StreamController<int>();
    final b = StreamController<int>();
    final ea = <String>[];
    final eb = <String>[];
    a.stream.listen(
      (_) => ea.add('item'),
      onError: (Object _) => ea.add('error'),
      onDone: () => ea.add('done'),
    );
    b.stream.listen(
      (_) => eb.add('item'),
      onError: (Object _) => eb.add('error'),
      onDone: () => eb.add('done'),
    );

    await expectLater(guardedPair(a: a, b: b), throwsA(isA<BridgeException>()));
    await Future<void>.delayed(Duration.zero);

    // Each sink got its item, then exactly one terminal.
    expect(ea.where((e) => e == 'done').length, 1, reason: 'exactly one');
    expect(eb.where((e) => e == 'done').length, 1, reason: 'exactly one');
    expect(Frustrate.instance.openChannelCount, 0);
  }, timeout: const Timeout(Duration(seconds: 30)));
}
