/// A bridge loads and answers: one sync call, one async call, data round trips,
/// an error and a panic. Small enough to run on every platform CI builds for,
/// and free of integer literals past 2^53, so dart2js compiles it.
///
/// Native: `bazel test //tests/dart_integration:smoke_test`.
/// Web: `bazel run //tests/dart_integration:web_test -- test/smoke_test.dart`,
/// with `-c dart2js` for the JS backend (the default is dart2wasm).
library;

import 'package:frustrate/frustrate.dart' show toI64List;
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('sync call', () {
    expect(addI32(a: 2, b: 40), 42);
    expect(addI32(a: -7, b: 7), 0);
  });

  test('async call', () async {
    final joined = await concatStrings(
      parts: ['héllo', '👨‍👩‍👧‍👦', 'мир'],
      sep: ' ~ ',
    );
    expect(joined, 'héllo ~ 👨‍👩‍👧‍👦 ~ мир');
  });

  test('scalars, a map and a Vec<i64> round-trip', () {
    expect(
      mixScalars(
        b: true,
        x8: -8,
        x16: -1600,
        x32: -320000,
        x64: -64000000000,
        u8v: 200,
        u16v: 65000,
        u32v: 4000000000,
        f32v: 1.5,
        f64v: 2.25,
        us: 12345,
        is_: -54321,
        u64v: BigInt.parse('18446744073709551615'),
      ),
      'true|-8|-1600|-320000|-64000000000|200|65000|4000000000|1.5|2.25|12345|-54321|18446744073709551615',
    );
    expect(invertMap(m: {'a': 1, 'b': 2}), {1: 'a', 2: 'b'});
    expect(echoI64s(xs: toI64List([1, -2, 3])), [1, -2, 3]);
  });

  test('a data enum round-trips every variant', () {
    final patches = echoPatches(
      ps: [
        const TextPatchSplice(index: 3, text: 'héllo'),
        const TextPatchDelete(index: 0, length: 2),
        const TextPatchMark('bold', -5),
        const TextPatchClear(),
      ],
    );
    expect(patches.length, 4);
    expect((patches[0] as TextPatchSplice).text, 'héllo');
    expect((patches[1] as TextPatchDelete).length, 2);
    expect((patches[2] as TextPatchMark).field1, -5);
    expect(patches[3], isA<TextPatchClear>());
  });

  test('an Err surfaces as BridgeException, sync and async', () async {
    expect(parseNumber(s: ' 42 '), 42);
    expect(
      () => parseNumber(s: ''),
      throwsA(
        isA<BridgeException>().having(
          (e) => e.message,
          'message',
          contains('empty input'),
        ),
      ),
    );
    await expectLater(
      parseNumberAsync(s: 'not a number'),
      throwsA(isA<BridgeException>()),
    );
  });

  test('a panic surfaces as BridgePanicException', () {
    expect(
      () => alwaysPanics(),
      throwsA(
        isA<BridgePanicException>().having(
          (e) => e.message,
          'message',
          contains('deliberate panic'),
        ),
      ),
    );
  });
}
