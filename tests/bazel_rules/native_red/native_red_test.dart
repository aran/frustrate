/// The native no_block check, run over fixtures it must not pass.
///
/// Each fixture is a real build of a claimed member (BUILD.bazel lists them),
/// for each platform it names, handed to the real scanner exactly as
/// `frustrate_no_block_check` hands it its inputs. A green here would be the
/// check passing a member that can stall its caller.
@TestOn('vm')
library;

import 'dart:io';

import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

String _rlocation(String path) =>
    Platform.environment.containsKey('TEST_SRCDIR')
    ? Runfiles.create().rlocation('_main/$path')
    : path;

/// Every native analysis's platform, with the C library its IR links against.
const _apple = ['ios_aarch64', 'macos_aarch64'];
const _bionic = ['android_aarch64', 'android_x86_64'];
const _glibc = ['linux_aarch64', 'linux_x86_64'];

void main() {
  final scanner = _rlocation('bazel/wasm_block_check/wasm_block_check');

  /// The scanner's exit code and output over one fixture's IR for a platform.
  (int, String) check(
    String package,
    String platform, [
    List<String> args = const [],
  ]) {
    final r = Process.runSync(scanner, [
      '--ir',
      ...args,
      '--claims',
      _rlocation('$package/bridge/no_block.claims'),
      _rlocation('$package/artifacts/$platform.ll'),
      '//$package',
    ]);
    return (r.exitCode, '${r.stderr}${r.stdout}');
  }

  (int, String) red(
    String fixture,
    String platform, [
    List<String> args = const [],
  ]) => check('tests/bazel_rules/native_red/$fixture', platform, args);

  for (final p in ['macos_aarch64', ..._glibc]) {
    test('$p: a member that calls an unclassified C function cannot be '
        'verified', () {
      final (code, err) = red('external', p);
      expect(code, isNot(0), reason: err);
      expect(
        err,
        contains('step\n      cannot verify: reaches native_red_step'),
      );
    });
  }

  for (final (platforms, wait) in [
    (_apple, 'pthread_mutex_lock'),
    ([..._bionic, ..._glibc], 'syscall::SYS_futex::FUTEX_WAIT_BITSET'),
  ]) {
    for (final p in platforms) {
      test('$p: a member that waits is red at the crate\'s own opt-level 2, '
          'named by its C library\'s wait', () {
        final (code, err) = red('optimized', p);
        expect(code, isNot(0), reason: err);
        expect(err, contains('add\n      reaches $wait — waits'));
      });
    }
  }

  for (final p in [..._apple, ..._bionic, ..._glibc]) {
    test('$p: a section elsewhere that sleeps leaves the bounded lock '
        'unproven', () {
      final (code, err) = red('dirty', p);
      expect(code, isNot(0), reason: err);
      expect(
        err,
        contains(
          'cannot verify: takes the bounded lock, whose wait is unproven',
        ),
      );
      expect(err, contains('frustrate_dirty_section_fixture'));
    });
  }

  for (final p in [..._bionic, ..._glibc]) {
    test('$p: std\'s HashMap keys pass, IO `files` declared or not', () {
      for (final args in [
        <String>[],
        ['--io', 'files'],
      ]) {
        final (code, out) = red('hashmap', p, args);
        expect(code, 0, reason: out);
      }
    });
  }
}
