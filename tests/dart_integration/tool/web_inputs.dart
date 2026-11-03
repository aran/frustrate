/// The Bazel-built inputs of a browser run (`:web_test_inputs`), resolved in
/// place, for `web_test.dart` and `web_bench.dart`.
library;

import 'dart:convert';
import 'dart:io';

/// The package the Bazel-built fixture files are served from, at
/// `packages/web_test_fixtures/<path>` (the `fixtures` paths of
/// `:web_test_inputs`).
const fixturePackage = 'web_test_fixtures';

Future<int> run(String exe, List<String> args, String cwd) async {
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
/// `bazel run` starts this binary in its runfiles. The default fixtures are
/// runfiles. The `--threaded` and `--facilities` ones need a locally built std,
/// and as dependencies of this binary they would make every run need it, and
/// `--release` ones are a different configuration of the same targets, so all
/// of them are built here, after `bazel run` has released the workspace, and
/// linked from where Bazel reports them.
Future<List<String>?> resolveInputs(
  String inputs,
  String scratch,
  String packageDir,
  String workspace, {
  required bool threaded,
  required bool facilities,
  required bool release,
}) async {
  final manifest =
      jsonDecode(File(inputs).readAsStringSync()) as Map<String, Object?>;
  final runfiles = Directory.current.absolute.path;

  final files = {
    for (final MapEntry(:key, :value)
        in (manifest['fixtures'] as Map<String, Object?>).entries)
      key: '$runfiles/$value',
  };

  // The fixtures this run rebuilds, by path: the target the flavour names,
  // else the fixture's own, built `-c opt` where `--release` asks.
  final variant = threaded
      ? manifest['threaded']
      : facilities
      ? manifest['facilities']
      : const <String, Object?>{};
  final labels = (manifest['labels'] as Map<String, Object?>)
      .cast<String, String>();
  final optimized = release
      ? (manifest['release'] as List).cast<String>().toSet()
      : const <String>{};
  final rebuilt = {...(variant as Map<String, Object?>).cast<String, String>()};
  for (final path in optimized) {
    rebuilt[path] ??= labels[path]!;
  }
  for (final optimize in [false, true]) {
    final group = {
      for (final MapEntry(:key, :value) in rebuilt.entries)
        if (optimized.contains(key) == optimize) key: value,
    };
    if (group.isEmpty) continue;
    final flags = [
      if (optimize) ...['-c', 'opt'],
    ];
    if (await run('bazel', ['build', ...flags, ...group.values], workspace) !=
        0) {
      return null;
    }
    for (final MapEntry(key: path, value: label) in group.entries) {
      final built = await outputOf(label, flags, workspace);
      if (built == null) return null;
      files[path] = built;
    }
  }
  for (final MapEntry(key: path, value: file) in files.entries) {
    Link('$scratch/fixtures/$path')
        .createSync(File(file).resolveSymbolicLinksSync(), recursive: true);
  }

  // Pub's own resolution of this package, as `dart test` would get it.
  final got = await run('dart', ['pub', 'get'], packageDir);
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
  // The suite's own package is also the name its bridge's bindings are
  // generated under, so pub resolves it to the source tree, which holds
  // none of them. The Bazel-built bindings take its place.
  final self = (config['packages'] as List).cast<Map>().firstWhere(
    (p) => p['rootUri'] == '../',
  )['name'];
  for (final MapEntry(key: name, value: package)
      in (manifest['packages'] as Map<String, Object?>).entries) {
    if (name != self && packages.any((p) => p['name'] == name)) {
      stderr.writeln('web_test_inputs: pub already resolves package $name');
      return null;
    }
    packages.removeWhere((p) => p['name'] == name);
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
    'name': fixturePackage,
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
    '${rebuilt.isEmpty ? '' : ', rebuilt: ${rebuilt.keys.join(', ')}'}',
  );
  return (manifest['tests'] as List).cast<String>();
}

/// The one file Bazel builds for [label] under [flags], or null if it is not
/// one file.
Future<String?> outputOf(
  String label,
  List<String> flags,
  String workspace,
) async {
  final query = await Process.run('bazel', [
    'cquery',
    ...flags,
    '--output=files',
    label,
  ], workingDirectory: workspace);
  final outputs = (query.stdout as String)
      .split('\n')
      .where((l) => l.isNotEmpty)
      .toList();
  if (query.exitCode != 0 || outputs.length != 1) {
    stderr
      ..writeln('web_test_inputs: expected one output of $label, got $outputs')
      ..write(query.stderr);
    return null;
  }
  return '$workspace/${outputs.single}';
}
