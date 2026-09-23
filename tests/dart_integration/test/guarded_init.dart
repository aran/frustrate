/// Bridge init wrapped in `runZonedGuarded`, the way an app that wants to hear
/// about pool degradation would write it.
///
/// A pool worker's spawn failure is discovered on a bare JS event callback —
/// `worker.onerror`, or an `initError` message — with no Dart caller above it
/// and no future it would be honest to fail. The runtime therefore reports it
/// to the zone captured when the transport was installed. Wrapping init is
/// what turns that into an
/// error the surrounding code can log, ignore, or rethrow; these suites double
/// as the proof that it does.
library;

import 'dart:async';

import 'init_web.dart';

/// Errors the runtime reported into the init zone after init completed.
final List<Object> zoneErrors = [];

/// [initBridge], but inside a guarded zone whose errors land in [zoneErrors].
/// A failure *during* init still surfaces through the returned future, so a
/// broken fixture fails the suite rather than hanging it.
Future<void> initBridgeGuarded() {
  final ready = Completer<void>();
  runZonedGuarded(
    () async {
      await initBridge();
      ready.complete();
    },
    (e, st) {
      if (!ready.isCompleted) {
        ready.completeError(e, st);
        return;
      }
      zoneErrors.add(e);
    },
  );
  return ready.future;
}

/// Poll until [predicate] holds, or fail with [reason].
///
/// `worker.onerror` is asynchronous and unordered with respect to the call
/// that triggered the spawn, so a suite that induces a spawn failure has to
/// wait for it rather than assume it landed.
Future<void> until(bool Function() predicate, String reason) async {
  final deadline = DateTime.now().add(const Duration(seconds: 10));
  while (!predicate()) {
    if (DateTime.now().isAfter(deadline)) {
      throw StateError('frustrate test: timed out waiting for $reason');
    }
    await Future<void>.delayed(const Duration(milliseconds: 10));
  }
}
