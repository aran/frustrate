/// The wasi demo: one screen that exercises every `std` facility this platform
/// exists to provide, and reports on each.
///
/// The reporting shape is deliberate. Each check renders a line beginning with
/// a stable `WASI-…` token, so `playwright/wasi.spec.js` can assert on the
/// *values* through Flutter's accessibility tree rather than on a screenshot.
/// A check that cannot be asserted from outside the app is a check that rots.
///
/// Nothing in this file is platform-aware. The same source runs on macOS, where
/// these calls are ordinary `std`, and on web, where each one is a
/// `wasi_snapshot_preview1` import frustrate's JS runtime answers.
library;

import 'package:flutter/material.dart';
import 'package:wasi_bridge/init.dart';
import 'package:wasi_bridge/wasi_rust.frustrate.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initBridge();
  runApp(const WasiDemoApp());
}

class WasiDemoApp extends StatelessWidget {
  const WasiDemoApp({super.key});

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Frustrate wasi Demo',
        theme: ThemeData(useMaterial3: true, colorSchemeSeed: Colors.teal),
        home: const _Home(),
      );
}

class _Home extends StatefulWidget {
  const _Home();
  @override
  State<_Home> createState() => _HomeState();
}

class _HomeState extends State<_Home> {
  /// Constructing this calls `Instant::now()` in Rust. If MONOTONIC were
  /// missing the app would die here, before the first frame.
  final EventLog _log = EventLog.new_();

  late final EntropyReport _entropy;
  late final int _envCount;
  late final int _clockSkewMs;
  Duration? _firstElapsed;
  Duration? _lastElapsed;
  final List<Entry> _entries = [];

  @override
  void initState() {
    super.initState();

    // Entropy. See the doc comment on `entropy_report` in bridge/src/api.rs:
    // this is the one failure on this platform that is both catastrophic and
    // silent, so it is sampled rather than trusted.
    _entropy = entropyReport(samples: 64);

    // `environ_sizes_get` / `environ_get`. Frustrate reports an empty
    // environment; the point of asking is that reaching an *unimplemented*
    // import throws, so a returned 0 distinguishes "no variables" from "never
    // asked".
    _envCount = envVarCount();

    // REALTIME. Rust's `SystemTime::now()` crosses as a Dart `DateTime`, so
    // the skew against Dart's own clock is directly measurable — and a host
    // that returned a constant, or seconds where nanoseconds were wanted,
    // shows up here as a skew of years rather than milliseconds.
    final probe = _log.append(label: 'clock-probe');
    _clockSkewMs =
        probe.at.difference(DateTime.now().toUtc()).inMilliseconds.abs();
    _firstElapsed = probe.sinceStart;
    _lastElapsed = probe.sinceStart;
    _entries.add(probe);
  }

  @override
  void dispose() {
    _log.dispose();
    super.dispose();
  }

  void _append() {
    final e = _log.append(label: 'entry-${_entries.length}');
    setState(() {
      _entries.add(e);
      _lastElapsed = e.sinceStart;
    });
  }

  /// MONOTONIC never goes backwards. One append is enough to have two samples
  /// (the clock probe in `initState` is the first).
  bool get _monotonicOk =>
      _firstElapsed != null &&
      _lastElapsed != null &&
      _lastElapsed! >= _firstElapsed!;

  @override
  Widget build(BuildContext context) {
    final ids = _entries.map((e) => e.id).toSet();
    return Scaffold(
      appBar: AppBar(title: const Text('frustrate on wasm32-wasip1')),
      floatingActionButton: FloatingActionButton.extended(
        onPressed: _append,
        label: const Text('Append entry'),
        icon: const Icon(Icons.add),
      ),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          const _Section('What this app is'),
          const Text(
            'Every line below is a std facility that wasm32-unknown-unknown '
            'does not have. This bridge is built for wasm32-wasip1 and the '
            'imports are answered by frustrate\'s own web runtime — no extra '
            'package, no build step.',
          ),
          const SizedBox(height: 16),
          const _Section('Checks'),

          // random_get
          _Check(
            'WASI-ENTROPY',
            ok: _entropy.healthy,
            detail: 'healthy=${_entropy.healthy} '
                'distinct=${_entropy.distinct}/${_entropy.samples} '
                'nil=${_entropy.anyNil} '
                'bits=${_entropy.setBits}/${_entropy.totalBits}',
            note: 'uuid v4 -> getrandom -> random_get',
          ),

          // clock_time_get(REALTIME)
          _Check(
            'WASI-CLOCK',
            ok: _clockSkewMs < 60000,
            detail: 'skew_ms=$_clockSkewMs',
            note: 'SystemTime::now() -> clock_time_get(REALTIME), '
                'against Dart\'s DateTime.now()',
          ),

          // clock_time_get(MONOTONIC)
          _Check(
            'WASI-MONOTONIC',
            ok: _monotonicOk,
            detail: 'increasing=$_monotonicOk '
                'last_us=${_lastElapsed?.inMicroseconds}',
            note: 'Instant::elapsed() -> clock_time_get(MONOTONIC)',
          ),

          // environ_get / environ_sizes_get
          _Check(
            'WASI-ENV',
            ok: _envCount >= 0,
            detail: 'count=$_envCount',
            note: 'std::env::vars() -> environ_sizes_get + environ_get',
          ),

          // The cross-check: distinct ids in the log itself, not just in the
          // entropy sample. Catches a source that is healthy when sampled in a
          // tight loop but stuck between calls.
          _Check(
            'WASI-IDS',
            ok: ids.length == _entries.length,
            detail: 'unique=${ids.length}/${_entries.length}',
            note: 'one v4 UUID per log entry',
          ),

          const SizedBox(height: 16),
          const _Section('Log'),
          const Text(
            'Each append also runs a Rust println!, which reaches fd_write and '
            'lands in the browser console. That is the only way to see a wasm '
            'module\'s stdout, and on the default platform it is silently '
            'dropped.',
            style: TextStyle(fontSize: 12),
          ),
          const SizedBox(height: 8),
          for (final e in _entries)
            Padding(
              padding: const EdgeInsets.symmetric(vertical: 2),
              child: Text(
                'WASI-ENTRY ${e.label} ${e.id} '
                'at=${e.at.toIso8601String()} '
                'elapsed_us=${e.sinceStart.inMicroseconds}',
                style: const TextStyle(fontFamily: 'monospace', fontSize: 11),
              ),
            ),
        ],
      ),
    );
  }
}

class _Section extends StatelessWidget {
  const _Section(this.title);
  final String title;
  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.only(bottom: 8),
        child: Text(title, style: Theme.of(context).textTheme.titleMedium),
      );
}

/// One check, rendered as a single line whose text is the assertion surface.
class _Check extends StatelessWidget {
  const _Check(this.token,
      {required this.ok, required this.detail, required this.note});
  final String token;
  final bool ok;
  final String detail;
  final String note;

  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(ok ? Icons.check_circle : Icons.error,
                color: ok ? Colors.green : Colors.red, size: 18),
            const SizedBox(width: 8),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('$token ${ok ? 'PASS' : 'FAIL'} $detail',
                      style: const TextStyle(
                          fontFamily: 'monospace', fontSize: 12)),
                  Text(note,
                      style: TextStyle(
                          fontSize: 11, color: Theme.of(context).hintColor)),
                ],
              ),
            ),
          ],
        ),
      );
}
