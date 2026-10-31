/// Runs the browser (dart2wasm + Chrome) side of the integration suite as one
/// command: `bazel run //tests/dart_integration:web_test [-- <args>]`.
///
/// Why a `dart_binary` and not a `dart_test`: rules_dart's `dart_test`
/// compiles the test's `main` to a **VM kernel** and runs it on the Dart VM —
/// it has no browser/dart2wasm launcher, and a browser run (system Chrome, the
/// pub `test` runner) is not hermetic under Bazel. So the web suite cannot be a
/// `bazel test` target today. Per the "if it must be a manual run, make it a
/// dart binary (never a shell script)" rule, this driver wraps the two-step
/// flow — build the wasm fixture, then run `dart test -p chrome -c dart2wasm` —
/// behind a single Bazel-native entry point, in Dart.
///
/// Usage:
///   bazel run //tests/dart_integration:web_test                 # single-threaded
///   bazel run //tests/dart_integration:web_test -- --threaded   # threaded wasm
///   bazel run //tests/dart_integration:web_test -- test/bridge_test.dart
///
/// `--threaded` selects the +atomics fixture, and `--facilities` the one built
/// against a std whose stubs were replaced by host-backed implementations.
/// Both need a locally staged std; see
/// toolchain/custom_std. They are mutually exclusive here — this driver stages
/// one fixture path. `--release` builds the *Rust* half with stock
/// `cargo --release` instead of debug. Any other args pass through to `dart
/// test` (e.g. a specific test file, `-n <name>`).
///
/// `--release` cannot make this a release *benchmark*: the pub `test` package
/// hardcodes `-O0 --enable-asserts` for dart2wasm, so the Dart half is always
/// unoptimized here. It makes the suite run against optimized Rust, nothing
/// more. Timings from this harness are not release numbers under any flag.
library;

import 'dart:io';

Future<void> main(List<String> args) async {
  final threaded = args.contains('--threaded');
  final facilities = args.contains('--facilities');
  final release = args.contains('--release');
  final passthrough = args
      .where(
        (a) => a != '--threaded' && a != '--release' && a != '--facilities',
      )
      .toList();

  // Under `bazel run`, Bazel sets BUILD_WORKSPACE_DIRECTORY to the workspace
  // root; the fixture build (cargo) and the browser run (pub `dart test`) both
  // operate on the real source tree, not the sandbox. Fall back to the cwd so
  // `dart run tool/web_test.dart` from the package still works.
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  final packageDir = workspace != null
      ? '$workspace/tests/dart_integration'
      : Directory.current.path;

  // 0. Stage the composed fixture (two bridges, one module) that Bazel built
  //    into this binary's runfiles: the module under build/, both bridges'
  //    bindings under lib/. `dart run` has no runfiles and nothing builds the
  //    fixture without Bazel, so that path leaves out the files that need it,
  //    by their `bazel_fixture` tag, and says so.
  final bazel = workspace != null;
  if (workspace != null) {
    _stageComposed(packageDir);
    if (!await _stageBindgen(packageDir, workspace, threaded)) exit(1);
  } else {
    stdout.writeln(
      'not under `bazel run`: leaving out the bazel_fixture tests '
      '(compose_web_test.dart, bindgen_web_test.dart), whose fixtures only '
      'Bazel builds',
    );
  }

  // 1. Build + stage the wasm fixture (reuses the single source of the build
  //    logic rather than duplicating the cargo invocation here).
  final built = await _run('dart', [
    'run',
    'tool/build_web_fixture.dart',
    if (threaded) '--threaded',
    if (facilities) '--facilities',
    if (release) '--release',
  ], packageDir);
  if (built != 0) exit(built);

  // 2. Run the browser suite. Chrome flags (SharedArrayBuffer for the threaded
  //    fixture) live in dart_test.yaml, so this stays a plain invocation.
  final tested = await _run('dart', [
    'test',
    '-p',
    'chrome',
    '-c',
    'dart2wasm',
    if (!bazel) ...['--exclude-tags', 'bazel_fixture'],
    ...passthrough,
  ], packageDir);
  exit(tested);
}

Future<int> _run(String exe, List<String> args, String cwd) async {
  stdout.writeln('\$ $exe ${args.join(' ')}  (in $cwd)');
  final proc = await Process.start(
    exe,
    args,
    workingDirectory: cwd,
    mode: ProcessStartMode.inheritStdio,
  );
  return proc.exitCode;
}

/// Copy the composed fixture out of the runfiles `bazel run` starts in.
void _stageComposed(String packageDir) {
  const module = 'tests/compose/composed.wasm';
  Directory('$packageDir/build').createSync(recursive: true);
  File(module).copySync('$packageDir/build/composed.wasm');
  for (final bridge in ['alpha', 'beta']) {
    final lib = Directory('tests/compose/$bridge/lib');
    for (final file in lib.listSync(recursive: true).whereType<File>()) {
      final relative = file.path.substring(lib.path.length + 1);
      final staged = File('$packageDir/lib/$relative')
        ..parent.createSync(recursive: true);
      file.copySync(staged.path);
    }
  }
  stdout.writeln('staged the composed fixture from $module');
}

/// Stage the wasm-bindgen fixture (//tests/bindgen): the module and its
/// wasm-bindgen JS under build/bindgen/, the bindings under lib/.
///
/// The single-threaded build is in this binary's runfiles. The threaded one
/// needs the locally built +atomics std, and as a dependency of this binary it
/// would make every run need that std, so under `--threaded` it is built here,
/// after `bazel run` has released the workspace.
Future<bool> _stageBindgen(
  String packageDir,
  String workspace,
  bool threaded,
) async {
  var from = 'tests/bindgen';
  if (threaded) {
    const target = '//tests/bindgen/threaded';
    final built = await _run('bazel', [
      'build',
      '$target:bindgen_api.wasm',
      '$target:bindgen_api_bg_js',
    ], workspace);
    if (built != 0) return false;
    // Only the crate under the rule is transitioned, so the rule's own
    // outputs are where `bazel-bin` points.
    from = '$workspace/bazel-bin/tests/bindgen/threaded';
  }
  final out = Directory('$packageDir/build/bindgen')
    ..createSync(recursive: true);
  for (final name in ['bindgen_api.wasm', 'bindgen_api_bg.js']) {
    File('$from/$name').copySync('${out.path}/$name');
  }
  final lib = Directory('tests/bindgen/lib');
  for (final file in lib.listSync(recursive: true).whereType<File>()) {
    final relative = file.path.substring(lib.path.length + 1);
    final staged = File('$packageDir/lib/$relative')
      ..parent.createSync(recursive: true);
    file.copySync(staged.path);
  }
  stdout.writeln('staged the wasm-bindgen fixture from $from');
  return true;
}
