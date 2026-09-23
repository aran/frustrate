/// The release (`-O2`) web benchmark — the vehicle the browser suite cannot
/// be. A `dart_binary`, run one config per invocation:
///
///     bazel run //tests/dart_integration:web_bench                 # single-threaded
///     bazel run //tests/dart_integration:web_bench -- --threaded   # threaded wasm
///     (cd tests/dart_integration && dart run tool/web_bench.dart)
///
/// **Why this driver runs `dart compile wasm` itself**, rather than depending on
/// a Bazel-built module: exactly the reason `native_bench.dart` runs `cargo
/// build --release` itself (see its header, and the `native_bench` comment in
/// BUILD.bazel). A Bazel target's optimization level silently follows the
/// invocation's `--compilation_mode`, whose default `fastbuild` is
/// `-Copt-level=0` on the Rust side — and on the Dart side the pub `test`
/// runner is worse still, hardcoding `-O0 --enable-asserts` for dart2wasm with
/// no way to override it (`tool/web_test.dart` documents this; it is why a
/// release web timing is unobtainable from `bazel run :web_test` under any
/// flag). Owning both compiler invocations is the only way the reported number
/// can name its own build. The Rust half is always built `--release` here for
/// the same reason: a release benchmark against a debug bridge is not a
/// benchmark of anything.
///
/// **It asserts nothing about absolute time**, here or in `bazel test`. Adding
/// an absolute-timing bar to CI is a separate decision on a separate day; this
/// driver exists so that decision can be made against real numbers.
///
/// Real COOP/COEP is a feature, not a detail. The in-process server sends
/// `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy:
/// credentialless` on every response, so the page is genuinely
/// cross-origin-isolated: the threaded fixture gets real `SharedArrayBuffer`
/// without the `--enable-features=SharedArrayBuffer` flag `dart_test.yaml` has
/// to pass, and `performance.now()` keeps its 5 us resolution instead of being
/// coarsened to 100 us. The page hard-fails if `crossOriginIsolated` is false.
///
/// Provenance is the point. Every artifact is hashed by this driver at build
/// time, the server serves an in-memory SNAPSHOT of `build/` taken immediately
/// after the build (so a concurrent `web_test` run staging a debug fixture into
/// the same directory cannot swap a module out from under the page), and the
/// page independently verifies that what it loaded matches what was built —
/// `asyncIsParallel` against the declared flavour, asserts against the declared
/// optimization level. Any disagreement is a nonzero exit with no table.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';

// -------------------------------------------------------------- constants --

/// The dart2wasm optimization level. `dart compile wasm` defaults to `-O1`;
/// asserts are off at every level unless `--enable-asserts` is passed, so the
/// page's assert probe cannot tell -O2 from -O1. That gap is closed procedurally
/// instead: stale outputs are deleted before compiling, a nonzero compiler exit
/// aborts, and the exact argv plus the output sha go into the provenance block.
const String _optLevel = '-O2';

/// How long to wait for the page to POST its results.
const Duration _browserTimeout = Duration(minutes: 20);

// ------------------------------------------------------------------- main --

Future<void> main(List<String> args) async {
  final threaded = args.contains('--threaded');
  final unknown = args.where((a) => a != '--threaded').toList();
  if (unknown.isNotEmpty) {
    stderr.writeln(
      'web_bench: unrecognized argument(s): ${unknown.join(' ')}\n'
      'Usage: web_bench [--threaded]   (one config per invocation)',
    );
    exit(2);
  }

  // Under `bazel run`, Bazel sets BUILD_WORKSPACE_DIRECTORY to the workspace
  // root; the fixture build (cargo) and the Dart compile both operate on the
  // real source tree, not the sandbox. Fall back to the cwd so `dart run
  // tool/web_bench.dart` from the package still works. Same shape as
  // tool/web_test.dart.
  final workspaceEnv = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  final packageDir = workspaceEnv != null
      ? '$workspaceEnv/tests/dart_integration'
      : Directory.current.path;
  final workspace = Directory('$packageDir/../..').absolute
      .resolveSymbolicLinksSync();

  // Verify the guess, loudly. A wrong root would benchmark some other checkout.
  final manifest = File('$workspace/Cargo.toml');
  if (!manifest.existsSync() ||
      !manifest.readAsStringSync().contains('[workspace]')) {
    stderr.writeln(
      'web_bench: $workspace is not the frustrate workspace '
      '(no Cargo.toml with [workspace]).\n'
      'Run it as `bazel run //tests/dart_integration:web_bench`, or as '
      '`dart run tool/web_bench.dart` from tests/dart_integration.',
    );
    exit(2);
  }

  final buildDir = Directory('$packageDir/build');
  final flavour = threaded ? 'threaded' : 'single-threaded';
  stderr.writeln('web_bench: config=$flavour  package=$packageDir');

  // 1. Package resolution. `dart compile wasm` needs .dart_tool/package_config
  //    and will not create it; and WHICH `frustrate` package resolves here
  //    decides which runtime_web.dart is under measurement, so the resolved
  //    root goes into the provenance block.
  if (await _run('dart', ['pub', 'get'], packageDir, 'pub get') != 0) exit(1);
  final frustrateRoot = _resolvedPackageRoot(packageDir, 'frustrate');

  // 2. The Rust half. Always --release: see the header. This reuses the single
  //    source of the build logic rather than duplicating the cargo invocation.
  //    Its own phase log, because a concurrent cargo in the same workspace
  //    serializes on the target lock and this can sit for minutes.
  final fixtureSw = Stopwatch()..start();
  final built = await _run(
    'dart',
    [
      'run',
      'tool/build_web_fixture.dart',
      '--release',
      if (threaded) '--threaded',
    ],
    packageDir,
    'build_web_fixture (rust, release, $flavour)',
  );
  if (built != 0) exit(built);
  stderr.writeln('web_bench: fixture built in ${fixtureSw.elapsed.inSeconds}s');

  // 3. The Dart half, at -O2. Delete first: a failed compile must not leave a
  //    servable stale artifact for the page to benchmark.
  for (final ext in ['wasm', 'mjs', 'support.js', 'wasm.map']) {
    final f = File('${buildDir.path}/web_bench.$ext');
    if (f.existsSync()) f.deleteSync();
  }
  final compileArgv = [
    'compile',
    'wasm',
    _optLevel,
    '--strip-wasm',
    'bench/web_bench_main.dart',
    '-o',
    'build/web_bench.wasm',
  ];
  if (await _run('dart', compileArgv, packageDir, 'dart compile wasm') != 0) {
    exit(1);
  }
  final dartModule = File('${buildDir.path}/web_bench.wasm');
  final fixture = File('${buildDir.path}/test_api.wasm');
  for (final f in [dartModule, fixture]) {
    if (!f.existsSync()) {
      stderr.writeln('web_bench: expected ${f.path} to exist after the build.');
      exit(1);
    }
  }

  // 4. Copy the page in, then snapshot build/ into memory. Everything served
  //    after this point is immutable for the life of the run.
  for (final name in ['index.html', 'bootstrap.js']) {
    File('$packageDir/bench/$name').copySync('${buildDir.path}/$name');
  }

  final provenance = <String, Object?>{
    'fixture_flavour': flavour,
    'rust_profile': 'release (stock: no lto/codegen-units tuning)',
    'fixture_bytes': fixture.lengthSync(),
    'fixture_sha256': _sha256(fixture.path),
    'dart_optimization_level': _optLevel,
    'dart_compile_argv': ['dart', ...compileArgv].join(' '),
    'dart_executable': _which('dart'),
    'dart_version': _dartVersion(),
    'dart_module_bytes': dartModule.lengthSync(),
    'dart_module_sha256': _sha256(dartModule.path),
    'frustrate_package_root': frustrateRoot,
    // The subject of the benchmark. Recorded because these files are being
    // changed concurrently; without them a number cannot be attributed to a
    // revision of the code it measures.
    'runtime_web_sha256': _sha256(
      '$workspace/runtime/dart/lib/src/runtime_web.dart',
    ),
    'binary_codec_sha256': _sha256(
      '$workspace/runtime/dart/lib/src/binary_codec.dart',
    ),
    'git_revision': _git(workspace, ['rev-parse', 'HEAD']),
    'git_status': _git(workspace, ['status', '--porcelain']),
    'git_dirty': (_git(workspace, ['status', '--porcelain']) ?? '').isNotEmpty,
    'workspace_resolved_via': workspaceEnv != null
        ? 'BUILD_WORKSPACE_DIRECTORY'
        : 'cwd',
    'os': Platform.operatingSystemVersion,
    'cpu': _sysctl('machdep.cpu.brand_string'),
    'cores': Platform.numberOfProcessors,
  };
  File('${buildDir.path}/provenance.json')
      .writeAsStringSync(jsonEncode(provenance));

  final snapshot = <String, List<int>>{};
  for (final e in buildDir.listSync()) {
    if (e is File) {
      snapshot['/${e.uri.pathSegments.last}'] = e.readAsBytesSync();
    }
  }
  stderr.writeln(
    'web_bench: serving ${snapshot.length} files from an '
    'in-memory snapshot of build/ '
    '(fixture ${provenance['fixture_bytes']} B sha '
    '${(provenance['fixture_sha256'] as String?)?.substring(0, 12)}, '
    'dart module ${provenance['dart_module_bytes']} B sha '
    '${(provenance['dart_module_sha256'] as String?)?.substring(0, 12)})',
  );

  // 5. Serve, run, collect.
  final results = Completer<Map<String, Object?>>();
  final logs = <String>[];
  var lastLogAt = DateTime.now();

  final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
  unawaited(
    _serve(server, snapshot, (path, body) {
      switch (path) {
        case '/log':
          logs.add(body);
          lastLogAt = DateTime.now();
          stderr.writeln('  page: $body');
        case '/fatal':
          if (!results.isCompleted) {
            results.completeError(StateError(body));
          }
        case '/results':
          if (!results.isCompleted) {
            results.complete(jsonDecode(body) as Map<String, Object?>);
          }
      }
    }),
  );

  // 127.0.0.1 rather than a LAN address: loopback is a "potentially
  // trustworthy" origin, so COOP/COEP produce real cross-origin isolation over
  // plain http without a certificate.
  final url = 'http://127.0.0.1:${server.port}/index.html';
  final chrome = _chromeExecutable();
  final profile = Directory.systemTemp.createTempSync('frustrate_web_bench_');
  stderr.writeln('web_bench: $chrome --headless=new $url');

  final proc = await Process.start(chrome, [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    // Nothing here should be throttled for being offscreen; headless is
    // always "backgrounded" by these heuristics.
    '--disable-background-timer-throttling',
    '--disable-renderer-backgrounding',
    '--disable-backgrounding-occluded-windows',
    '--user-data-dir=${profile.path}',
    url,
  ]);
  // Drain both pipes: a full pipe buffer would deadlock the browser.
  final chromeErr = <String>[];
  proc.stdout.transform(utf8.decoder).listen((_) {});
  proc.stderr.transform(utf8.decoder).listen(chromeErr.add);

  Map<String, Object?>? payload;
  var failure = '';
  try {
    payload = await results.future.timeout(_browserTimeout);
  } on TimeoutException {
    final silent = DateTime.now().difference(lastLogAt);
    failure =
        'web_bench: timed out after ${_browserTimeout.inMinutes} min.\n'
        '  last page log: ${logs.isEmpty ? '(none — the page never started)' : logs.last}\n'
        '  silent for:    ${silent.inSeconds}s\n'
        'A hang inside a synchronous batch produces no log traffic at all, so '
        'a long silence with a plausible last line means a wedged cell, not a '
        'dead page.';
  } catch (e) {
    failure = 'web_bench: the page reported a fatal error.\n$e';
  }

  proc.kill();
  await server.close(force: true);
  try {
    profile.deleteSync(recursive: true);
  } on FileSystemException {
    // A browser still exiting can hold files here; the temp dir is disposable.
  }

  if (payload == null) {
    stderr.writeln(failure);
    if (chromeErr.isNotEmpty) {
      stderr.writeln('--- chrome stderr ---');
      stderr.write(chromeErr.join());
    }
    exit(1);
  }

  final meta = payload['meta'] as Map<String, Object?>;
  stdout.write(payload['table'] as String);
  stdout.writeln('RESULTS ${jsonEncode({...meta, 'cells': payload['cells']})}');
  await stdout.flush();
  exit(0);
}

// ------------------------------------------------------------------ serve --

/// Static server over an immutable in-memory snapshot, plus the three POST
/// endpoints the page reports through.
///
/// COOP/COEP go on EVERY response, not just the document: a subresource served
/// without them can strip the isolation the document asked for. `no-store`
/// keeps the browser from reusing anything across runs.
Future<void> _serve(
  HttpServer server,
  Map<String, List<int>> files,
  void Function(String path, String body) onPost,
) async {
  await for (final req in server) {
    final res = req.response;
    res.headers
      ..set('Cross-Origin-Opener-Policy', 'same-origin')
      ..set('Cross-Origin-Embedder-Policy', 'credentialless')
      ..set('Cross-Origin-Resource-Policy', 'same-origin')
      ..set('Cache-Control', 'no-store');

    if (req.method == 'POST') {
      onPost(req.uri.path, await utf8.decoder.bind(req).join());
      res.statusCode = HttpStatus.noContent;
      await res.close();
      continue;
    }

    final body = files[req.uri.path];
    if (body == null) {
      res.statusCode = HttpStatus.notFound;
      await res.close();
      continue;
    }
    res.headers.contentType = switch (req.uri.path.split('.').last) {
      'wasm' => ContentType('application', 'wasm'),
      'js' || 'mjs' => ContentType('text', 'javascript', charset: 'utf-8'),
      'html' => ContentType('text', 'html', charset: 'utf-8'),
      'json' => ContentType('application', 'json', charset: 'utf-8'),
      _ => ContentType.binary,
    };
    res.add(body);
    await res.close();
  }
}

// ------------------------------------------------------------------ tools --

Future<int> _run(
  String exe,
  List<String> args,
  String cwd,
  String label,
) async {
  stderr.writeln('\$ $exe ${args.join(' ')}  ($label, in $cwd)');
  final proc = await Process.start(
    exe,
    args,
    workingDirectory: cwd,
    mode: ProcessStartMode.inheritStdio,
  );
  final code = await proc.exitCode;
  if (code != 0) stderr.writeln('web_bench: $label failed (exit $code).');
  return code;
}

/// Where the pub solve actually put a package. Which `frustrate` resolves is
/// which `runtime_web.dart` is being benchmarked — in a git worktree that is a
/// real question, so it is recorded rather than assumed.
String? _resolvedPackageRoot(String packageDir, String name) {
  final f = File('$packageDir/.dart_tool/package_config.json');
  if (!f.existsSync()) return null;
  final cfg = jsonDecode(f.readAsStringSync()) as Map<String, Object?>;
  for (final p in (cfg['packages'] as List).cast<Map<String, Object?>>()) {
    if (p['name'] == name) {
      return Uri.parse('${f.uri}').resolve(p['rootUri'] as String).toString();
    }
  }
  return null;
}

/// The same resolution the pub `test` package uses for `-p chrome`
/// (test/lib/src/runner/browser/default_settings.dart): the CHROME_EXECUTABLE
/// override, else the platform default.
String _chromeExecutable() {
  final env = Platform.environment['CHROME_EXECUTABLE'];
  if (env != null && env.isNotEmpty) return env;
  if (Platform.isMacOS) {
    return '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
  }
  if (Platform.isWindows) return r'Google\Chrome\Application\chrome.exe';
  return 'google-chrome';
}

String? _which(String exe) => _capture('/usr/bin/which', [exe]);

String? _dartVersion() => _capture('dart', ['--version']);

String? _git(String workspace, List<String> args) =>
    _capture('git', ['-C', workspace, ...args]);

String? _sysctl(String key) => _capture('sysctl', ['-n', key]);

String? _capture(String exe, List<String> args) {
  try {
    final r = Process.runSync(exe, args);
    if (r.exitCode != 0) return null;
    final out = (r.stdout as String).trim();
    return out.isEmpty ? (r.stderr as String).trim() : out;
  } on ProcessException {
    return null;
  }
}

String? _sha256(String path) {
  if (!File(path).existsSync()) return null;
  for (final exe in ['shasum', 'sha256sum']) {
    try {
      final r = Process.runSync(exe, [
        if (exe == 'shasum') ...['-a', '256'],
        path,
      ]);
      if (r.exitCode == 0) {
        return (r.stdout as String).trim().split(RegExp(r'\s+')).first;
      }
    } on ProcessException {
      continue;
    }
  }
  return null;
}
