/// `.silences` over silences no analysis applied.
///
/// The applied-silences files are the stale fixture's real analyses' output
/// (BUILD.bazel), handed to the real scanner exactly as `.silences` hands them.
@TestOn('vm')
library;

import 'dart:io';

import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

String _rlocation(String path) =>
    Platform.environment.containsKey('TEST_SRCDIR')
    ? Runfiles.create().rlocation('_main/$path')
    : path;

void main() {
  test('a silence no analysis applied fails .silences, named', () {
    const dir = 'tests/bazel_rules/silences/stale';
    final out = Directory.systemTemp.createTempSync('silences');
    final r = Process.runSync(
      _rlocation('bazel/wasm_block_check/wasm_block_check'),
      [
        '--silences',
        _rlocation('$dir/bridge/no_block.claims'),
        _rlocation('$dir/stale_check.macos_aarch64.applied'),
        _rlocation('$dir/stale_check.wasi.applied'),
        '//$dir:stale_check.silences',
        '${out.path}/verdict',
      ],
    );
    final err = '${r.stderr}${r.stdout}';
    expect(r.exitCode, isNot(0), reason: err);
    expect(err, contains('2 silence(s) silence nothing'));
    expect(
      err,
      contains('`sqlite3_step` on plain: no analysis found anything'),
    );
    expect(
      err,
      contains(
        '`silences_stale::api::plus_one` (`#[frustrate::no_block(allow)]`): '
        'a claimed member reaches it',
      ),
    );
  });
}
