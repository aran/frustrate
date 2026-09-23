/// A disposed actor releases its Worker.
///
/// `_WebActorHost.shutdown` kills the worker outright — native drains its FIFO
/// to a Stop marker, web does not. A `shutdown` that stops calling
/// `terminate()` leaves a Worker running for the life of the page: one per
/// actor, and at `ActorPool`'s default width one per core per pool.
///
/// A `terminate()` counter cannot see this. `actor_spawn_failure_test`
/// deliberately replaces `Worker.prototype.terminate` with a counter, which
/// proves *someone called a method* — and would have gone on passing if the
/// method it counts had stopped being called on the healthy path, which is
/// exactly what happened. So the assertion here is liveness instead: post the
/// worker a message its own pump answers, and see whether an answer arrives. A
/// terminated Worker never runs another task, so silence is death and a reply
/// is life; neither is anything Dart is keeping score of.
///
/// The probe is self-validating, which is the point of the two actors. One is
/// probed while it is unquestionably alive (it has just answered a real bridge
/// call) and must come back alive; the other is probed after `dispose()` and
/// must come back dead. A probe that could only ever say "dead" would pass this
/// file's second assertion against the broken build, so the first one is not
/// decoration.
///
/// The live actor is abandoned rather than disposed: probing it instantiates a
/// throwaway module over its pump's `exports`, so it can no longer serve calls.
/// The page is torn down immediately afterwards.
///
/// Runs on both web fixtures — an actor owns its own worker on either, so this
/// has nothing to do with the pool. Its own file because it replaces
/// `globalThis.Worker` page-wide.
@TestOn('browser')
library;

import 'dart:js_interop';
import 'dart:js_interop_unsafe';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'guarded_init.dart';

@JS('eval')
external void _eval(String source);

/// Record every Worker the page constructs, and install the liveness probe.
/// Installed before init, so the capture list is exactly the runtime's own
/// workers in creation order.
void _instrument() {
  _eval(r'''
    (() => {
      const Real = globalThis.Worker;
      globalThis.$frustrateTestWorkers = [];
      globalThis.Worker = function (url, opts) {
        const w = new Real(url, opts);
        globalThis.$frustrateTestWorkers.push(w);
        return w;
      };

      // Liveness, not bookkeeping. The glue's actor pump answers an `init`
      // message on the way in or on the way out, so a reply of any kind means
      // the Worker still has a running event loop. The module posted is the
      // 8-byte empty one: valid enough for the pump to instantiate, after
      // which it throws on the missing `frustrate_web_init` export and reports
      // `initError`. A terminated Worker sends nothing at all.
      globalThis.$frustrateTestAlive = (w) => new Promise((resolve) => {
        const timer = setTimeout(() => resolve(false), 2000);
        const alive = () => { clearTimeout(timer); resolve(true); };
        w.onmessage = alive;
        w.onerror = alive;
        w.postMessage({
          type: 'init',
          module: new WebAssembly.Module(
              new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0])),
          memInitial: 1,
          memMaximum: 1,
        });
      });
    })();
  ''');
}

@JS(r'$frustrateTestAlive')
external JSPromise<JSBoolean> _alive(JSObject worker);

/// Does [worker] still run tasks? Resolves false once the probe's budget
/// expires with no reply.
Future<bool> _isAlive(JSObject worker) async =>
    (await _alive(worker).toDart).toDart;

List<JSObject> _createdWorkers() =>
    (globalContext.getProperty<JSArray<JSObject>>(
      r'$frustrateTestWorkers'.toJS,
    )).toDart;

void main() {
  setUpAll(() async {
    _instrument();
    await initBridgeGuarded();
  });

  test('the probe reports a working actor worker as alive', () async {
    final before = _createdWorkers().length;
    final miner = await Miner.new_(label: 'probe control');
    expect(
      _createdWorkers(),
      hasLength(before + 1),
      reason: 'an actor spawns exactly one worker',
    );
    final worker = _createdWorkers()[before];

    // Unquestionably alive: it just served a real bridge call.
    expect(await miner.label(), 'probe control');

    expect(
      await _isAlive(worker),
      isTrue,
      reason:
          'a running actor worker answers its own pump — without this '
          'the negative below would pass against any build',
    );
    // Deliberately not disposed: the probe replaced this pump's instance.
  });

  test('dispose() terminates the actor worker', () async {
    final before = _createdWorkers().length;
    final miner = await Miner.new_(label: 'teardown');
    expect(_createdWorkers(), hasLength(before + 1));
    final worker = _createdWorkers()[before];
    expect(await miner.label(), 'teardown');

    await miner.dispose();

    expect(
      await _isAlive(worker),
      isFalse,
      reason:
          'once dispose() returns the host is unreachable from Dart, so '
          'a worker it did not terminate will run until the page goes away',
    );
  });
}
