/// `FrustrateNative.idleBarrier` against a bridge built with the idle barrier
/// (`:test_api_idle`): it waits for a pool job, an actor call and an `async fn`
/// alike, and every one of their answers is handled before it completes.
@TestOn('vm')
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  setUpAll(initBridge);

  test('with nothing running it completes at once', () async {
    final barrier = FrustrateNative.idleBarrier();
    final generation = await barrier.whenIdle();
    expect(barrier.pending, 0);
    expect(barrier.generation, generation);
  });

  test(
    'it waits for every kind of job, and their answers come first',
    () async {
      final barrier = FrustrateNative.idleBarrier();
      final miner = await Miner.new_(label: 'idle');
      final answered = <String>[];
      final calls = [
        // A rendezvous nobody else joins: a pool job that waits out its 50 ms.
        poolRendezvous(
          width: 1 << 30,
          maxWaitMs: 50,
        ).then((_) => answered.add('pool')),
        miner.sleepOnExecutor(millis: 50).then((_) => answered.add('actor')),
        withdrawAwaiting(
          balance: 10,
          amount: 3,
        ).then((_) => answered.add('async fn')),
      ];
      expect(barrier.pending, greaterThan(0));

      final generation = await barrier.whenIdle();
      expect(answered, unorderedEquals(['pool', 'actor', 'async fn']));
      expect(barrier.pending, 0);
      expect(barrier.generation, generation);

      await Future.wait(calls);
      await miner.dispose();
    },
  );
}
