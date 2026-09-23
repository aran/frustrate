/// The web glue exists in more than one carrier, and this file is what keeps
/// the copies from drifting.
///
/// Two pins, for two different duplications:
///
///  1. `glue_source.dart` (the no-CSP inline/blob fallback) must be
///     byte-identical to `lib/src/js/frustrate.js` (the strict-CSP served
///     asset). One behavior, two carriers.
///  2. The list of import namespaces the runtime supplies itself is stated
///     twice — once in JS, once in Dart — because instantiation happens in both
///     contexts. Same rule, two languages, no mechanism forcing agreement.
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

/// The members of a Dart `static const Set<String> _name = {...}` literal.
Set<String> _dartSetLiteral(String source, String name) {
  final open = source.indexOf('$name = {');
  expect(open, isNot(-1), reason: 'no `$name = {` in runtime_web.dart');
  final close = source.indexOf('};', open);
  return RegExp(r"'([^']+)'")
      .allMatches(source.substring(open, close))
      .map((m) => m.group(1)!)
      .toSet();
}

/// The members of a JS `const name = [...]` array literal.
Set<String> _jsArrayLiteral(String source, String name) {
  final open = source.indexOf('const $name = [');
  expect(open, isNot(-1), reason: 'no `const $name = [` in frustrate.js');
  final close = source.indexOf('];', open);
  return RegExp(r"'([^']+)'")
      .allMatches(source.substring(open, close))
      .map((m) => m.group(1)!)
      .toSet();
}

void main() {
  late String js;
  setUpAll(
    () => js = File(_srcPath('lib/src/js/frustrate.js')).readAsStringSync(),
  );

  test('frustrate.js and frustrateGlueSource are byte-identical', () {
    expect(
      frustrateGlueSource,
      js,
      reason:
          'lib/src/js/frustrate.js (served asset) and '
          'lib/src/glue_source.dart (embedded fallback) must stay '
          'byte-identical — edit both together',
    );
  });

  test('both loaders agree on which import namespaces the runtime supplies', () {
    // The rule decides one thing: whether a foreign import namespace means
    // "this module needs a wasm-bindgen sidecar". Getting the list wrong in
    // either carrier is not a subtle failure — omitting
    // `wasi_snapshot_preview1` made EVERY wasm32-wasip1 module refuse to load,
    // with an error telling the developer to pass a wasm-bindgen glue URL for
    // a platform that cannot use wasm-bindgen at all. Found by the wasi
    // example's browser test, which is a separate Bazel module; this pin puts
    // the guard in the wildcard.
    final dart = _dartSetLiteral(
      File(_srcPath('lib/src/runtime_web.dart')).readAsStringSync(),
      '_selfSuppliedNamespaces',
    );
    expect(
      dart,
      _jsArrayLiteral(js, 'selfSupplied'),
      reason:
          'runtime_web.dart `_selfSuppliedNamespaces` and frustrate.js '
          '`selfSupplied` state the same rule for two instantiation '
          'contexts (the main instance in Dart, pool and actor workers in '
          'JS) and must list the same namespaces',
    );
    expect(dart, contains('frustrate'));
    expect(dart, contains('wasi_snapshot_preview1'));
  });
}
