/// Every static analysis this repo can run, over every configuration it ships.
///
///     dart run tools/analyze.dart              the whole matrix
///     dart run tools/analyze.dart --list       what it would run, and why
///     dart run tools/analyze.dart --only miri  one step (substring match)
///     dart run tools/analyze.dart --help
///
/// **Why a matrix and not a command.** A lint result is per-target and
/// per-feature. Most of this runtime's hardest code — the threaded-wasm pool,
/// its allocator, the executor's registry lock — lives behind
/// `cfg(all(target_family = "wasm", feature = "wasm-threads"))` and therefore
/// does not exist in a plain `cargo clippy`. Running one configuration and
/// calling it analysed is how two of the three spin locks in this crate came to
/// carry an unsoundness under unwinding that no tool could have reported: they
/// compiled on no configuration anyone ever analysed.
///
/// **Why this had never run.** A single deny-by-default lint
/// (`not_unsafe_ptr_arg_deref` on `frustrate_buffer_free`) stopped clippy
/// *compiling* the runtime crate, on every target, so there was no baseline to
/// gate on and 170 warnings behind it. Fixed in the commit that introduced this
/// file's first green run.
///
/// **`-D warnings` is the point.** Clean-today is worth little; the flag is
/// what makes it stay clean. A new warning fails the run.
library;

import 'dart:io';

/// The pinned nightly, read rather than written down twice.
///
/// The wasm legs need nightly (`feature(stdarch_wasm_atomic_wait)`), and they
/// need *this* nightly: clippy's lint set drifts between nightlies, so gating
/// on whatever `+nightly` happens to resolve to would make the gate flake as
/// the ambient toolchain moves. Everything else in this repo pins; so does
/// this.
String get _pin =>
    File('toolchain/custom_std/nightly-pin.txt').readAsStringSync().trim();

/// One analysis. [why] is printed by `--list` and is the reason the step earns
/// its place — a step nobody can justify is a step that gets skipped when it
/// goes red.
class _Step {
  final String name;
  final String why;
  final List<String> command;
  final Map<String, String> env;
  const _Step(this.name, this.why, this.command, {this.env = const {}});
}

List<_Step> _matrix(String pin) => [
  // ---------------------------------------------------------- clippy --
  _Step(
    'clippy host',
    'The whole workspace, tests included. The only leg that sees codegen, '
        'the macros and the test fixtures at all.',
    ['cargo', 'clippy', '--workspace', '--all-targets', '--', '-D', 'warnings'],
  ),
  _Step(
    'clippy wasm',
    'Single-threaded web: the default browser build. Compiles the wasm '
        'exports (frustrate_alloc, frustrate_web_init) that the host leg '
        'cfgs away entirely.',
    [
      'cargo',
      '+$pin',
      'clippy',
      '-p',
      'frustrate',
      '--target',
      'wasm32-unknown-unknown',
      '--',
      '-D',
      'warnings',
    ],
  ),
  _Step(
    'clippy wasm-threads',
    'Threaded web. The only leg that compiles the worker pool and the '
        '`+atomics` arms — the executor\'s '
        'spin lock, and everywhere else std\'s futex backends are live. '
        '`--features wasm-threads` alone does not select those: the '
        'executor keys its lock on `target_feature = "atomics"`, not on '
        'the feature, because atomics is what makes `memory.atomic.wait32` '
        'exist (executor.rs, `Lock`). Measured, not assumed — a '
        '`#[cfg(all(target_family = "wasm", target_feature = "atomics"))] '
        'compile_error!` in executor.rs is silent under the feature alone '
        'and fires with the flags below. `-Zbuild-std` so the std these '
        'arms are checked against is one built for atomics, as it is on '
        'threaded web, rather than the shipped no-threads rlib.',
    [
      'cargo',
      '+$pin',
      'clippy',
      '-p',
      'frustrate',
      '--target',
      'wasm32-unknown-unknown',
      '--features',
      'wasm-threads',
      '-Zbuild-std=std,panic_abort',
      '--',
      '-D',
      'warnings',
    ],
    // Its own directory: these rustflags differ from every other leg's, so
    // a shared one would rebuild the world in both directions on every
    // run. `\x1f`-separated because plain RUSTFLAGS is word-split, which
    // silently mangles multi-word flags — see tools/check_block.dart.
    env: {
      'CARGO_TARGET_DIR': 'target/analyze-wasm-threads',
      'CARGO_ENCODED_RUSTFLAGS':
          '-C\x1ftarget-feature=+atomics,+bulk-memory,+mutable-globals',
    },
  ),

  // ------------------------------------------------------------ miri --
  // Two passes rather than one, because leak checking and coverage pull
  // in opposite directions here — see the comment on the second.
  _Step(
    'miri (leak-checked)',
    'UB *and* leak detection over every runtime test except the four '
        'modules named in the skip flags, which cannot be leak-checked. '
        'Those four are the only reason the pass below has to give leak '
        'detection up. Deliberately no test count here: the last one went '
        'stale the moment a module was added, which is how this leg came '
        'to be failing.',
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
    env: {'CARGO_TARGET_DIR': 'target/analyze-miri'},
  ),
  _Step(
    'miri (whole crate)',
    'UB over every runtime test, including the four modules the pass '
        'above skips — so nothing loses UB coverage, only leak coverage. '
        'Leaks are ignored for two distinct reasons. `pool::`, `actor::` '
        'and `executor::` start threads that outlive main by design — the '
        'native pool loops on a receiver whose sender lives in a static '
        'that never drops — and no test teardown can reap that. '
        '`runtime::` instead *asserts* leaks: its fixture models a real '
        'async runtime (a `Box::leak`ed `&\'static` reactor, a detached '
        'timer thread), and two of its tests pin the contract that an '
        'un-run poll must be forgotten rather than dropped, because '
        'dropping it aborts the process inside a destructor. A leak '
        'checker cannot tell an asserted leak from an accident. Measured '
        'per module rather than assumed: every other module is leak-clean, '
        'which is why the pass above covers most of the crate rather than '
        'the 4 tests it started with.',
    ['cargo', '+$pin', 'miri', 'test', '-p', 'frustrate', '--lib'],
    env: {
      'CARGO_TARGET_DIR': 'target/analyze-miri',
      'MIRIFLAGS': '-Zmiri-ignore-leaks',
    },
  ),
  _Step(
    'miri (hostile requests)',
    'The runtime legs above test the runtime\'s own code. This one tests '
        'the *generated* code, by driving requests derived from the '
        'interface at the real dispatch entry — the same `#[no_mangle]` '
        'the transport calls. One object handed to two positions that can '
        'each accept it is the attack that found the confined aliasing '
        'defect; trailing bytes, a truncated request, an impossible '
        'length prefix, an unknown impl tag and an unknown fn_id are the '
        'refusals the codec and the glue claim to make. Default flags on '
        'purpose: Stacked Borrows is what reports two references over one '
        'Box, and `-Zmiri-tree-borrows` does not. Leak checking on, so an '
        'unreclaimed handle is a finding too. Sync arms only — see the '
        'header of tests/test_api/src/hostile.rs for what that leaves out '
        'and why.',
    ['cargo', '+$pin', 'miri', 'test', '-p', 'test_api', '--lib'],
    env: {'CARGO_TARGET_DIR': 'target/analyze-miri'},
  ),

  // ----------------------------------------------------------- shape --
  _Step(
    'shape coverage',
    'Every member shape the checker accepts must be instantiated by this '
        'repo\'s own bridge sources. Codegen emits per shape — dispatch '
        'arm, opaque model, receiver and parameter borrows, `dyn`-ness, '
        'the declared flags — so a shape nobody writes is a shape nothing '
        'compiles and nothing runs, which is how both of this week\'s '
        'defects arrived. The checker is the oracle: each cell is '
        'synthesized, parsed and checked, and "accepted" is the whole '
        'definition of legal. A red run *is* the report: every line is a '
        'work item, and some of them cannot be answered with a fixture at '
        'all — an `async fn` on a locked type does not compile, because '
        'the generated future holds an `RwLock` guard across the body\'s '
        'awaits and the executor requires `Send`. Those are answered by '
        'the checker refusing the shape instead.',
    ['cargo', 'run', '--quiet', '-p', 'shape-coverage'],
  ),

  // ------------------------------------------------------- artifact --
  _Step(
    'no-park (single-threaded web)',
    'The one hazard with no runtime seam. Every other way of waiting on '
        'single-threaded wasm refuses loudly — std\'s `no_threads` mutex '
        'and condvar panic, `sleep` panics, `spawn` returns an error — and '
        'the panic hook makes each attributable. `thread::park` alone is a '
        'no-op there, so a caller polling around it spins the browser\'s '
        'only thread forever, unobservably. Source lints cannot see it '
        '(it arrives through a dependency), so this reads the artifact.',
    ['dart', 'run', 'tools/check_no_park.dart'],
  ),
  _Step(
    'block-check (no_block claims)',
    'Settles every `#[bridge(no_block)]` claim that needs an artifact — '
        'the ones placement does not already settle. Codegen checks the '
        'declared half (FR0048, FR0049) and stops at the crate boundary, '
        'so it cannot see a `Mutex` three dependencies down that becomes '
        '`memory.atomic.wait32` on threaded wasm. Also the only thing '
        'anywhere that builds the `--cfg frustrate_block_check` '
        'configuration — `bazel test //...` never does, because the check '
        'targets are `manual` — so it is the only place a regression to '
        'the executor\'s `+atomics` lock arm can surface at all.',
    ['dart', 'run', 'tools/check_block.dart'],
  ),

  // ------------------------------------------------------------ dart --
  _Step('dart analyze runtime/dart', 'The Dart half of the runtime.', [
    'dart',
    'analyze',
    'runtime/dart',
  ]),
  _Step(
    'dart analyze tools',
    'The shared benchmark harness, and — since this leg was widened from '
        '`tools/bench_harness` — the gate scripts themselves. Every '
        'artifact gate in the matrix above is a Dart program in this '
        'directory, and none of them was analysed by the matrix they '
        'belong to.',
    ['dart', 'analyze', 'tools'],
  ),
  _Step(
    'dart analyze tests/dart_integration',
    'The integration suite. Was excluded until its browser-only files '
        'could name `FrustrateWeb` on any platform — see '
        'package:frustrate/frustrate_web.dart.',
    ['dart', 'analyze', 'tests/dart_integration'],
  ),
];

const _usage = '''
Every static analysis this repo can run, over every configuration it ships.

  dart run tools/analyze.dart              the whole matrix
  dart run tools/analyze.dart --list       what it would run, and why
  dart run tools/analyze.dart --only miri  only steps whose name contains this
  dart run tools/analyze.dart --help

Exit status is 0 only if every step passed. Clippy legs run with
`-D warnings`, so a new warning is a failure, not a note.
''';

Future<void> main(List<String> args) async {
  if (args.contains('--help') || args.contains('-h')) {
    stdout.write(_usage);
    return;
  }
  Directory.current = File.fromUri(Platform.script).parent.parent;

  String? only;
  for (var i = 0; i < args.length; i++) {
    if (args[i] == '--only') {
      if (i + 1 >= args.length) {
        stderr.writeln('analyze: --only needs a value\n');
        stderr.write(_usage);
        exit(2);
      }
      only = args[++i];
    } else if (args[i] != '--list') {
      stderr.writeln('analyze: unrecognised ${args[i]}\n');
      stderr.write(_usage);
      exit(2);
    }
  }

  final pin = _pin;
  var steps = _matrix(pin);
  if (only != null) {
    steps = steps.where((s) => s.name.contains(only!)).toList();
    if (steps.isEmpty) {
      stderr.writeln('analyze: no step matches "$only"');
      exit(2);
    }
  }

  if (args.contains('--list')) {
    for (final s in steps) {
      stdout.writeln('${s.name}\n    ${s.why}\n    ${s.command.join(' ')}\n');
    }
    return;
  }

  stdout.writeln(
    '==> stable ${_version(['cargo', '--version'])}   '
    'nightly pin $pin   dart ${_version(['dart', '--version'])}',
  );
  _requireToolchain(pin);

  final failed = <String>[];
  for (final s in steps) {
    stdout.write('==> ${s.name} ... ');
    final sw = Stopwatch()..start();
    final r = await Process.run(
      s.command.first,
      s.command.skip(1).toList(),
      environment: s.env,
    );
    sw.stop();
    if (r.exitCode == 0) {
      stdout.writeln('ok (${sw.elapsed.inSeconds}s)');
      continue;
    }
    stdout.writeln('FAILED (${sw.elapsed.inSeconds}s)');
    stdout.writeln('    ${s.command.join(' ')}');
    stdout.write(r.stdout);
    stderr.write(r.stderr);
    failed.add(s.name);
  }

  stdout.writeln();
  if (failed.isEmpty) {
    stdout.writeln('==> all ${steps.length} clean.');
    return;
  }
  stderr.writeln(
    '==> ${failed.length} of ${steps.length} FAILED: '
    '${failed.join(', ')}',
  );
  exit(1);
}

/// Fail with the exact command to run, rather than silently falling back to a
/// floating `+nightly` — a gate on an unpinned toolchain is a gate that flakes.
void _requireToolchain(String pin) {
  final installed = Process.runSync('rustup', [
    'component',
    'list',
    '--toolchain',
    pin,
    '--installed',
  ]).stdout.toString();
  // `rust-src` is what `-Zbuild-std` compiles std from; without it the wasm
  // legs fail deep inside cargo with a message about a missing
  // `library/std/Cargo.toml`, which reads like a toolchain bug rather than a
  // missing component.
  final missing = [
    'clippy',
    'miri',
    'rust-src',
  ].where((c) => !installed.contains(c));
  final targets = Process.runSync('rustup', [
    'target',
    'list',
    '--toolchain',
    pin,
    '--installed',
  ]).stdout.toString();

  final fixes = <String>[
    if (missing.isNotEmpty)
      'rustup component add --toolchain $pin ${missing.join(' ')}',
    if (!targets.contains('wasm32-unknown-unknown'))
      'rustup target add wasm32-unknown-unknown --toolchain $pin',
  ];
  if (fixes.isEmpty) return;
  stderr.writeln(
    'analyze: the pinned toolchain is missing what the wasm and '
    'miri legs need.\nRun:\n',
  );
  for (final f in fixes) {
    stderr.writeln('  $f');
  }
  exit(2);
}

String _version(List<String> cmd) {
  try {
    final r = Process.runSync(cmd.first, cmd.skip(1).toList());
    final out = '${r.stdout}${r.stderr}'.trim();
    final m = RegExp(r'(\d+\.\d+\.\d+)').firstMatch(out);
    return m?.group(1) ?? '?';
  } on ProcessException {
    return '?';
  }
}
