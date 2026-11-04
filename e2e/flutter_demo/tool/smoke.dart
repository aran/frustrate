/// Runs the demo gallery on one device and checks that the Rust bridge answered:
///
///   bazel run //:smoke -- -t //:app_ios -d ios-simulator
///   bazel run //:smoke -- -t //:app_android -d android --build-arg=--config=android
///
/// It starts `flutter_bazel run --machine`, which builds the app, installs and
/// launches it, and asks the running app for its semantics tree over the
/// machine protocol until every line in [_expected] is on screen. The gallery
/// awaits `initBridge()` before its first frame, and each line is the result of
/// a Rust call: a sync call and an async call.
///
/// Anything else the tool accepts (`-c`, `--build-arg`, `--wasm`, ...) passes
/// through to `flutter_bazel run`.
library;

import 'dart:async';
import 'dart:collection';
import 'dart:convert';
import 'dart:io';

/// What the gallery shows once its first calls have returned.
/// How long one semantics request may go unanswered before it is sent again.
const _attempt = Duration(seconds: 20);

final _expected = <String, RegExp>{
  'a sync call': RegExp('Hello, Flutter — from Rust'),
  'an async call': RegExp('The 10000th prime is 104729'),
};

Future<void> main(List<String> args) async {
  final timeout = Duration(
    seconds: int.parse(
      args
              .where((a) => a.startsWith('--timeout='))
              .map((a) => a.substring('--timeout='.length))
              .firstOrNull ??
          '900',
    ),
  );
  final toolArg = args
      .firstWhere((a) => a.startsWith('--tool='), orElse: () => '')
      .substring('--tool='.length);
  final passthrough = args
      .where((a) => !a.startsWith('--timeout=') && !a.startsWith('--tool='))
      .toList();
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  if (workspace == null) {
    stderr.writeln(
      'smoke: run this as `bazel run //:smoke -- <flutter_bazel run args>`.',
    );
    exit(2);
  }

  // The dev tool is a runfile, not a nested `bazel run`: a Bazel client in
  // between need not pass stdin on, and the protocol's requests arrive on it.
  final tool = await Process.start(
    File(toolArg).absolute.path,
    ['run', '--machine', '--no-devtools', ...passthrough],
    workingDirectory: workspace,
    environment: {...Platform.environment, 'LOG_FORMAT': 'json'},
  );
  final messages = StreamController<Map<String, dynamic>>.broadcast();
  tool.stdout.transform(utf8.decoder).transform(const LineSplitter()).listen((
    line,
  ) {
    try {
      // One message per line, wrapped in a one-element array, as Flutter's
      // daemon writes them.
      final decoded = json.decode(line);
      for (final msg in decoded is List ? decoded : [decoded]) {
        if (msg is Map<String, dynamic>) messages.add(msg);
      }
    } on FormatException {
      // Not protocol: a line of the tool's own log.
    }
  });
  // The tool's own log is long: kept, and shown only when the check fails.
  final toolLog = Queue<String>();
  tool.stderr.transform(utf8.decoder).transform(const LineSplitter()).listen((
    line,
  ) {
    toolLog.add(line);
    if (toolLog.length > 300) toolLog.removeFirst();
  });

  var finished = false;
  unawaited(
    tool.exitCode.then((code) {
      if (finished) return;
      stderr.writeln(
        'smoke: the dev tool exited with $code. Its last log lines:',
      );
      toolLog.forEach(stderr.writeln);
      exit(1);
    }),
  );

  var exitCode = 1;
  final deadline = DateTime.now().add(timeout);
  try {
    Duration left() {
      final d = deadline.difference(DateTime.now());
      return d.isNegative ? Duration.zero : d;
    }

    final started = messages.stream.firstWhere(
      (m) => m['event'] == 'app.started',
    );
    final appId =
        (await started.timeout(
              left(),
              onTimeout: () => throw TimeoutException(
                'the app did not start within $timeout',
              ),
            ))['params']['appId']
            as String;

    var id = 0;
    final missing = {..._expected};
    String? last;
    while (missing.isNotEmpty && left() > Duration.zero) {
      final request = ++id;
      final reply = messages.stream.firstWhere((m) => m['id'] == request);
      tool.stdin.writeln(
        json.encode({
          'id': request,
          'method': 'app.dumpSemanticsTree',
          'params': {'appId': appId},
        }),
      );
      // A request sent before the session is ready may never be answered, so
      // each attempt is bounded and the next one is a new request.
      final Map<String, dynamic> answer;
      try {
        answer = await reply.timeout(left() < _attempt ? left() : _attempt);
      } on TimeoutException {
        continue;
      }
      last =
          (answer['result'] as Map?)?['text'] as String? ??
          '${answer['error']}';
      missing.removeWhere((_, re) => re.hasMatch(last!));
      if (missing.isNotEmpty) {
        await Future<void>.delayed(const Duration(seconds: 2));
      }
    }
    if (missing.isEmpty) {
      stdout.writeln(
        'smoke: the bridge answered (${_expected.keys.join(', ')}).',
      );
      exitCode = 0;
    } else {
      stderr.writeln(
        'smoke: not on screen: ${missing.keys.join(', ')}.\n'
        'The last semantics tree was:\n$last',
      );
    }
  } on TimeoutException catch (e) {
    stderr.writeln('smoke: timed out: ${e.message ?? e}');
  } finally {
    finished = true;
    if (exitCode != 0) {
      stderr.writeln('smoke: the dev tool\'s last log lines:');
      toolLog.forEach(stderr.writeln);
    }
    tool.stdin.writeln(json.encode({'id': -1, 'method': 'daemon.shutdown'}));
    await tool.exitCode.timeout(
      const Duration(seconds: 60),
      onTimeout: () {
        tool.kill();
        return -1;
      },
    );
  }
  exit(exitCode);
}
