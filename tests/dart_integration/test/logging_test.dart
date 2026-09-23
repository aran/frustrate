/// Rust's `log` facade reaching Dart as a stream, on both transports.
///
/// The facility is a runtime pair — `frustrate::logging::install` plus a
/// `StreamSink` — with no codegen involvement at all, so what this file
/// exercises is deliberately ordinary bridge surface: a bridged struct, a sink
/// parameter, a `Result`. The properties that are *not* ordinary and that this
/// pins:
///
///   * a record logged from inside the logger is dropped and counted, not
///     recursed and not deadlocked — the sharpest correctness question in the
///     design, asked here by actually doing it (a `Display` impl that logs
///     while the mapper formats it);
///   * a second install replaces the first and closes its stream, which is the
///     Flutter hot-restart path;
///   * a cancelled subscription retires the logger, and the records that pass
///     while it does are counted rather than lost silently;
///   * an actor's records reach the logger installed *in the actor's own
///     instance* — and the platforms differ here, deliberately, because on web
///     an actor is a separate wasm instance with its own `static`s while
///     natively there is one process-global logger. Both are asserted by name.
///
/// Native prerequisite: `cargo build -p test_api`.
/// Web prerequisite: `dart run tool/build_web_fixture.dart`.
library;

import 'dart:async';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// One Dart consumer of a logging stream: the controller to hand Rust, what
/// arrived, and whether the stream ended.
class _Consumer {
  _Consumer() {
    subscription = controller.stream.listen(
      received.add,
      onDone: () {
        closed = true;
      },
    );
  }

  final StreamController<LogLine> controller = StreamController<LogLine>();
  final List<LogLine> received = <LogLine>[];
  late final StreamSubscription<LogLine> subscription;
  bool closed = false;

  List<String> get messages => received.map((r) => r.message).toList();

  Future<void> release() async {
    await subscription.cancel();
    if (!controller.isClosed) await controller.close();
  }
}

/// Turn the event loop until [condition] holds, then assert it did.
///
/// Delivery is asynchronous on both transports and by different mechanisms —
/// a port message natively, a microtask on web — so nothing here may assume a
/// record has landed by the time the call that produced it returns. Bounded,
/// so a regression is a failed expectation naming the property rather than a
/// suite that hangs.
Future<void> until(bool Function() condition, String what) async {
  for (var i = 0; i < 500 && !condition(); i++) {
    await Future<void>.delayed(Duration.zero);
  }
  expect(condition(), isTrue, reason: what);
}

/// Turn the event loop a fixed number of times, for assertions about something
/// *not* arriving. There is no event to wait for, so this is the only shape
/// available; it runs the same number of turns [until] would.
Future<void> settle() async {
  for (var i = 0; i < 500; i++) {
    await Future<void>.delayed(Duration.zero);
  }
}

void main() {
  setUpAll(initBridge);

  tearDown(() async {
    // The Rust-side release lever. Natively this covers an actor's install too
    // — `log`'s slot is process-global there — so no test can leave a logger
    // holding a sink into the next one.
    uninstallLogging();
  });

  test('a record carries its level, its target, and its message', () async {
    final logs = _Consumer();
    installLogging(maxLevel: 'trace', sink: logs.controller);

    emitLog(level: 'info', message: 'hello');
    emitLog(level: 'error', message: 'boom');
    await until(() => logs.received.length == 2, 'two records must arrive');

    expect(logs.received[0].level, 'INFO');
    expect(logs.received[1].level, 'ERROR');
    expect(logs.messages, ['hello', 'boom']);
    expect(
      logs.received[0].target,
      contains('test_api'),
      reason: 'the target is the logging module path, which log fills in',
    );
    await logs.release();
  });

  test("the filter is log's own, so an excluded record never reaches the "
      'bridge and is not a drop', () async {
    final logs = _Consumer();
    installLogging(maxLevel: 'warn', sink: logs.controller);

    final before = loggingDropped();
    emitLog(level: 'debug', message: 'below the filter');
    emitLog(level: 'warn', message: 'above the filter');
    await until(() => logs.received.isNotEmpty, 'the warn record must arrive');
    await settle();

    expect(
      logs.messages,
      ['above the filter'],
      reason:
          'records arrive in order on one channel, so seeing the second '
          'proves the first was never sent',
    );
    expect(
      loggingDropped() - before,
      BigInt.zero,
      reason:
          'a record the macro filtered out never reached the logger, so '
          'it is not something the logger dropped',
    );
    await logs.release();
  });

  test('an async body reaches the same logger, wherever it runs', () async {
    final logs = _Consumer();
    installLogging(maxLevel: 'trace', sink: logs.controller);

    // A pool worker natively; the calling thread on single-threaded web. One
    // process-global (instance-global, on web) logger is what makes those the
    // same sink.
    await emitLogAsync(level: 'warn', message: 'from an async body');
    await until(() => logs.received.isNotEmpty, 'the async record must arrive');

    expect(logs.messages, ['from an async body']);
    expect(logs.received.single.level, 'WARN');
    await logs.release();
  });

  test('a record logged from inside the logger is dropped and counted, '
      'not recursed', () async {
    final logs = _Consumer();
    installLogging(maxLevel: 'trace', sink: logs.controller);

    final before = loggingDropped();
    // `emitReentrantLog` logs a value whose `Display` impl logs. The mapper
    // formats it, so the inner record is emitted while this thread is already
    // inside the logger. Without the re-entrancy guard this is unbounded
    // recursion; the call returning at all is half the assertion.
    emitReentrantLog();
    await until(() => logs.received.isNotEmpty, 'the outer record must arrive');
    await settle();

    expect(
      logs.messages,
      ['outer record'],
      reason:
          'the outer record is delivered exactly once, and the inner one '
          'is not delivered at all',
    );
    expect(
      loggingDropped() - before,
      BigInt.one,
      reason: 'the re-entrant record is counted, so it is not silent',
    );
    await logs.release();
  });

  test(
    'installing again redirects, and closes the stream it displaced',
    () async {
      final first = _Consumer();
      installLogging(maxLevel: 'trace', sink: first.controller);
      emitLog(level: 'info', message: 'before the restart');
      await until(
        () => first.received.isNotEmpty,
        'the first record must arrive',
      );

      // What a Flutter hot restart does: Dart `main()` runs again in the same
      // process, with a new controller, while the old isolate's is dead.
      final second = _Consumer();
      installLogging(maxLevel: 'trace', sink: second.controller);
      await until(
        () => first.closed,
        'the displaced sink drops, which ends its Dart stream',
      );

      emitLog(level: 'info', message: 'after the restart');
      await until(
        () => second.received.isNotEmpty,
        'the new stream must receive',
      );
      expect(first.messages, [
        'before the restart',
      ], reason: 'nothing may reach the stream that was displaced');
      expect(second.messages, ['after the restart']);
      await first.release();
      await second.release();
    },
  );

  test('cancelling the subscription retires the logger, and the records that '
      'pass while it does are counted', () async {
    final logs = _Consumer();
    installLogging(maxLevel: 'trace', sink: logs.controller);
    await logs.subscription.cancel();

    final before = loggingDropped();
    emitLog(level: 'info', message: 'first after cancel');
    emitLog(level: 'info', message: 'second after cancel');
    await settle();

    expect(logs.received, isEmpty);
    expect(
      loggingDropped() - before,
      BigInt.two,
      reason:
          'the record that discovers the cancellation is dropped, and so '
          'is every one after it',
    );

    // Retired, not merely refusing: a fresh install works, which it would not
    // if the dead registration were still in the slot.
    final next = _Consumer();
    installLogging(maxLevel: 'trace', sink: next.controller);
    emitLog(level: 'info', message: 'live again');
    await until(() => next.received.isNotEmpty, 'the new logger must deliver');
    expect(next.messages, ['live again']);
    await next.release();
    if (!logs.controller.isClosed) await logs.controller.close();
  });

  test("an actor's records reach the logger installed in the actor's own "
      'instance', () async {
    final page = _Consumer();
    installLogging(maxLevel: 'trace', sink: page.controller);

    final miner = await Miner.new_(label: 'logging');
    final actor = _Consumer();
    await miner.installLogging(maxLevel: 'trace', sink: actor.controller);
    await miner.emitLog(message: 'from the actor');
    await until(
      () => actor.received.isNotEmpty,
      "the actor's own logger must receive its records",
    );
    expect(actor.messages, ['from the actor']);

    // The one place the two platforms genuinely differ: `log`'s logger is a
    // `static`, natively there is one process, and on web an actor is a
    // separate wasm instance with its own copy.
    await settle();
    if (isNativeVm) {
      expect(
        page.closed,
        isTrue,
        reason:
            "natively log's slot is process-global, so the actor's "
            'install displaced the free function\'s and closed its stream',
      );
      emitLog(level: 'info', message: 'from the page');
      await until(
        () => actor.received.length == 2,
        'natively every record now goes to the most recent install',
      );
      expect(actor.messages, ['from the actor', 'from the page']);
    } else {
      expect(
        page.closed,
        isFalse,
        reason:
            'on web the two installs are in different wasm instances, so '
            'neither can displace the other',
      );
      emitLog(level: 'info', message: 'from the page');
      await until(
        () => page.received.isNotEmpty,
        "on web the page instance's logger is untouched by the actor's",
      );
      expect(page.messages, ['from the page']);
      expect(actor.messages, [
        'from the actor',
      ], reason: "a page record cannot reach the actor's instance");
    }

    await actor.release();
    await page.release();
    await miner.dispose();
  });
}
