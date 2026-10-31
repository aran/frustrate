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

  for (final p in _glibc) {
    test('$p: a thread-local with a destructor registers it under glibc\'s '
        'loader lock', () {
      final (code, err) = check('tests/bazel_rules/thread_local_fixture', p);
      expect(code, isNot(0), reason: err);
      expect(
        err,
        contains('seen\n      reaches __cxa_thread_atexit_impl — waits'),
      );
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

  for (final p in [..._bionic, ..._glibc]) {
    test('$p: a call through memchr\'s chosen routine is followed', () {
      final (code, out) = red('memchr', p);
      expect(code, 0, reason: out);
    });
  }

  const mac = 'macos_aarch64';

  test('a silence covers only the member it is written on', () {
    final (code, err) = red('silenced', mac);
    expect(code, isNot(0), reason: err);
    expect(
      err,
      contains('plain\n      cannot verify: reaches native_red_silenced_step'),
    );
    expect(err, isNot(contains('silenced\n      cannot verify')));
  });

  test('a wait behind a function marked no_block(allow) passes, listed', () {
    final (code, out) = red('marked', mac);
    expect(code, 0, reason: out);
    expect(
      out,
      contains(
        '`native_red_marked::api::lock_uncontended_here` in every claim '
        '(`#[frustrate::no_block(allow)]`)',
      ),
    );
  });

  test('a member that logs through an installed logger cannot be verified', () {
    final (code, err) = red('logger', mac);
    expect(code, isNot(0), reason: err);
    expect(
      err,
      contains('work\n      cannot verify: reaches an indirect call in'),
    );
    expect(err, contains('loaded from `log::LOGGER`'));
  });

  test(
    'a member that calls a Box<dyn Fn> behind a static cannot be verified',
    () {
      final (code, err) = red('callback', mac);
      expect(code, isNot(0), reason: err);
      expect(
        err,
        contains('call\n      cannot verify: reaches an indirect call in'),
      );
      expect(err, contains('loaded from `native_red_callback::api::CALLBACK`'));
    },
  );

  test('a member that sleeps from inline asm is red', () {
    final (code, err) = red('asm', mac);
    expect(code, isNot(0), reason: err);
    expect(err, contains('jump\n      reaches nanosleep — waits'));
  });

  group('a library linking two bridges', () {
    const dir = 'tests/bazel_rules/linked_fixture';
    final example = _rlocation('$dir/example/bridge/no_block.claims');
    final app = _rlocation('$dir/app/bridge/no_block.claims');
    final ir = _rlocation('$dir/artifacts/$mac.ll');

    (int, String) linked(List<String> censuses) {
      final r = Process.runSync(scanner, [
        '--ir',
        '--claims',
        censuses.first,
        for (final c in censuses.skip(1)) ...['--also-claims', c],
        ir,
        '//$dir:linked_block_check',
      ]);
      return (r.exitCode, '${r.stderr}${r.stdout}');
    }

    test('is checked against every linked bridge\'s census, in any order', () {
      for (final order in [
        [example, app],
        [app, example],
      ]) {
        final (code, out) = linked(order);
        expect(code, 0, reason: out);
        expect(out, contains('3 `no_block` root(s)'));
      }
    });

    test('is not passed over a census that names only one bridge', () {
      for (final (census, other) in [
        (example, 'linked_app'),
        (app, 'linked_example'),
      ]) {
        final (code, err) = linked([census]);
        expect(code, isNot(0), reason: err);
        expect(err, contains("no linked bridge's census claims"));
        expect(err, contains('frustrate_check_block_${other}_'));
      }
    });

    test(
      'names a finding with its own bridge, and silences stay per bridge',
      () {
        // The same census without its silences: the finding comes back, in
        // the bridge that had them. `add` is in both bridges under one fn id,
        // and neither is a finding.
        final bare = Directory.systemTemp.createTempSync('linked_census');
        final stripped = File('${bare.path}/app.claims')
          ..writeAsStringSync(
            File(app)
                .readAsLinesSync()
                .where((l) => !l.startsWith('allow '))
                .join('\n'),
          );
        final (code, err) = linked([example, stripped.path]);
        expect(code, isNot(0), reason: err);
        expect(
          err,
          contains(
            'linked_app: read\n      reaches pthread_mutex_lock — waits',
          ),
        );
        expect(err, isNot(contains('linked_example')));
        expect(err, isNot(contains('linked_app: add')));
      },
    );
  });
}
