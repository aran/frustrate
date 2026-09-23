/// Gate: nothing in the **single-threaded** web module may call
/// `std::thread::park`.
///
///     dart run tools/check_no_park.dart [--no-build]
///
/// Unlike `tools/report_parking.dart` — a report over the *threaded* module,
/// where parking is legal and expected on a pool worker — this is a **gate**,
/// and an empty result is the passing state.
///
/// # Why this one is decidable
///
/// A parking report over the *shipped* threaded module cannot be a gate: a pool
/// worker parks on its job queue by design, so every finding needs a human to
/// sort legal from fatal, and the allowlist that would encode that needs a
/// stable answer to "whose function is this" that v0 mangling does not cheaply
/// give (report_parking.dart, §"Why this is not a gate"). The threaded build's
/// decidable gate is `bazel/wasm_block_check`, which sidesteps that by scanning
/// a purpose-built module instead of the shipped one.
///
/// The single-threaded module has no such ambiguity, because it has no threads.
/// `std::thread::spawn` returns `Err(UNSUPPORTED_PLATFORM)`, so there is never a
/// second thread to be unparked *by*. Every call to `park` in this artifact is
/// therefore either dead code or a guaranteed hang. No judgment call, no
/// allowlist, no per-symbol ownership question — the predicate is exact.
///
/// # Why it must be a gate and not a report
///
/// This is the one failure in the system that the platform cannot make loud.
/// Every other way of waiting on single-threaded wasm is already fatal-and-
/// attributable, because std's `no_threads` backends refuse rather than block:
///
/// | operation                    | single-threaded wasm behaviour           |
/// |------------------------------|------------------------------------------|
/// | `Mutex::lock` (recursive)    | panics, "cannot recursively acquire"     |
/// | `Condvar::wait`              | panics, "condvar wait not supported"     |
/// | `thread::sleep`              | panics, "can't sleep"                    |
/// | `thread::spawn`              | `Err(UNSUPPORTED_PLATFORM)`              |
/// | `RwLock` read/write conflict | `rtabort!` — aborts (see the caveat below)|
/// | **`thread::park`**           | **`{}` — returns immediately**           |
///
/// (Verified against the pinned toolchain's own sources, not from memory:
/// `library/std/src/sys/sync/{mutex,condvar,rwlock}/no_threads.rs`,
/// `sys/thread/unsupported.rs`, `sys/sync/thread_parking/unsupported.rs`.)
///
/// A panic is attributable: `frustrate_web_init` installs a hook that ships the
/// message through the `frustrate.panic` import, and the host rethrows it as
/// `BridgePanicException` (runtime/rust/src/lib.rs). So those rows are already
/// loud and already blamed on the right bridged call.
///
/// `park` is the outlier. `unsupported::Parker::park` is literally `{}`, so a
/// caller polling in a loop — `pollster::block_on`, `futures::executor::block_on`,
/// `mpsc::Receiver::recv` — never blocks and never progresses: it spins the
/// browser's only thread at 100% CPU, forever. Nothing observes it, and nothing
/// can: a watchdog timer needs an event loop turn, and the event loop is the
/// thing that is wedged. There is no runtime seam. Link time is the only one
/// left, which is why this exists.
///
/// # What it does not cover
///
/// * `RwLock` misuse aborts via `rtabort!`, which formats its message to stderr
///   and calls `process::abort()`. On wasm32-unknown-unknown stderr is the
///   `unsupported` backend, which discards writes, so the message is lost and
///   the host sees a bare `RuntimeError: unreachable`. Loud, but unattributed.
///   That is a separate defect with a separate fix; it is not a silent hang, so
///   it is not this gate's business.
/// * This scans the debug fixture that the browser suite runs. A user's own
///   `--release` build with LTO could inline `park` into its caller and defeat
///   the direct-call edge. The gate is a build-time property of *this* repo's
///   artifact, not a guarantee carried into every downstream build.
library;

import 'dart:convert';
import 'dart:io';

/// `std::thread::park` and `park_timeout`, as the *free functions* in
/// `std::thread` — v0 mangles the module lowercase (`3std6thread4park`).
///
/// Getting this wrong is the documented way a scanner like this ships green and
/// blind: `report_parking.dart`'s pattern spells the segment `6Thread4park`,
/// which is the `Thread` *type*, and therefore never matched the free function
/// that is the entire silent class. It reported 111 crossings while missing the
/// only one that could hang without a trace. `_selfTest` below exists so that
/// cannot happen again silently.
final _park = RegExp(r'\$_ZN3std6thread(4park|12park_timeout)17h');

final _call = RegExp(r'call (\$\S+)');
final _func = RegExp(r'^  \(func (\$\S+)');

/// A mangled name this artifact always contains. If the disassembly parses but
/// this is absent, the format changed under us and every other result is void.
final _sentinel = RegExp(r'\$_ZN3std');

const _artifact = 'tests/dart_integration/build/test_api.wasm';
const _wasmToolsVersion = '1.256.0';

Future<void> main(List<String> args) async {
  Directory.current = File.fromUri(Platform.script).parent.parent;
  _requireWasmTools();
  _selfTest();

  // Always build, for the reason report_parking.dart records: both web
  // configurations stage to this one path, and scanning whichever module
  // happens to be there is how you get a confident all-clear for the
  // configuration that was not built.
  if (args.contains('--no-build')) {
    stdout.writeln('==> NOT building (--no-build)');
  } else {
    stdout.writeln('==> building the single-threaded fixture');
    final b = await Process.run('dart', [
      'run',
      'tool/build_web_fixture.dart',
    ], workingDirectory: 'tests/dart_integration');
    if (b.exitCode != 0) {
      stderr.write(b.stdout);
      stderr.write(b.stderr);
      exit(b.exitCode);
    }
  }

  final r = await _scan(_artifact);

  // Control 1: the parse worked at all.
  if (!r.sawSentinel || r.functions < 1000) {
    stderr.writeln(
      'check_no_park: parsed ${r.functions} functions and '
      '${r.sawSentinel ? "did" : "did NOT"} see a std symbol. The scan is '
      'broken (wasm-tools output format changed?), so its silence means '
      'nothing.',
    );
    exit(2);
  }
  // Control 2: the right artifact. The threaded module parks legally on its
  // workers; gating it would be wrong, and passing it would be meaningless.
  if (r.atomicWaits > 0) {
    stderr.writeln(
      'check_no_park: $_artifact contains ${r.atomicWaits} '
      'atomic-wait instructions, so it is the THREADED fixture. This gate '
      'only has meaning over the single-threaded one, where no thread exists '
      'to be unparked. Build without --threaded.',
    );
    exit(2);
  }

  if (r.callers.isEmpty) {
    stdout.writeln(
      '==> clean: nothing calls std::thread::park '
      '(${r.functions} functions scanned).',
    );
    return;
  }

  stderr.writeln(
    '\ncheck_no_park: ${r.callers.length} function(s) in the '
    'single-threaded web module call `std::thread::park`:\n',
  );
  for (final f in r.callers.toList()..sort()) {
    stderr.writeln('  ${_readable(f)}');
  }
  stderr.writeln(
    '\nOn this target `park` is a no-op (`Parker::park` is `{}`) '
    'and `spawn` fails, so there is\nnever a thread to do the waking: each of '
    'these spins the browser\'s only thread at 100%\nCPU forever, with no '
    'error, no log and no way for the page to recover.\n\n'
    'If the code is meant to be native-only, say so — `#[bridge(native_only)]`, '
    'or\n`#[bridge(native_only, web = "runtime_fail")]` to keep it on the web '
    'surface as a member\nthat throws instead of hanging. If it needs to run '
    'on web, it must not block: make it\nan `async fn` (the executor yields to '
    'the event loop instead of parking) or move the\nwork to an Actor.',
  );
  exit(1);
}

class _Result {
  final Set<String> callers;
  final int functions;
  final int atomicWaits;
  final bool sawSentinel;
  _Result(this.callers, this.functions, this.atomicWaits, this.sawSentinel);
}

/// Stream the disassembly; never hold it. This module is ~11 MB of wasm and far
/// more as text.
Future<_Result> _scan(String wasm) async {
  final p = await Process.start('wasm-tools', ['print', wasm]);
  unawaited(p.stderr.drain<void>());
  final callers = <String>{};
  var current = '';
  var functions = 0;
  var atomicWaits = 0;
  var sawSentinel = false;
  await for (final line
      in p.stdout.transform(utf8.decoder).transform(const LineSplitter())) {
    final m = _func.firstMatch(line);
    if (m != null) {
      current = m.group(1)!;
      functions++;
      if (!sawSentinel && _sentinel.hasMatch(current)) sawSentinel = true;
      continue;
    }
    if (line.contains('memory.atomic.wait')) atomicWaits++;
    // The parking layer calling itself is not a finding; only who crosses in.
    if (current.isEmpty || _park.hasMatch(current)) continue;
    for (final c in _call.allMatches(line)) {
      if (_park.hasMatch(c.group(1)!)) callers.add(current);
    }
  }
  if (await p.exitCode != 0) {
    stderr.writeln('check_no_park: `wasm-tools print $wasm` failed');
    exit(2);
  }
  return _Result(callers, functions, atomicWaits, sawSentinel);
}

/// The pattern must match a real mangled `std::thread::park` and must not match
/// its neighbours. Four scanners were written for the threaded report and the
/// first passed its own sabotage test, because a pattern that matches nothing
/// looks exactly like a codebase with nothing to find. A gate whose predicate
/// is one regex has to prove that regex bites before it is allowed to say
/// "clean".
void _selfTest() {
  const hits = [
    r'$_ZN3std6thread4park17h6d5c1c9d4f1a2b3cE',
    r'$_ZN3std6thread12park_timeout17h6d5c1c9d4f1a2b3cE',
  ];
  const misses = [
    // The `Thread` type's own methods — `unpark` is the wake side and is
    // harmless; matching it would make the gate fire on correct code.
    r'$_ZN3std6thread6Thread6unpark17h6d5c1c9d4f1a2b3cE',
    // A bridged fixture whose *name* contains "park".
    r'$_ZN8test_api3api9park_sink17h6d5c1c9d4f1a2b3cE',
    r'$_ZN8test_api3api17parked_call_state17h6d5c1c9d4f1a2b3cE',
  ];
  for (final h in hits) {
    if (!_park.hasMatch(h)) {
      stderr.writeln(
        'check_no_park: self-test failed — the pattern does not '
        'match `$h`, so the gate cannot fire and its "clean" is a lie.',
      );
      exit(2);
    }
  }
  for (final m in misses) {
    if (_park.hasMatch(m)) {
      stderr.writeln(
        'check_no_park: self-test failed — the pattern matches '
        '`$m`, which is not a park.',
      );
      exit(2);
    }
  }
}

/// Drop monomorphisation hashes so the output is legible.
String _readable(String s) => s.replaceAll(RegExp(r'17h[a-f0-9]{16}E'), '');

void _requireWasmTools() {
  final r = Process.runSync('wasm-tools', ['--version']);
  final version = '${r.stdout}'.trim();
  if (r.exitCode == 0 && version.contains(_wasmToolsVersion)) return;
  stderr.writeln(
    'check_no_park: needs wasm-tools $_wasmToolsVersion (found: '
    '${version.isEmpty ? 'nothing' : version}).\nThis parses `wasm-tools '
    'print` output, so the version is pinned.\nRun:\n\n'
    '  cargo install wasm-tools --version $_wasmToolsVersion\n',
  );
  exit(2);
}

void unawaited(Future<void> f) {}
