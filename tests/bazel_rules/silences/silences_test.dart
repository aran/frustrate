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
      _rlocation('bazel/no_block_check/no_block_check'),
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

  group('over a library linking two bridges', () {
    const dir = 'tests/bazel_rules/linked_fixture';

    (int, String) silences(List<String> applied) {
      final out = Directory.systemTemp.createTempSync('silences');
      final r = Process.runSync(
        _rlocation('bazel/no_block_check/no_block_check'),
        [
          '--silences',
          _rlocation('$dir/example/bridge/no_block.claims'),
          '--also-claims',
          _rlocation('$dir/app/bridge/no_block.claims'),
          for (final a in applied)
            _rlocation('$dir/linked_no_block_check.$a.applied'),
          '//$dir:linked_no_block_check.silences',
          '${out.path}/verdict',
        ],
      );
      return (r.exitCode, '${r.stderr}${r.stdout}');
    }

    test('a bridge\'s silences are applied by the analyses that find them', () {
      // `web_threads` names the wait its own way and is not among these, so
      // its name is the one left over; the other two are the bridge's, found.
      final (code, err) = silences(['linux_x86_64', 'macos_aarch64', 'wasi']);
      expect(code, isNot(0), reason: err);
      expect(err, contains('1 silence(s) silence nothing'));
      expect(
        err,
        contains(
          '`std::sys::sync::mutex::futex::Mutex::lock_contended` '
          'on linked_app: read',
        ),
      );
      expect(err, isNot(contains('`pthread_mutex_lock`')));
      expect(err, isNot(contains('FUTEX_WAIT_BITSET')));
    });

    test('a silence no analysis applied is named with its bridge', () {
      // Without the macOS analysis, nothing finds the wait the silences name.
      final (code, err) = silences(['wasi']);
      expect(code, isNot(0), reason: err);
      expect(err, contains('3 silence(s) silence nothing'));
      expect(err, contains('`pthread_mutex_lock` on linked_app: read'));
      expect(err, isNot(contains('linked_example')));
    });
  });
}
