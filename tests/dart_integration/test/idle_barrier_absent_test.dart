/// `FrustrateNative.idleBarrier` against an ordinary bridge build refuses,
/// rather than handing a harness a barrier that counts nothing.
@TestOn('vm')
library;

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  test('before init there is no bridge to ask', () {
    expect(
      FrustrateNative.idleBarrier,
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('FrustrateNative.init'),
        ),
      ),
    );
  });

  test('a build without the barrier says how to get one', () async {
    await initBridge();
    expect(
      FrustrateNative.idleBarrier,
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('frustrate_idle_barrier_library'),
        ),
      ),
    );
  });
}
