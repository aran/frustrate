/// What a guarded zone is told about errors nobody caught.
library;

import 'dart:async';

/// The errors a `runZonedGuarded` zone has heard, and a way to wait for the
/// first. Waiting is unbounded; the test's own timeout ends a wait for an
/// error that never comes.
final class ZoneHeard {
  final List<Object> errors = [];
  final Completer<void> _first = Completer<void>();

  /// Completes once the zone has heard an error.
  Future<void> get first => _first.future;

  void _hear(Object error) {
    errors.add(error);
    if (!_first.isCompleted) _first.complete();
  }

  /// Run [body] in a zone that records its uncaught errors here; answer
  /// [body]'s result, or fail with its error.
  Future<T> run<T>(Future<T> Function() body) {
    final done = Completer<T>();
    runZonedGuarded(
      () => body().then(done.complete, onError: done.completeError),
      (e, _) => _hear(e),
    );
    return done.future;
  }
}

/// Run [body] in a fresh guarded zone; answer its result and what the zone
/// hears.
Future<(T, ZoneHeard)> guarded<T>(Future<T> Function() body) async {
  final heard = ZoneHeard();
  return (await heard.run(body), heard);
}
