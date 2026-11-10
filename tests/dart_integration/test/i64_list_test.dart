/// `Vec<i64>` and `[i64; N]` across the real bridge, on every Dart backend.
///
/// The Dart type is `I64List`: `Int64List` where `int` is 64 bits (the VM,
/// dart2wasm) and `List<int>` on dart2js and DDC, which cannot construct an
/// `Int64List`. The build picks it (`@rules_dart//dart/constraints:runtime`; the
/// browser driver does the same for `-c`), so one file compiles against either
/// and builds its arguments with `toI64List`. On a JS-number backend the
/// scalar `i64` contract holds: exact to 2^53, a throw beyond, in both
/// directions.
///
/// Native: `bazel test //tests/dart_integration:i64_list_test`.
/// Web: `bazel run //tests/dart_integration:web_test` (dart2wasm), and
/// `-- -c dart2js test/i64_list_test.dart` for the JS-number arm. Nothing in
/// this file may name an integer literal past 2^53: dart2js refuses to compile
/// one.
library;

import 'dart:typed_data';

import 'package:frustrate/frustrate.dart' show toI64List;
import 'package:frustrate/src/js_numbers.dart' show jsMaxExact, kJsNumbers;
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// The container a decoded `Vec<i64>` has on this backend.
final Matcher _backendList = kJsNumbers
    ? isNot(isA<Int64List>())
    : isA<Int64List>();

void main() {
  setUpAll(initBridge);

  test('a decoded Vec<i64> is the backend\'s list, with the same values', () {
    final out = echoI64s(xs: toI64List([1, -2, 3]));
    expect(out, [1, -2, 3]);
    expect(out, _backendList);
    expect(echoI64s(xs: toI64List(const [])), isEmpty);
  });

  test('an Int64List argument still crosses where it exists', () {
    if (kJsNumbers) {
      expect(() => Int64List(1), throwsUnsupportedError);
      return;
    }
    expect(echoI64s(xs: Int64List.fromList([4, -5])), [4, -5]);
  });

  test('2^53 crosses exactly in both directions, on every backend', () {
    final edge = [jsMaxExact, -jsMaxExact, jsMaxExact - 1, 0];
    expect(echoI64s(xs: toI64List(edge)), edge);
  });

  test('a value past 2^53 is refused on encode on a JS-number backend', () {
    // 2^54 is exactly representable as a double, so this is a real value the
    // JS app could hold; it is the wire that cannot carry it faithfully.
    final big = jsMaxExact * 2;
    if (kJsNumbers) {
      expect(() => echoI64s(xs: toI64List([1, big])), throwsUnsupportedError);
    } else {
      expect(echoI64s(xs: toI64List([1, big])), [1, big]);
    }
  });

  test('a value past 2^53 is refused on decode on a JS-number backend', () {
    if (kJsNumbers) {
      expect(i64ListExtremes, throwsUnsupportedError);
    } else {
      expect(i64ListExtremes(), [
        int.parse('-9223372036854775808'),
        int.parse('9223372036854775807'),
      ]);
    }
  });

  test('nested occurrences decode to the backend\'s list', () {
    final opt = echoOptVec(x: toI64List([1, 2]));
    expect(opt, [1, 2]);
    expect(opt, _backendList);
    expect(echoOptVec(x: null), isNull);

    final map = echoMapOfVec(
      m: {
        'a': toI64List([7]),
        'b': toI64List(const []),
      },
    );
    expect(map, {
      'a': [7],
      'b': <int>[],
    });
    expect(map['a'], _backendList);

    final grid = [
      [1, 2, 3],
      [4, 5, 6],
    ];
    final input = [for (final row in grid) toI64List(row)];
    final vv = echoVecOfVec(xss: input);
    expect(vv, grid);
    expect(vv[0], _backendList);
    expect(transpose(grid: input), [
      [1, 4],
      [2, 5],
      [3, 6],
    ]);

    expect(maxOf(xs: toI64List([3, 9, 4])), 9);
  });

  test('[i64; N] in a struct field round-trips fixed-length', () {
    final t = Transform(
      matrix: Float32List(16),
      ids: Int32List(4),
      longs: toI64List([-jsMaxExact, jsMaxExact]),
      corners: const ['', '', ''],
      bounds: const [
        Point(x: 0, y: 0, label: null),
        Point(x: 0, y: 0, label: null),
      ],
      digest: Uint8List(8),
      grid: [Int32List(2), Int32List(2), Int32List(2)],
      flags: const [null, null],
      none: Float64List(0),
    );
    final back = echoTransform(t: t);
    expect(back.longs, [-jsMaxExact, jsMaxExact]);
    expect(back.longs, _backendList);
    expect(() => back.longs.add(0), throwsUnsupportedError);
    expect(
      () => echoTransform(
        t: Transform(
          matrix: t.matrix,
          ids: t.ids,
          longs: toI64List([1]),
          corners: t.corners,
          bounds: t.bounds,
          digest: t.digest,
          grid: t.grid,
          flags: t.flags,
          none: t.none,
        ),
      ),
      throwsArgumentError,
    );
  });
}
