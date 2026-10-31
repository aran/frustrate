/// `glue_source.dart` (the no-CSP inline/blob fallback) must be byte-identical
/// to `lib/src/js/frustrate.js` (the strict-CSP served asset): one behavior,
/// two carriers.
@TestOn('vm')
library;

import 'dart:io';

import 'package:frustrate/src/glue_source.dart';
import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

/// Bazel runs this test with runfiles; the pub loop (`dart test` in
/// runtime/dart) runs with cwd at the package root. TEST_SRCDIR is the
/// canonical Bazel-test marker.
String _srcPath(String relative) {
  if (Platform.environment.containsKey('TEST_SRCDIR')) {
    return Runfiles.create().rlocation('_main/runtime/dart/$relative');
  }
  return relative;
}

void main() {
  test('frustrate.js and frustrateGlueSource are byte-identical', () {
    expect(
      frustrateGlueSource,
      File(_srcPath('lib/src/js/frustrate.js')).readAsStringSync(),
      reason:
          'lib/src/js/frustrate.js (served asset) and '
          'lib/src/glue_source.dart (embedded fallback) must stay '
          'byte-identical — edit both together',
    );
  });
}
