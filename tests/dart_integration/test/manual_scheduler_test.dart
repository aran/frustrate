/// `FrustrateNative.manualDrain` against a bridge built with the manual
/// scheduler (`:test_api_manual`): a Rust `async fn` makes no progress until
/// the harness drains, and one drain carries it to its answer.
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  setUpAll(initBridge);

  test('an async fn runs only when the harness drains', () async {
    final drain = FrustrateNative.manualDrain();
    expect(drain(), isFalse, reason: 'nothing has been spawned yet');

    var answered = false;
    final result = withdrawAwaiting(
      balance: 10,
      amount: 3,
    ).whenComplete(() => answered = true);
    await pumpEventQueue();
    expect(answered, isFalse, reason: 'no drain, so the body never polled');

    // The body suspends once and wakes itself; the drain polls until the run
    // queue is empty, so both polls happen inside this one call.
    expect(drain(), isTrue);
    expect(await result, 7);
    expect(drain(), isFalse, reason: 'the finished task left nothing behind');
  });
}
