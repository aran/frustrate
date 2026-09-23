// The driven two-peer test: `bazel run //tools:two_peers`.
//
// Proves, without a human looking at anything, that a message typed into the
// running Tin Can app reaches a real second peer and that the peer's reply
// reaches the app. The fourth of the five test layers in ../README.md, and the
// only one that drives the real UI.
//
// What it does, in order:
//
//   1. builds //peerbot:peerbot and //:app;
//   2. starts peerbot — a headless iroh peer on `presets::Minimal`, so no
//      relay, no DNS and no pkarr — and reads its ticket off stdout;
//   3. launches the app under `flutter_bazel run --machine`, which prints an
//      HTTP control channel URI and token;
//   4. drives the UI through that channel: read `ticketText`, type peerbot's
//      ticket into `connectField`, press `connectButton`, select the peer,
//      type a message, press `sendButton`;
//   5. asserts the message text arrived, twice over — peerbot's stdout is an
//      independent witness that the bytes crossed the wire, and `messageLog`
//      read back through the control channel is the app's own state;
//   6. screenshots, shuts both halves down, and exits 0.
//
// Every assertion is on text the app actually holds. Nothing here concludes
// anything from a screenshot; the PNG is an artifact, not evidence.
//
// # Hermeticity
//
// **This contacts nobody.** Both peers bind `presets::Minimal` — no relay, no
// DNS, no pkarr — and pair by a full ticket, which carries addresses, so iroh
// dials loopback directly and no packet leaves the host. That is not a reduced
// stand-in for the real thing: it is how two peers on one machine genuinely
// connect. Only the browser needs a relay, because only the browser has no UDP
// socket.
//
// This used to be false of the app half, and it took a bridge change to fix
// rather than a flag: `Node::open` hardcoded `presets::N0`, so the app
// published to pkarr and reached for n0's relays no matter what this driver
// did. The `Preset` argument is what fixed it.
//
// The default is also what the assertions now pin — step 0 fails the run if the
// app's transport chip says anything but "no relay — direct only", so a build
// that quietly acquired a third party cannot pass.
//
// # Using a relay, or n0
//
//     bazel run //relay:relay_dev                      # in another shell
//     bazel run //tools:two_peers -- --relay http://127.0.0.1:PORT
//
// `--relay` puts both peers on `presets::Minimal` through that relay: one relay
// of our own and no n0 service at all. It is passed to peerbot as `--relay` and
// to the app as `--dart-define=RELAY_URL=…`.
//
//     bazel run //tools:two_peers -- --n0
//
// `--n0` builds the app with `PRESET=n0`: n0's public relays, pkarr and DNS.
// The only reason to run it is to check that path still works. peerbot stays
// `Minimal` either way — it hands its ticket to this driver on stdout, so it
// has nothing to discover.
//
// # Why this is not a `bazel test`
//
// It must invoke bazel itself (`flutter_bazel run` builds and installs the
// app), and a test action holds the very lock that inner build needs. So it is
// a `bazel run` target. The consequence is that `bazel test //...` does not
// run the only test that proves two peers talk: no test log, no timeout, no
// sharding, no --runs_per_test, and nothing forcing it to keep working.

import 'dart:async';
import 'dart:convert';
import 'dart:io';

/// The line the app sends. Distinctive on purpose: it has to be findable in a
/// log that also contains the echo of itself.
const String kMessage = 'ping from the driven app';

/// peerbot's echo prefix, so the app's log line is `echo: <kMessage>` and the
/// two directions are distinguishable in one string.
const String kPrefix = 'echo: ';

/// The nickname the app is launched with, asserted from peerbot's side: it is
/// how the bot knows the thing that dialled it was this app and not a stray.
const String kAppNickname = 'drivenapp';

/// What both peers are configured with. One object because the two halves must
/// agree: a run where the app and peerbot disagree about the relay is a run
/// where nothing connects and nothing says why.
class Net {
  const Net({required this.preset, this.relay});

  /// `minimal` or `n0`, spelled the way both `--dart-define=PRESET` and
  /// `peerbot --preset` spell it.
  final String preset;

  /// The relay both peers use, or null for whatever the preset chose.
  final String? relay;

  /// The default: no third party of any kind.
  ///
  /// Both peers on `presets::Minimal` with no relay. This is not a reduced
  /// configuration standing in for a real one — it is how two peers on one host
  /// actually connect. The ticket carries loopback addresses, iroh dials them
  /// directly, and there is nothing for a relay to carry. The browser is the
  /// only peer that needs one, because it is the only peer with no UDP socket.
  static const hermetic = Net(preset: 'minimal');

  /// `--n0`: n0's public relays, pkarr and DNS on the app side.
  static const n0 = Net(preset: 'n0');

  bool get isHermetic => preset == 'minimal' && relay == null;

  String describe() {
    if (isHermetic) {
      return 'both peers on presets::Minimal with no relay — '
          'no relay, no DNS, no pkarr, nothing outside this machine';
    }
    if (preset == 'minimal') {
      return 'both peers on presets::Minimal through $relay — '
          'that relay and no n0 service at all';
    }
    if (relay == null) {
      return 'the app on presets::N0 — n0\'s public relays, pkarr and DNS';
    }
    return 'the app on presets::N0 through $relay — n0\'s relays are out of '
        'the data path, but pkarr and DNS are still theirs';
  }
}

/// The `--dart-define`s the app is launched with, in order.
///
/// `RELAY_URL` is present only when a relay was named: an empty define and an
/// absent one mean the same thing to `main.dart` (both become a `null` relay
/// argument), but passing an empty one would change the build configuration for
/// no reason and cost a second full compile.
List<String> _defines(Net net) => [
      'NICKNAME=$kAppNickname',
      'PRESET=${net.preset}',
      if (net.relay != null) 'RELAY_URL=${net.relay}',
    ];

/// The build setting behind `flutter_bazel --dart-define`. Passed to the
/// pre-build too, so the dev tool's own build is a cache hit rather than a
/// second full compile in a second configuration.
///
/// The flag repeats rather than taking a list: `extra_dart_defines` is a
/// string_list build setting, and one `--flag=a --flag=b` pair is how Bazel
/// accumulates one.
List<String> _defineFlags(Net net) => [
      for (final define in _defines(net))
        '--@rules_flutter//flutter:extra_dart_defines=$define',
    ];

/// How long to wait for the dev tool to have both a control channel and an app.
/// Generous: it builds and installs the bundle first.
const Duration kLaunch = Duration(minutes: 15);

/// How long a UI-visible consequence of a network event may take.
const Duration kNetwork = Duration(seconds: 60);

final Stopwatch _clock = Stopwatch()..start();

void log(String message) {
  final t = (_clock.elapsedMilliseconds / 1000).toStringAsFixed(1).padLeft(6);
  stdout.writeln('[drive $t] $message');
}

/// An assertion that failed, as opposed to a crash.
class DriveFailure implements Exception {
  DriveFailure(this.message);
  final String message;
  @override
  String toString() => message;
}

Future<void> main(List<String> argv) async {
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'] ??
      Directory.current.path;
  final screenshot = _flag(argv, '--screenshot') ??
      '${Directory.systemTemp.path}/tin_can_two_peers.png';
  final keepGoing = argv.contains('--no-shutdown');
  // The default contacts nobody. `--relay` self-hosts, `--n0` opts into n0's
  // public infrastructure; they are mutually exclusive, because "relay through
  // ours" and "use theirs" are two answers to one question.
  final relayFlag = _flag(argv, '--relay');
  final wantsN0 = argv.contains('--n0');
  if (relayFlag != null && wantsN0) {
    log('--relay and --n0 cannot both be given: --relay names a relay to use, '
        '--n0 asks for n0\'s.');
    exitCode = 2;
    return;
  }
  final net = switch ((relayFlag, wantsN0)) {
    (final url?, _) => Net(preset: 'minimal', relay: url),
    (null, true) => Net.n0,
    (null, false) => Net.hermetic,
  };

  Peerbot? bot;
  DevTool? tool;
  var failure = '';
  try {
    log('network: ${net.describe()}');
    await _build(workspace, net);
    bot = await Peerbot.start(workspace, net);
    tool = await DevTool.start(workspace, net);
    await _drive(tool, bot, screenshot, net);
    log('PASS — a message crossed both ways and the app can prove it');
    if (argv.contains('--probe-restart')) await _probeRestart(tool);
  } on DriveFailure catch (e) {
    failure = e.message;
  } catch (e, stack) {
    failure = '$e\n$stack';
  }

  if (failure.isNotEmpty) {
    stderr.writeln('\n=== FAILED ===\n$failure\n');
    if (bot != null) {
      stderr.writeln('--- peerbot said ---');
      for (final line in bot.lines) {
        stderr.writeln('  $line');
      }
    }
    if (tool != null) {
      stderr.writeln('--- the app said ---');
      for (final line in await tool.appLogs()) {
        stderr.writeln('  $line');
      }
      stderr.writeln('--- the dev tool said (tail) ---');
      final tail = tool.stderrLines;
      for (final line in tail.skip(tail.length > 40 ? tail.length - 40 : 0)) {
        stderr.writeln('  $line');
      }
    }
  }

  if (!keepGoing) {
    await tool?.shutdown();
    await bot?.shutdown();
  }
  exit(failure.isEmpty ? 0 : 1);
}

String? _flag(List<String> argv, String name) {
  final i = argv.indexOf(name);
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : null;
}

/// Build both halves before anything is launched.
///
/// Deliberately first and deliberately blocking: `flutter_bazel` runs bazel
/// itself, so every build this test needs happens while nothing else holds the
/// workspace.
Future<void> _build(String workspace, Net net) async {
  log('building //peerbot:peerbot and //:app');
  final build = await Process.start(
    'bazel',
    ["build", "//peerbot:peerbot", "//:app", ..._defineFlags(net)],
    workingDirectory: workspace,
    mode: ProcessStartMode.inheritStdio,
  );
  if (await build.exitCode != 0) {
    throw DriveFailure('bazel build failed');
  }
}

// ============================================================== peerbot ===

/// The headless peer, and everything it has said.
class Peerbot {
  Peerbot._(this._process);

  final Process _process;

  /// peerbot's endpoint id — the app's per-peer widget keys are suffixed with
  /// it, so this is what `peer:`, `path:` and `disconnect:` resolve to.
  late final String id;

  /// What goes into the app's `connectField`.
  late final String ticket;

  /// Every line peerbot has produced, in order, stderr included.
  final List<String> lines = <String>[];
  final StreamController<String> _incoming = StreamController.broadcast();

  static Future<Peerbot> start(String workspace, Net net) async {
    final exe = '$workspace/bazel-bin/peerbot/peerbot';
    if (!File(exe).existsSync()) {
      throw DriveFailure('peerbot was built but is not at $exe');
    }
    // peerbot stays on `presets::Minimal` whatever the app does. Even under
    // `--n0` there is no reason for the *bot* to publish to pkarr: it hands its
    // ticket to the driver directly, on stdout.
    log(net.relay == null
        ? 'starting peerbot (presets::Minimal — no relay, no DNS, no pkarr)'
        : 'starting peerbot (presets::Minimal + --relay ${net.relay} — that '
            'relay and nothing else)');
    final process = await Process.start(
      exe,
      [
        '--nickname', 'peerbot',
        '--prefix', kPrefix,
        if (net.relay != null) ...['--relay', net.relay!],
      ],
      workingDirectory: workspace,
    );
    final bot = Peerbot._(process);
    process.stdout
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen((line) {
      bot.lines.add(line);
      bot._incoming.add(line);
    });
    process.stderr
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen((line) => bot.lines.add('stderr: $line'));

    bot.id = await bot._field('id');
    bot.ticket = await bot._field('ticket');
    await bot.expect('ready', 'peerbot: ready', const Duration(seconds: 30));
    log('peerbot id ${bot.id.substring(0, 12)}…, '
        'ticket ${bot.ticket.length} chars');
    return bot;
  }

  Future<String> _field(String verb) async {
    final prefix = 'peerbot: $verb ';
    final line =
        await expect('its $verb', prefix, const Duration(seconds: 30));
    return line.substring(line.indexOf(prefix) + prefix.length).trim();
  }

  /// Wait for a line containing [pattern], counting ones already seen.
  ///
  /// Scanning the backlog first is not an optimisation: peerbot prints
  /// `connected` the moment the handshake completes, which can easily be
  /// before the driver gets around to asking.
  Future<String> expect(String what, Pattern pattern, Duration within) async {
    for (final line in lines) {
      if (line.contains(pattern)) return line;
    }
    try {
      return await _incoming.stream
          .firstWhere((line) => line.contains(pattern))
          .timeout(within);
    } on TimeoutException {
      throw DriveFailure('peerbot never reported $what');
    }
  }

  Future<void> shutdown() async {
    log('stopping peerbot');
    try {
      // EOF on stdin is peerbot's graceful shutdown: it closes the endpoint
      // and prints "bye". There is no SIGINT handler to use instead: peerbot
      // shares the app's crate hub, which cannot add `tokio/signal`.
      await _process.stdin.close();
      await _process.exitCode.timeout(const Duration(seconds: 10));
    } catch (_) {
      _process.kill();
    }
  }
}

// ============================================================= dev tool ===

/// `flutter_bazel run --machine`, plus its HTTP control channel.
class DevTool {
  DevTool._(this._process);

  final Process _process;

  late final Uri uri;
  late final String token;
  late final String appId;

  final List<String> stderrLines = <String>[];

  /// How many `http_control_channel` banners the dev tool has emitted. A
  /// relaunch ought to emit a second one; this is how [_probeRestart] checks
  /// whether it does.
  int channelBanners = 0;

  static Future<DevTool> start(String workspace, Net net) async {
    log('launching the app under flutter_bazel (it builds and installs it)');
    final process = await Process.start(
      'bazel',
      [
        'run',
        '@rules_flutter//tools/dev_tool:flutter_bazel',
        '--',
        'run',
        '--target',
        '//:app',
        '--device',
        'macos',
        '--machine',
        for (final define in _defines(net)) ...['--dart-define', define],
      ],
      workingDirectory: workspace,
      // LOG_FORMAT=json is what turns the control-channel banner into one
      // parseable stderr line instead of four lines of prose.
      environment: {...Platform.environment, 'LOG_FORMAT': 'json'},
    );

    final tool = DevTool._(process);
    final channel = Completer<List<String>>();
    final started = Completer<String>();

    process.stderr
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen((line) {
      tool.stderrLines.add(line);
      Object? decoded;
      try {
        decoded = json.decode(line);
      } catch (_) {
        return;
      }
      if (decoded is Map && decoded['message'] == 'http_control_channel') {
        tool.channelBanners++;
        if (!channel.isCompleted) {
          channel
              .complete([decoded['uri'] as String, decoded['token'] as String]);
        }
      }
    });

    process.stdout
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen((line) {
      // The machine protocol wraps every message in `[{…}]`. Anything else on
      // this stream is noise the protocol does not own.
      final start = line.indexOf('[{');
      if (start < 0 || !line.endsWith('}]')) return;
      List<dynamic> messages;
      try {
        messages = json.decode(line.substring(start)) as List<dynamic>;
      } catch (_) {
        return;
      }
      for (final message in messages) {
        if (message is Map &&
            message['event'] == 'app.start' &&
            !started.isCompleted) {
          started.complete((message['params'] as Map)['appId'] as String);
        }
      }
    });

    unawaited(process.exitCode.then((code) {
      final why = DriveFailure(
          'the dev tool exited ($code) before the app was drivable:\n'
          '  ${tool.stderrLines.join("\n  ")}');
      if (!channel.isCompleted) channel.completeError(why);
      if (!started.isCompleted) started.completeError(why);
    }));

    final pair = await channel.future.timeout(kLaunch,
        onTimeout: () => throw DriveFailure(
            'no http_control_channel within ${kLaunch.inMinutes} min'));
    tool.uri = Uri.parse(pair[0]);
    tool.token = pair[1];
    tool.appId = await started.future.timeout(kLaunch,
        onTimeout: () =>
            throw DriveFailure('no app.start within ${kLaunch.inMinutes} min'));
    log('control channel ${tool.uri}, app ${tool.appId}');
    return tool;
  }

  /// Run one machine-protocol method against the running app.
  ///
  /// A dev-tool-level failure is an HTTP status; an app-level failure ("no
  /// widget matching …") comes back 200 with an `error` field, so both are
  /// checked here. Returning an error map quietly is how a driver ends up
  /// asserting against `null` three steps later.
  Future<Map<String, dynamic>> call(String method,
      [Map<String, dynamic> params = const {}]) async {
    final client = HttpClient();
    try {
      final request = await client.postUrl(
          uri.replace(path: '/command', queryParameters: {'token': token}));
      request.headers.contentType = ContentType.json;
      request.write(json.encode({
        'method': method,
        'params': {'appId': appId, ...params},
      }));
      final response = await request.close();
      final body = await utf8.decoder.bind(response).join();
      if (response.statusCode != 200) {
        throw DriveFailure('$method → HTTP ${response.statusCode}: $body');
      }
      final result = (json.decode(body) as Map<String, dynamic>)['result'];
      if (result is Map<String, dynamic>) {
        if (result['error'] != null) {
          throw DriveFailure(
              '$method ${json.encode(params)} → ${result['error']}');
        }
        return result;
      }
      return {'result': result};
    } finally {
      client.close();
    }
  }

  /// The text of the widget with this key.
  Future<String> text(String key) async =>
      (await call('app.getText', {'key': key}))['text'] as String;

  /// Tap a widget, scrolling the page to it first when it is below the fold.
  ///
  /// `page` is the key C4 put on the app's `ListView` for exactly this: a
  /// widget a lazy list has not built yet cannot be found, let alone tapped.
  Future<void> tap(String key) async {
    await call('app.scrollIntoView', {'key': key, 'scrollableKey': 'page'});
    await call('app.tap', {'key': key});
  }

  /// Focus a field and replace its contents.
  ///
  /// The tap is not optional: `app.enterText` writes to whatever
  /// `EditableText` currently holds focus, and errors when nothing does.
  Future<void> type(String key, String value) async {
    await tap(key);
    await call('app.enterText', {'text': value});
  }

  /// Poll [read] until [accept] likes the answer, then return it.
  Future<String> until(
    String what,
    Future<String> Function() read,
    bool Function(String) accept, {
    Duration within = kNetwork,
  }) async {
    final deadline = DateTime.now().add(within);
    String? last;
    while (DateTime.now().isBefore(deadline)) {
      try {
        last = await read();
        if (accept(last)) return last;
      } on DriveFailure catch (e) {
        last = '<$e>';
      }
      await Future<void>.delayed(const Duration(milliseconds: 250));
    }
    throw DriveFailure('gave up waiting for $what after ${within.inSeconds}s; '
        'last saw: $last');
  }

  Future<List<String>> appLogs() async {
    final client = HttpClient();
    try {
      final request = await client.getUrl(uri.replace(
          path: '/sessions/$appId/logs', queryParameters: {'token': token}));
      final response = await request.close();
      final body = await utf8.decoder.bind(response).join();
      if (response.statusCode != 200) return ['<logs unavailable: $body>'];
      final lines = (json.decode(body) as Map<String, dynamic>)['lines'] as List;
      return [for (final line in lines) (line as Map)['t'].toString()];
    } catch (e) {
      return ['<logs unavailable: $e>'];
    } finally {
      client.close();
    }
  }

  Future<void> screenshot(String path) async {
    final client = HttpClient();
    try {
      final request = await client.getUrl(uri.replace(
          path: '/sessions/$appId/screenshot/flutter',
          queryParameters: {'token': token}));
      final response = await request.close();
      if (response.statusCode != 200) {
        log('screenshot unavailable (HTTP ${response.statusCode})');
        return;
      }
      final bytes = <int>[];
      await for (final chunk in response) {
        bytes.addAll(chunk);
      }
      File(path).writeAsBytesSync(bytes);
      log('screenshot → $path (${bytes.length} bytes)');
    } catch (e) {
      log('screenshot failed: $e');
    } finally {
      client.close();
    }
  }

  Future<void> shutdown() async {
    log('shutting the app down');
    try {
      await call('app.stop').timeout(const Duration(seconds: 30));
    } catch (_) {
      // Already gone, or wedged; daemon.shutdown below is the backstop.
    }
    try {
      await call('daemon.shutdown').timeout(const Duration(seconds: 30));
    } catch (_) {}
    try {
      await _process.exitCode.timeout(const Duration(seconds: 30));
    } on TimeoutException {
      _process.kill();
    }
  }
}

// ============================================================ the drive ===

Future<void> _drive(
    DevTool app, Peerbot bot, String screenshotPath, Net net) async {
  // 0. Both peers really took the relay. peerbot prints the relay url out of
  //    its own EndpointAddr, so this is the endpoint's answer and not an echo
  //    of the flag; the app's chip is the same fact on the other side.
  if (net.relay != null) {
    final line = await bot.expect(
        'its relay address', 'peerbot: relay ', const Duration(seconds: 30));
    log('peerbot bound $line');
    final chip = await app.text('transportChip');
    if (!chip.startsWith('relay ')) {
      throw DriveFailure(
          'the app was launched with RELAY_URL=${net.relay} but its '
          'transport chip reads "$chip"');
    }
    log('the app chip reads "$chip"');
  } else if (net.isHermetic) {
    // The other half of the same check, and the one that actually pins this
    // run's headline claim. A chip reading anything else means the app was
    // built with a preset it was not asked for — the silent regression this
    // whole change exists to make impossible.
    final chip = await app.text('transportChip');
    if (chip != 'no relay — direct only') {
      throw DriveFailure('this run is meant to contact nobody, but the app\'s '
          'transport chip reads "$chip"');
    }
    log('the app chip reads "$chip" — no third party configured');
  }

  // 1. The app is up and has bound an endpoint of its own. `ticketText` starts
  //    at "no ticket yet" and is replaced by the first Listening event, so
  //    this is the app proving Rust answered, not just that Flutter drew.
  //
  //    When it never arrives, `ticketText` says nothing useful and the reason
  //    is one widget over: a rejected relay url, a failed bind, anything
  //    `Node.open` threw is rendered into `statusText`. Quoting it turns "gave
  //    up waiting" into an actual diagnosis.
  final String ticket;
  try {
    ticket = await app.until(
      'the app to publish its own ticket',
      () => app.text('ticketText'),
      (t) => t.isNotEmpty && t != 'no ticket yet',
      within: const Duration(seconds: 90),
    );
  } on DriveFailure catch (e) {
    String status;
    try {
      status = await app.text('statusText');
    } catch (_) {
      status = '<statusText unreadable too>';
    }
    throw DriveFailure('${e.message}\n  the app\'s own status line reads: '
        '$status');
  }
  if (ticket.length < 32) {
    throw DriveFailure('that is not a ticket: "$ticket"');
  }
  log('the app is listening; its ticket is ${ticket.length} chars');

  // 2. Pair. This is the user's actual gesture: paste, press Connect.
  log("typing peerbot's ticket into connectField and pressing Connect");
  await app.type('connectField', bot.ticket);
  await app.tap('connectButton');

  // 3. Both sides agree a connection exists. peerbot's line is the independent
  //    witness — and it carries the nickname, which is the --dart-define this
  //    run launched the app with, so the peer that dialled really is this app.
  final connected =
      await bot.expect('a connection from the app', 'peerbot: connected ', kNetwork);
  final appPeerId = connected.split(' ')[2];
  if (!connected.endsWith(' $kAppNickname')) {
    throw DriveFailure('peerbot was dialled by "$connected", '
        'expected the nickname $kAppNickname');
  }
  await app.call('app.waitFor',
      {'key': 'peer:${bot.id}', 'timeoutMs': kNetwork.inMilliseconds});
  log('connected: the app sees peer:${bot.id.substring(0, 12)}…, '
      'peerbot sees ${appPeerId.substring(0, 12)}… as "$kAppNickname"');

  // 4. Select the peer, then send. Selecting is what opens the message pane;
  //    without it `messageLog` reads "no peer selected".
  await app.tap('peer:${bot.id}');
  log('sending "$kMessage" from the app');
  await app.type('messageField', kMessage);
  await app.tap('sendButton');

  // 5. The bytes crossed. This assertion is peerbot's, not the app's: it read
  //    the frame off a QUIC stream and decoded it with the same proto.rs the
  //    app encoded it with.
  await bot.expect(
      'the message arriving', 'peerbot: recv $appPeerId $kMessage', kNetwork);
  log('peerbot received it — the message really crossed the wire');

  // 6. And back. peerbot echoed with a prefix, so finding `echo: <message>` in
  //    the app's own log proves the *return* direction independently of the
  //    outbound one: that text was composed on the other peer.
  final logText = await app.until(
    'the echo to appear in messageLog',
    () => app.text('messageLog'),
    (t) => t.contains('$kPrefix$kMessage'),
  );
  if (!logText.contains(kMessage)) {
    throw DriveFailure('messageLog lost the outbound line:\n$logText');
  }
  log('the app received the echo');

  // 7. The path badge. Both peers are on this host and pairing was by ticket,
  //    so the only path there is is a direct one — M1's claim, asserted on the
  //    widget a human would otherwise have squinted at.
  final path = await app.until(
    'the path badge to settle',
    () => app.text('path:${bot.id}'),
    // With a custom relay the first badge legitimately reads `relay` — the
    // connection is relayed until loopback hole punching lands — so wait for
    // the end state rather than the first one. With no relay configured there
    // is no relay path to see and the first badge is already `direct`, so the
    // default predicate is unchanged.
    (t) => t == 'direct' || (net.relay == null && t == 'relay'),
  );
  if (path != 'direct') {
    throw DriveFailure(
        'two peers on one host should be direct; the badge says "$path"');
  }
  log('the path badge reads direct');

  // 8. Nothing failed quietly. The app renders every failure into failureText,
  //    so a run that satisfied every assertion above while the app was full of
  //    errors is not a passing run.
  final failure = await app.text('failureText');
  if (failure != 'no failures') {
    throw DriveFailure('the app reported: $failure');
  }

  await app.screenshot(screenshotPath);
}

/// Ask whether `app.restart` is usable by a driver at all.
///
/// It is not, in one case: when the native library has changed, the restart
/// relaunches the app and the HTTP control channel's listening socket closes
/// and never re-opens — no second banner, no new port, and a driver that has
/// lost the app it just relaunched. A bridge demo's native library changes
/// constantly, so that is a likely first restart.
///
/// Reports, never asserts: it runs after every assertion above has passed, so
/// what it finds cannot make a good run red. The default drive never restarts,
/// which is how this test is designed around the defect; this flag is how it
/// gets re-checked when rules_flutter changes.
Future<void> _probeRestart(DevTool app) async {
  final before = app.channelBanners;
  log('probe: POST app.restart');
  Object response;
  try {
    response = await app.call('app.restart').timeout(kLaunch);
  } catch (e) {
    response = '<threw: $e>';
  }
  log('probe: restart said $response');
  // Give a relaunch time to re-open a channel, if it ever does.
  await Future<void>.delayed(const Duration(seconds: 3));
  String alive;
  try {
    alive = 'yes — statusText reads "${await app.text('statusText')}"';
  } catch (e) {
    alive = 'NO — $e';
  }
  log('probe: is the control channel still answering? $alive');
  log('probe: new http_control_channel banners since the restart: '
      '${app.channelBanners - before}');
}
