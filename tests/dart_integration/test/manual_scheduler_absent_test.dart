/// `FrustrateNative.manualDrain` against an ordinary bridge build refuses,
/// rather than handing a harness a drain that would never be the one running
/// its Rust tasks.
library;

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  test('before init there is no bridge to drain', () {
    expect(
      FrustrateNative.manualDrain,
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('FrustrateNative.init'),
        ),
      ),
    );
  });

  test('a pool build has no drain, and says how to get one', () async {
    await initBridge();
    expect(
      FrustrateNative.manualDrain,
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('frustrate_manual_scheduler_library'),
        ),
      ),
    );
  });
}
