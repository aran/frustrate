/// Miri over the runtime's tests and the generated glue's hostile requests.
///
///     bazel run //tools:miri               every pass
///     bazel run //tools:miri -- hostile    the passes whose name contains this
///
/// **Why this is not a Bazel test.** Miri is `cargo +<pinned nightly> miri`:
/// an interpreter for rustc's MIR, shipped as a rustup component of a nightly
/// toolchain, with its own sysroot. No Bazel toolchain here provides it, so
/// this drives cargo. The pinned nightly is the one the wasm toolchains use
/// (`toolchain/custom_std/nightly-pin.txt`): Miri's findings drift between
/// nightlies, so gating on whatever `+nightly` resolves to would flake as the
/// ambient toolchain moves.
///
/// Every other static analysis is a Bazel target that `bazel test //...` runs;
/// see `tools/BUILD.bazel`.
///
/// Exit status is 0 only if every pass passed.
library;

import 'dart:io';

/// The pinned nightly, read rather than written down twice.
String _pin(String workspace) =>
    File('$workspace/toolchain/custom_std/nightly-pin.txt')
        .readAsStringSync()
        .trim();

/// One pass. [why] is the reason it earns its place, printed when it fails.
class _Pass {
  final String name;
  final String why;
  final List<String> command;
  final Map<String, String> env;
  const _Pass(this.name, this.why, this.command, {this.env = const {}});
}

List<_Pass> _passes(String pin) => [
  // Two passes over the runtime rather than one, because leak checking and
  // coverage pull in opposite directions here: see the second.
  _Pass(
    'leak-checked',
    'UB *and* leak detection over every runtime test except the four '
        'modules named in the skip flags, which cannot be leak-checked. '
        'Those four are the only reason the pass below has to give leak '
        'detection up.',
    [
      'cargo',
      '+$pin',
      'miri',
      'test',
      '-p',
      'frustrate',
      '--lib',
      '--',
      '--skip',
      'pool::',
      '--skip',
      'actor::',
      '--skip',
      'executor::',
      '--skip',
      'runtime::',
    ],
    env: {'CARGO_TARGET_DIR': 'target/miri'},
  ),
  _Pass(
    'whole crate',
    'UB over every runtime test, including the four modules the pass '
        'above skips, so nothing loses UB coverage, only leak coverage. '
        'Leaks are ignored for two distinct reasons. `pool::`, `actor::` '
        'and `executor::` start threads that outlive main by design: the '
        'native pool loops on a receiver whose sender lives in a static '
        'that never drops, and no test teardown can reap that. '
        '`runtime::` instead *asserts* leaks: its fixture models a real '
        'async runtime (a `Box::leak`ed `&\'static` reactor, a detached '
        'timer thread), and two of its tests pin the contract that an '
        'un-run poll must be forgotten rather than dropped, because '
        'dropping it aborts the process inside a destructor. A leak '
        'checker cannot tell an asserted leak from an accident.',
    ['cargo', '+$pin', 'miri', 'test', '-p', 'frustrate', '--lib'],
    env: {
      'CARGO_TARGET_DIR': 'target/miri',
      'MIRIFLAGS': '-Zmiri-ignore-leaks',
    },
  ),
  _Pass(
    'hostile requests',
    'The runtime passes test the runtime\'s own code. This one tests '
        'the *generated* code, by driving requests derived from the '
        'interface at the real dispatch entry, the same `#[no_mangle]` '
        'the transport calls. One object handed to two positions that can '
        'each accept it is the attack that found the confined aliasing '
        'defect; trailing bytes, a truncated request, an impossible '
        'length prefix, an unknown impl tag and an unknown fn_id are the '
        'refusals the codec and the glue claim to make. Default flags on '
        'purpose: Stacked Borrows is what reports two references over one '
        'Box, and `-Zmiri-tree-borrows` does not. Leak checking on, so an '
        'unreclaimed handle is a finding too. Sync arms only: see the '
        'header of tests/test_api/src/hostile.rs for what that leaves out '
        'and why.',
    ['cargo', '+$pin', 'miri', 'test', '-p', 'test_api', '--lib'],
    env: {'CARGO_TARGET_DIR': 'target/miri'},
  ),
];

Future<void> main(List<String> args) async {
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  if (workspace == null) {
    stderr.writeln('miri: run this as `bazel run //tools:miri`.');
    exit(2);
  }
  final pin = _pin(workspace);
  _requireToolchain(pin);

  var passes = _passes(pin);
  if (args.isNotEmpty) {
    passes = passes.where((p) => args.any(p.name.contains)).toList();
    if (passes.isEmpty) {
      stderr.writeln('miri: no pass matches ${args.join(', ')}');
      exit(2);
    }
  }

  final failed = <String>[];
  for (final p in passes) {
    stdout.write('==> miri ${p.name} ... ');
    final sw = Stopwatch()..start();
    final r = await Process.run(
      p.command.first,
      p.command.skip(1).toList(),
      environment: {
        ...p.env,
        'CARGO_TARGET_DIR': '$workspace/${p.env['CARGO_TARGET_DIR']}',
      },
      workingDirectory: workspace,
    );
    if (r.exitCode == 0) {
      stdout.writeln('ok (${sw.elapsed.inSeconds}s)');
      continue;
    }
    stdout.writeln('FAILED (${sw.elapsed.inSeconds}s)');
    stdout.writeln('    ${p.command.join(' ')}\n    ${p.why}');
    stdout.write(r.stdout);
    stderr.write(r.stderr);
    failed.add(p.name);
  }

  if (failed.isEmpty) {
    stdout.writeln('==> all ${passes.length} clean.');
    return;
  }
  stderr.writeln(
    '==> ${failed.length} of ${passes.length} FAILED: ${failed.join(', ')}',
  );
  exit(1);
}

/// Fail with the exact command to run, rather than silently falling back to a
/// floating `+nightly`: a gate on an unpinned toolchain is a gate that flakes.
void _requireToolchain(String pin) {
  final installed = Process.runSync('rustup', [
    'component',
    'list',
    '--toolchain',
    pin,
    '--installed',
  ]).stdout.toString();
  final missing = ['miri', 'rust-src'].where((c) => !installed.contains(c));
  if (missing.isEmpty) return;
  stderr.writeln(
    'miri: the pinned toolchain is missing ${missing.join(', ')}.\n'
    'Run:\n\n  rustup component add --toolchain $pin ${missing.join(' ')}',
  );
  exit(2);
}
