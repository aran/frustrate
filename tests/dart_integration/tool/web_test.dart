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

import 'dart:convert';
import 'dart:io';

/// The package the Bazel-built fixture files are served from, at
/// `packages/web_test_fixtures/<path>` (the `fixtures` paths of
/// `:web_test_inputs`). Tests fetch them by URLs relative to their page.
const _fixturePackage = 'web_test_fixtures';

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

  // Under `bazel run`, Bazel sets BUILD_WORKSPACE_DIRECTORY to the workspace
  // root; the fixture build (cargo) and the browser run (pub `dart test`) both
  // operate on the real source tree, not the sandbox. Fall back to the cwd so
  // `dart run tool/web_test.dart` from the package still works.
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  final packageDir = workspace != null
      ? '$workspace/tests/dart_integration'
      : Directory.current.path;

  // 0. Resolve the Bazel-built fixtures (`:web_test_inputs`) in place: a
  //    package config outside the source tree adds their bindings packages to
  //    pub's, and a temporary directory of links serves their files. `dart
  //    run` has no runfiles and nothing builds these fixtures without Bazel,
  //    so that path leaves out the files that need them, by their
  //    `bazel_fixture` tag, and says so.
  Directory? scratch;
  var tests = const <String>[];
  if (workspace != null && inputs != null) {
    scratch = Directory.systemTemp.createTempSync('frustrate_web_test.');
    final declared = await _resolveInputs(
      inputs,
      scratch.path,
      packageDir,
      workspace,
      threaded,
    );
    if (declared == null) {
      scratch.deleteSync(recursive: true);
      exit(1);
    }
    tests = declared;
  } else {
    stdout.writeln(
      'not under `bazel run`: leaving out the bazel_fixture tests '
      '(compose_web_test.dart, compose_concurrent_init_web_test.dart, '
      'bindgen_web_test.dart, host_imports_web_test.dart), whose fixtures '
      'only Bazel builds',
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
  if (built != 0) {
    scratch?.deleteSync(recursive: true);
    exit(built);
  }

  // 2. Run the browser suite. Chrome flags (SharedArrayBuffer for the threaded
  //    fixture) live in dart_test.yaml, so this stays a plain invocation.
  const testArgs = ['-p', 'chrome', '-c', 'dart2wasm'];
  final int tested;
  if (scratch != null) {
    // The runner reads its isolate's package config, for the compiler and to
    // serve `packages/`, so it runs from source under the merged config.
    tested = await _run('dart', [
      '--packages=${scratch.path}/package_config.json',
      _testRunner('${scratch.path}/package_config.json'),
      ...testArgs,
      // The test files `:web_test_inputs` declares, unless the caller chose.
      if (passthrough.isEmpty) ...tests else ...passthrough,
    ], packageDir);
    scratch.deleteSync(recursive: true);
  } else {
    tested = await _run('dart', [
      'test',
      ...testArgs,
      '--exclude-tags',
      'bazel_fixture',
      ...passthrough,
    ], packageDir);
  }
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

/// Write `<scratch>/package_config.json`, pub's packages plus those of
/// [inputs], and link each fixture file under `<scratch>/fixtures/`. Returns
/// the test files [inputs] declares, or null on failure.
///
/// [inputs] is the runfiles path of the manifest `:web_test_inputs` writes;
/// `bazel run` starts this binary in its runfiles. The single-threaded
/// fixtures are runfiles. The threaded ones need the locally built +atomics
/// std, and as dependencies of this binary they would make every run need
/// that std, so under `--threaded` they are built here, after `bazel run` has
/// released the workspace, and linked from where Bazel reports them.
Future<List<String>?> _resolveInputs(
  String inputs,
  String scratch,
  String packageDir,
  String workspace,
  bool threaded,
) async {
  final manifest =
      jsonDecode(File(inputs).readAsStringSync()) as Map<String, Object?>;
  final runfiles = Directory.current.absolute.path;

  final files = {
    for (final MapEntry(:key, :value)
        in (manifest['fixtures'] as Map<String, Object?>).entries)
      key: '$runfiles/$value',
  };
  if (threaded) {
    final labels = (manifest['threaded'] as Map<String, Object?>)
        .cast<String, String>();
    if (await _run('bazel', ['build', ...labels.values], workspace) != 0) {
      return null;
    }
    for (final MapEntry(key: path, value: label) in labels.entries) {
      final built = await _outputOf(label, workspace);
      if (built == null) return null;
      files[path] = built;
    }
  }
  for (final MapEntry(key: path, value: file) in files.entries) {
    Link('$scratch/fixtures/$path')
        .createSync(File(file).resolveSymbolicLinksSync(), recursive: true);
  }

  // Pub's own resolution of this package, as `dart test` would get it.
  final got = await _run('dart', ['pub', 'get'], packageDir);
  if (got != 0) return null;
  final pubConfig = File('$packageDir/.dart_tool/package_config.json');
  final config =
      jsonDecode(pubConfig.readAsStringSync()) as Map<String, Object?>;
  final packages = [
    for (final package in (config['packages'] as List).cast<Map>())
      {
        ...package,
        // Relative to pub's config, which this one does not sit beside.
        'rootUri': pubConfig.uri
            .resolve(package['rootUri'] as String)
            .toString(),
      },
  ];
  final pubNames = {for (final p in packages) p['name']};
  for (final MapEntry(key: name, value: package)
      in (manifest['packages'] as Map<String, Object?>).entries) {
    if (pubNames.contains(name)) {
      stderr.writeln('web_test: pub already resolves package $name');
      return null;
    }
    final root = Directory('$runfiles/${(package as Map)['root']}')
        .resolveSymbolicLinksSync();
    packages.add({
      'name': name,
      'rootUri': Directory(root).uri.toString(),
      'packageUri': 'lib/',
      if ((package['languageVersion'] as String).isNotEmpty)
        'languageVersion': package['languageVersion'],
    });
  }
  packages.add({
    'name': _fixturePackage,
    'rootUri': Directory(scratch).uri.toString(),
    'packageUri': 'fixtures/',
  });
  File('$scratch/package_config.json').writeAsStringSync(
    const JsonEncoder.withIndent('  ')
        .convert({...config, 'packages': packages}),
  );
  stdout.writeln(
    'serving ${files.length} Bazel-built fixture files and '
    '${(manifest['packages'] as Map).length} bindings packages'
    '${threaded ? ', the threaded build where there is one' : ''}',
  );
  return (manifest['tests'] as List).cast<String>();
}

/// The one file Bazel builds for [label], or null if it is not one file.
Future<String?> _outputOf(String label, String workspace) async {
  final query = await Process.run('bazel', [
    'cquery',
    '--output=files',
    label,
  ], workingDirectory: workspace);
  final outputs = (query.stdout as String)
      .split('\n')
      .where((l) => l.isNotEmpty)
      .toList();
  if (query.exitCode != 0 || outputs.length != 1) {
    stderr
      ..writeln('web_test: expected one output of $label, got $outputs')
      ..write(query.stderr);
    return null;
  }
  return '$workspace/${outputs.single}';
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
