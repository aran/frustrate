/// Runs the browser (dart2wasm + Chrome) side of the integration suite as one
/// command: `bazel run //tests/dart_integration:web_test [-- <args>]`.
///
/// Why a `dart_binary` and not a `dart_test`: rules_dart's `dart_test`
/// compiles the test's `main` to a **VM kernel** and runs it on the Dart VM —
/// it has no browser/dart2wasm launcher, and a browser run (system Chrome, the
/// pub `test` runner) is not hermetic under Bazel. So the web suite cannot be a
/// `bazel test` target today. Per the "if it must be a manual run, make it a
/// dart binary (never a shell script)" rule, this driver wraps the flow —
/// serve the Bazel-built fixtures, then run `dart test -p chrome -c dart2wasm`
/// — behind a single Bazel-native entry point, in Dart.
///
/// Usage:
///   bazel run //tests/dart_integration:web_test                 # single-threaded
///   bazel run //tests/dart_integration:web_test -- --threaded   # threaded wasm
///   bazel run //tests/dart_integration:web_test -- test/bridge_test.dart
///
/// `--threaded` selects the +atomics fixtures, and `--facilities` the one built
/// against a std whose stubs were replaced by host-backed implementations.
/// Both need a locally staged std; see toolchain/custom_std. They are mutually
/// exclusive. `--release` rebuilds the *Rust* half of the bridge fixture with
/// `-c opt` instead of the invocation's default. Any other args pass through
/// to `dart test` (e.g. a specific test file, `-n <name>`).
///
/// `--release` cannot make this a release *benchmark*: the pub `test` package
/// hardcodes `-O0 --enable-asserts` for dart2wasm, so the Dart half is always
/// unoptimized here. It makes the suite run against optimized Rust, nothing
/// more. Timings from this harness are not release numbers under any flag.
library;

import 'dart:convert';
import 'dart:io';

import 'web_inputs.dart';

Future<void> main(List<String> args) async {
  final threaded = args.contains('--threaded');
  final facilities = args.contains('--facilities');
  final release = args.contains('--release');
  final inputs = args
      .where((a) => a.startsWith('--inputs='))
      .map((a) => a.substring('--inputs='.length))
      .firstOrNull;
  final passthrough = args
      .where(
        (a) =>
            a != '--threaded' &&
            a != '--release' &&
            a != '--facilities' &&
            !a.startsWith('--inputs='),
      )
      .toList();

  if (threaded && facilities) {
    // Not a fundamental conflict -- an `atomics,clock,...` flavour is a
    // legitimate thing to build -- but the fixtures name one std each. Refused
    // rather than silently preferring one, because the suite reads the *value*
    // of the std probes to decide what it is running against.
    stderr.writeln(
      'web_test: --threaded and --facilities together are not supported. '
      'Run one flavour at a time.',
    );
    exit(2);
  }

  // Under `bazel run`, Bazel sets BUILD_WORKSPACE_DIRECTORY to the workspace
  // root; the browser run (pub `dart test`) operates on the real source tree,
  // not the sandbox.
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  if (workspace == null || inputs == null) {
    stderr.writeln(
      'web_test: every fixture is built by Bazel, so run this as '
      '`bazel run //tests/dart_integration:web_test`.',
    );
    exit(2);
  }
  final packageDir = '$workspace/tests/dart_integration';

  // `-c dart2js` in the passthrough runs the suite on the JS backend, with
  // that backend's variant of the per-compiler packages.
  final cFlag = passthrough.indexOf('-c');
  final testCompiler = cFlag >= 0 && cFlag + 1 < passthrough.length
      ? passthrough[cFlag + 1]
      : 'dart2wasm';

  // 0. Resolve the Bazel-built fixtures (`:web_test_inputs`) in place: a
  //    package config outside the source tree adds their bindings packages to
  //    pub's, and a temporary directory of links serves their files.
  final scratch = Directory.systemTemp.createTempSync('frustrate_web_test.');
  final tests = await resolveInputs(
    inputs,
    scratch.path,
    packageDir,
    workspace,
    threaded: threaded,
    facilities: facilities,
    release: release,
    compiler: webCompilerKey(testCompiler),
  );
  if (tests == null) {
    scratch.deleteSync(recursive: true);
    exit(1);
  }

  // 1. Run the browser suite. Chrome flags (SharedArrayBuffer for the threaded
  //    fixture) live in dart_test.yaml, so this stays a plain invocation. The
  //    runner reads its isolate's package config, for the compiler and to
  //    serve `packages/`, so it runs from source under the merged config.
  final testArgs = [
    '-p',
    'chrome',
    if (cFlag < 0) ...['-c', testCompiler],
  ];
  final tested = await run('dart', [
    '--packages=${scratch.path}/package_config.json',
    _testRunner('${scratch.path}/package_config.json'),
    ...testArgs,
    // The test files `:web_test_inputs` declares, unless the caller chose.
    if (passthrough.isEmpty) ...tests else ...passthrough,
  ], packageDir);
  scratch.deleteSync(recursive: true);
  exit(tested);
}

/// The pub `test` runner's entry point, from [packageConfig].
String _testRunner(String packageConfig) {
  final config = jsonDecode(
    File(packageConfig).readAsStringSync(),
  ) as Map<String, Object?>;
  final test = (config['packages'] as List).cast<Map>().firstWhere(
    (p) => p['name'] == 'test',
  );
  final root = test['rootUri'] as String;
  return Uri.parse(root.endsWith('/') ? root : '$root/')
      .resolve('bin/test.dart')
      .toFilePath();
}
