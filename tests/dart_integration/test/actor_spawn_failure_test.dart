/// An actor host that never starts releases its Worker.
///
/// Delivery was already right here — `spawnActorHost` awaits the host's ready
/// signal, so the failure rejects exactly one call. What was missing was the
/// cleanup: neither init-failure path terminated the Worker, and because the
/// rejection makes the half-built host unreachable, nothing ever would. One
/// leaked Worker per failed spawn, held for the page's lifetime — invisible
/// from Dart, which is why it needs a pin rather than a review.
///
/// Runs on both web fixtures: an actor owns its own worker on either, so this
/// has nothing to do with the pool.
///
/// Its own file because it replaces `Worker.prototype.terminate` page-wide
/// and then breaks worker loading.
@TestOn('browser')
library;

import 'dart:js_interop';
import 'dart:js_interop_unsafe';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'guarded_init.dart';

int _terminateCount = 0;

/// Count `terminate()` without delegating to the real one. Deliberate: the
/// page is torn down right after, and not delegating keeps the counter honest
/// about who called rather than about what the browser did.
void _instrumentTerminate() {
  final proto = globalContext
      .getProperty<JSObject>('Worker'.toJS)
      .getProperty<JSObject>('prototype'.toJS);
  proto.setProperty('terminate'.toJS, (() => _terminateCount++).toJS);
}

void main() {
  setUpAll(initBridgeGuarded);

  test(
    'an actor worker that fails to load is terminated, not leaked',
    () async {
      _instrumentTerminate();
      globalContext.setProperty(
        r'$frustrateGlueUrl'.toJS,
        '../build/no-such-glue.js'.toJS,
      );

      await expectLater(
        Miner.new_(label: 'doomed'),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('actor worker script failed to load'),
          ),
        ),
      );

      expect(
        _terminateCount,
        1,
        reason:
            'the half-built host is unreachable once the spawn rejects, '
            'so if it does not terminate its own worker nothing will',
      );
    },
  );
}
