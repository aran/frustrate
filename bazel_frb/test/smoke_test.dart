/// The end-to-end proof that FRB runs under Bazel: a Bazel-built FRB cdylib, loaded
/// by Bazel-built generated Dart, answering through the FRB runtime — with no
/// cargo, no pub and no FRB tooling anywhere in the `bazel test` invocation.
///
/// Both halves arrive through runfiles: the `.dylib` from
/// `//bridge:bazel_frb_bridge` (data), the Dart from `//:bazel_frb_dart`
/// (deps). Nothing here reads the source tree.
library;

import 'dart:io';

import 'package:bazel_frb_dart/bazel_frb_dart.dart';
// `ExternalLibrary` is not on FRB's public surface — it lives in the
// `_for_generated` library, so a caller that needs it imports from there.
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;
import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

String _dylibPath() {
  final name = Platform.isMacOS
      ? 'libbazel_frb_bridge.dylib'
      : Platform.isWindows
      ? 'bazel_frb_bridge.dll'
      : 'libbazel_frb_bridge.so';
  return Runfiles.create().rlocation('_main/bridge/$name');
}

void main() {
  setUpAll(() async {
    await RustLib.init(externalLibrary: ExternalLibrary.open(_dylibPath()));
  });

  test('sync scalars', () {
    expect(add(a: 2, b: 40), 42);
  });

  test('sync String', () {
    expect(greet(name: 'Bazel'), 'Hello, Bazel!');
  });

  test('sync Vec<u8>', () {
    expect(sumBytes(data: [1, 2, 3, 250]), BigInt.from(256));
  });

  test('mirrored struct round-trip', () {
    final p = translate(p: Point(x: 1.0, y: 2.0), dx: 0.5, dy: -0.5);
    expect(p.x, 1.5);
    expect(p.y, 1.5);
  });

  test('async', () async {
    expect(await doubleSlowly(x: 21), 42);
  });

  test('stream', () async {
    expect(await tick(n: 4).toList(), [0, 1, 2, 3]);
  });

  test('typed error', () async {
    expect(await checkedDiv(a: 10, b: 2), 5);
    // `Result<i32, String>` surfaces as a thrown bare `String`, not an
    // exception type — observed, not assumed (the first spelling of this test
    // expected AnyhowException and the failure printed the actual value).
    await expectLater(checkedDiv(a: 1, b: 0), throwsA('division by zero'));
  });
}
