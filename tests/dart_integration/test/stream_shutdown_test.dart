/// Isolate keep-alive: what pins an isolate alive, and what must release it.
/// VM-only — `keepIsolateAlive` is a native-transport concern.
///
/// Two pins, both of which fail as a *hang* rather than a wrong value, so every
/// await below is bounded:
///
/// 1. An open stream vs a long-lived Rust producer.
/// 2. An in-flight async call — which must be released when the call is issued
///    and *fails*, not just when it completes.
///
/// For (1) a spawned isolate opens a stream fed by a detached Rust thread and
/// then returns from its entry function. Contract (b): the open stream keeps
/// that isolate alive to consume the items — exactly as an open `ReceivePort`
/// would — and the isolate exits only once the producer finishes and
/// drop-retire closes the stream. (The prior behavior let the isolate exit
/// immediately, leaving the producer to post into a deleted completion
/// `NativeCallable` — a hard abort, `Callback invoked after it has been
/// deleted`.)
///
/// Own target, own file: the failure mode for (1) used to be undefined
/// behavior that could take the process down. It is a clean assertion now, but
/// the isolation is kept — a regression must not be able to abort the rest of
/// the suite.
@TestOn('vm')
library;

import 'dart:async';
import 'dart:io';
import 'dart:isolate';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

/// How many items the detached Rust ticker posts. Small enough that natural
/// completion (and the isolate exit it triggers) lands well within the test
/// timeout — the ticker paces at ~5ms/item.
const int _tickCount = 40;

/// Entry point for the spawned isolate: init the bridge, listen to a stream fed
/// by a detached Rust thread, and return. Returning from an async entry with
/// nothing else pending is what *would* exit the isolate — but under contract
/// (b) the open stream pins it until the producer finishes. Reports 'opened'
/// once the stream is running, then the consumed item count from `onDone`
/// (which fires as the stream closes, just before the isolate exits).
Future<void> _tickerEntry((SendPort, String, int) args) async {
  final (toMain, libPath, count) = args;
  FrustrateNative.init(libPath);
  final c = StreamController<int>();
  final received = <int>[];
  c.stream.listen(received.add, onDone: () => toMain.send(received.length));
  await spawnTicker(count: count, sink: c);
  toMain.send('opened');
}

/// Entry point for the failed-issue isolate: init the bridge, then issue one
/// async call that cannot reach Rust (the host is already shut down) and
/// return. Registering the call pins the isolate; the pin must be released
/// when the issue fails, or this isolate never exits.
///
/// Reports what the refusal was so the parent can check it arrived through the
/// future rather than as a synchronous throw.
Future<void> _failedIssueEntry((SendPort, String) args) async {
  final (toMain, libPath) = args;
  FrustrateNative.init(libPath);
  final host = await Frustrate.instance.spawnActorHost();
  await host.shutdown();
  // Not wrapped: a synchronous throw here would escape the entry, and the
  // isolate would exit for the wrong reason — reported as an uncaught error,
  // not as the 'refused' below.
  final refused = host.call(0, 0, (w) {});
  try {
    await refused;
    toMain.send('completed');
  } on StateError catch (e) {
    toMain.send('refused: ${e.message}');
  }
}

void main() {
  final libPath = _bridgeLibraryPath();

  setUpAll(() {
    // The main isolate reads the process-global ticker counters; the ticker's
    // sink belongs to the spawned isolate.
    FrustrateNative.init(libPath);
  });

  test(
    'a Rust producer keeps its isolate alive until the stream ends',
    () async {
      final fromWorker = ReceivePort();
      final onExit = ReceivePort();
      await Isolate.spawn(_tickerEntry, (
        fromWorker.sendPort,
        libPath,
        _tickCount,
      ), onExit: onExit.sendPort);

      final opened = Completer<void>();
      final consumed = Completer<int>();
      fromWorker.listen((msg) {
        if (msg == 'opened') {
          opened.complete();
        } else if (msg is int) {
          consumed.complete(msg);
        }
      });
      final exited = Completer<void>();
      onExit.listen((_) {
        if (!exited.isCompleted) exited.complete();
      });

      await opened.future;
      // The isolate has returned from its entry but must stay alive to consume:
      // while the producer is still ticking, it must NOT have exited yet.
      await Future<void>.delayed(const Duration(milliseconds: 60));
      expect(
        exited.isCompleted,
        isFalse,
        reason:
            'the isolate must stay alive while its stream is producing '
            '(the open stream pins it, like a ReceivePort)',
      );

      // The producer finishes on its own; drop-retire closes the stream, which
      // unpins the isolate, which then exits cleanly — no UB, no abort.
      final count = await consumed.future.timeout(const Duration(seconds: 10));
      expect(
        count,
        _tickCount,
        reason: 'every item must be consumed before the stream closes',
      );
      await exited.future.timeout(
        const Duration(seconds: 10),
        onTimeout: () => fail(
          'the isolate must exit once its stream closes — '
          'a hang here means the pin was never released',
        ),
      );

      // It ran to completion — never refused (not cancelled, and its consumer
      // stayed alive the whole time). Read after exit, when the thread is done.
      expect(
        tickerRefused(),
        isFalse,
        reason: 'a live consumer must never refuse the producer',
      );
      expect(tickerPosted(), _tickCount);

      fromWorker.close();
      onExit.close();
    },
    timeout: const Timeout(Duration(seconds: 30)),
  );

  test('an async call that fails to issue releases the isolate', () async {
    // Registering a call pins the isolate — it owes someone a completion. If
    // the issue then fails before Rust ever has the call, the registration has
    // to be rolled back, or `keepIsolateAlive` stays latched on for good and
    // the isolate silently never exits.
    // That is the whole failure mode: not a wrong value, a process that hangs.
    final fromWorker = ReceivePort();
    final onExit = ReceivePort();
    await Isolate.spawn(_failedIssueEntry, (
      fromWorker.sendPort,
      libPath,
    ), onExit: onExit.sendPort);

    final outcome = Completer<String>();
    fromWorker.listen((msg) {
      if (msg is String && !outcome.isCompleted) outcome.complete(msg);
    });
    final exited = Completer<void>();
    onExit.listen((_) {
      if (!exited.isCompleted) exited.complete();
    });

    expect(
      await outcome.future.timeout(const Duration(seconds: 10)),
      contains('actor host was shut down'),
      reason: 'the refusal must arrive through the future',
    );
    await exited.future.timeout(
      const Duration(seconds: 10),
      onTimeout: () => fail(
        'the isolate must exit after a failed issue — '
        'a hang here means the failed call left its keep-alive pin on',
      ),
    );

    fromWorker.close();
    onExit.close();
  }, timeout: const Timeout(Duration(seconds: 30)));
}

/// Locate the bridge dylib. Mirrors init_native.dart: Bazel supplies it as a
/// runfile (TEST_SRCDIR set), the cargo loop uses the cargo target dir.
String _bridgeLibraryPath() {
  if (Platform.environment.containsKey('TEST_SRCDIR')) {
    return Runfiles.create().rlocation(
      '_main/tests/test_api/libtest_api_shared.$_libExt',
    );
  }
  final lib = File('../../target/debug/libtest_api.$_libExt').absolute.path;
  if (!File(lib).existsSync()) {
    fail('libtest_api.$_libExt not found; run `cargo build -p test_api` first');
  }
  return lib;
}

/// The shared-library extension of the host the bridge was built for.
final String _libExt = Platform.isMacOS
    ? 'dylib'
    : Platform.isWindows
    ? 'dll'
    : 'so';
