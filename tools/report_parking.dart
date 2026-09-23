/// Report every function in the threaded-wasm build that crosses into std's
/// parking layer.
///
///     dart run tools/report_parking.dart [--no-build]
///
/// **This is a report, not a gate.** It always exits 0. Read the last section
/// for why, and for what would have to be solved to make it one.
///
/// # The property it illuminates
///
/// On threaded wasm every `std::sync::{Mutex, RwLock, Once, OnceLock, Condvar}`
/// and `thread::park` bottoms out in `memory.atomic.wait32`, and the
/// specification bars `Atomics.wait` on the browser main thread: a contended
/// acquisition there is
/// `RuntimeError: Atomics.wait cannot be called in this context`. That is why
/// the runtime's own registries use `spin::SpinLock` (runtime/rust/src/spin.rs)
/// — see that module for the full argument.
///
/// The argument is source-level and therefore rots. This looks at the artifact.
///
/// # Why the artifact
///
/// A source lint sees only this crate's own text. It cannot see a parking
/// primitive arriving through a dependency, through a `std` path taken
/// implicitly, or — the case that actually happened — through a module whose
/// `#[cfg]` was misread as excluding it. `callback::returning`'s
/// `OnceLock<Mutex<HashMap<..>>>` compiled into every wasm build and was
/// contended between the browser main thread and a pool worker. It survived
/// being read by eye twice in one sitting. This scan surfaced it immediately,
/// which is the entire case for looking at the binary.
///
/// # Why "crossings", not reachability or absence
///
/// *Absence* is the wrong question **for this module**: the shipped threaded
/// artifact must contain waits. A pool worker parks on the job queue by design
/// (`pool.rs`: "Blocking is legal here (worker thread)"), and
/// `__wasm_init_memory` is the linker's own startup coordination. Absence is
/// decidable over a *purpose-built* artifact instead — see
/// `bazel/wasm_block_check`, which links only a claimed member's reachable code
/// and requires zero waits.
///
/// *Backward* reachability is the direction that carries no information:
/// everything that reaches a wait over direct calls is 8203 of 13125 functions,
/// because `Once::call` sits under every lazy static in std. True and useless —
/// but note the defect is the traversal direction, not reachability itself.
/// Forward reachability from the handful of main-thread entry exports hits only
/// 6 of the module's 10 wait-containing functions, each with a short witness
/// path, because the finding set is bounded by the wait *leaves* rather than by
/// the reachable set.
///
/// What this tool prints is the boundary — who calls into std's parking layer
/// from outside it. That stays useful as a *survey* of the shipped module, which
/// is a different question from the per-member proof `wasm_block_check` gives.
///
/// # Why this is not a gate
///
/// Turning this report into a gate needs an allowlist, and an allowlist needs a
/// stable answer to "whose function is this". Rust's v0 mangling does not give
/// one cheaply:
///
///   * Generic code is monomorphised into the *instantiating* crate, so
///     `frustrate::pool::threaded::spawn` can appear inside `test_api`'s
///     symbols. Attributing by crate name mis-files it.
///   * Generated glue is literally named `test_api::frustrate_generated::…`,
///     so a substring test for the runtime's own code matches the fixture's.
///   * A bridge author's `Locked` type IS `Arc<RwLock<T>>` by design, so its
///     *async* accessors take a blocking `write()` — on a pool worker, where
///     that is legal. Those are the model working as specified, governed by
///     FR0007/FR0008 at codegen time rather than here, and they would dominate
///     any allowlist while churning with the fixture. (Their *sync* accessors
///     no longer appear at all: codegen emits `try_write`, which cannot park,
///     and this scan excludes the `try_` family for exactly that reason.)
///
/// Resolving that means real demangling and a considered definition of code
/// ownership. A pass/fail here would be a gate that cannot fail, which is worse
/// than no gate: four drafts of this file were written, and the first *passed
/// its own sabotage test* because an unanchored `8contended` pattern matched
/// nothing (the real symbol is `14lock_contended`).
///
/// The gate that *is* decidable took the other route entirely — it changes the
/// artifact instead of the predicate. `bazel/wasm_block_check` links a check
/// module whose only root is one `#[bridge(no_block)]` member, lets lld's
/// conservative GC do the reachability (so `call_indirect` is over-approximated
/// rather than guessed at), and asserts zero waits. No ownership question
/// arises, because nothing legal is in the module to begin with.
///
/// So: read this output as a survey, and do not automate it.
library;

import 'dart:convert';
import 'dart:io';

/// std's parking layer: `std::sys::sync::*` (mutex, rwlock, once, condvar,
/// thread_parking), the wasm futex beneath it, the public `sync::{poison,
/// nonpoison}` wrappers over it, and `thread::park*`.
///
/// Treated as one unit rather than as individual functions, because its
/// internals call each other — `RwLock::write` → `write_contended` →
/// `futex_wait` — and auditing those edges would only report std talking to
/// itself. What matters is who crosses in.
/// `6thread(4park|12park_timeout)` is the lowercase *module* segment, i.e. the
/// free functions `std::thread::park` / `park_timeout`. The capitalised
/// `6Thread` segment beside it is the `Thread` type, whose `park`-named members
/// are a different set — and for most of this file's life only the capitalised
/// spelling was here, so the scan never once matched the free function that is
/// the whole silent-hang class. It reported 111 crossings while blind to the
/// only symbol that can wedge a browser without a trace. `tools/check_no_park.dart`
/// gates on that symbol specifically and self-tests its pattern; this one now at
/// least reports it.
final _layer = RegExp(
  r'(3sys4sync|3sys3pal4wasm5futex'
  r'|4sync6poison|4sync9nonpoison'
  r'|6[Tt]hread(4park|12park_timeout))',
);

final _call = RegExp(r'call (\$\S+)');

/// The non-blocking half of the sync layer. `try_lock`, `try_read` and
/// `try_write` are a compare-exchange and a branch — they never park, and they
/// are how a `Locked` type is reached synchronously on web (codegen emits
/// exactly these, `emit_rust.rs`), so counting them would be wrong.
///
/// Measured effect: 112 crossings to 111. Excluding them is *correct* and it is
/// nearly all it sounds like it should do — a first draft of this comment
/// claimed it would stop `Locked` accessors dominating the list, and the number
/// says otherwise. They stay listed through other non-parking members of the
/// same namespace (`Arc<RwLock<T>>` drop glue, `Once::new`, `is_completed`,
/// unlock paths), which this scan still cannot tell apart from a real wait.
/// So read the output as *candidates that touch the sync layer*, not as a set
/// that can each park. Narrowing it further is the same demangling problem the
/// gate needs.
final _nonBlocking = RegExp(r'[0-9]+try_(lock|read|write)');
final _func = RegExp(r'^  \(func (\$\S+)');

const _artifact = 'tests/dart_integration/build/test_api.wasm';
const _wasmToolsVersion = '1.256.0';

Future<void> main(List<String> args) async {
  Directory.current = File.fromUri(Platform.script).parent.parent;
  _requireWasmTools();

  // ALWAYS build by default, and this is not caution. `test_api.wasm` is one
  // path shared by both web configurations: the single-threaded browser suite
  // writes a single-threaded module there, which crosses into the parking layer
  // nowhere at all because it has no threads. Scanning whatever happens to be
  // staged reports "all clear" for the configuration that cannot fail. That
  // mistake was made while writing this file, and the positive control below is
  // what caught it.
  if (args.contains('--no-build')) {
    stdout.writeln(
      '==> NOT building (--no-build): $_artifact may be the '
      'single-threaded fixture, in which case this shows nothing.',
    );
  } else {
    stdout.writeln('==> building the threaded fixture');
    final b = await Process.run('dart', [
      'run',
      'tool/build_web_fixture.dart',
      '--threaded',
    ], workingDirectory: 'tests/dart_integration');
    if (b.exitCode != 0) {
      stderr.write(b.stdout);
      stderr.write(b.stderr);
      exit(b.exitCode);
    }
  }

  final found = await _scan(_artifact);

  // Positive control. A text parser over a disassembler's output can "pass" by
  // matching nothing, and an empty result must never be indistinguishable from
  // a broken scan. A threaded build always crosses into the parking layer
  // somewhere, so zero means the scan broke — not that the code got safer.
  if (found.isEmpty) {
    stderr.writeln(
      'report_parking: no crossings at all, which is not '
      'plausible for a threaded build.\nThe scan is broken (wasm-tools '
      'output format changed?), or $_artifact is the single-threaded '
      'fixture.',
    );
    exit(2);
  }

  stdout.writeln(
    '==> ${found.length} function(s) call into std\'s parking '
    'layer:\n',
  );
  for (final f in found.toList()..sort()) {
    stdout.writeln('  ${_readable(f)}');
  }
  stdout.writeln(
    '\nThese touch the sync layer; the ones that take a *blocking* '
    'acquisition park a thread,\nand on the browser main thread that traps. '
    'The list does not separate the two — see\nthe note on `_nonBlocking`.\n\n'
    'NOT expected here: anything under `frustrate::rwlock`. The `Locked` '
    'model\'s lock is the\nruntime\'s own and its waiter list is a '
    '`spin::SpinLock` on this build, which is what puts\na synchronous '
    'locked member on the browser main thread at all — a hit there means '
    'that\nregressed. Expected: std internals reached from native-only '
    'paths. What to look for\ninstead: a hand-rolled `static '
    'Mutex`/`OnceLock` in bridge code, which is the one case\ncodegen '
    'cannot see. Shared mutable state wants an Actor, and the runtime\'s '
    'own registries\nwant `spin::SpinLock`.',
  );
}

/// Stream the disassembly; never hold it. The threaded module is ~19 MB of
/// wasm and far more as text, so this tracks only the current function name.
Future<Set<String>> _scan(String wasm) async {
  final p = await Process.start('wasm-tools', ['print', wasm]);
  unawaited(p.stderr.drain<void>());
  final found = <String>{};
  var current = '';
  await for (final line
      in p.stdout.transform(utf8.decoder).transform(const LineSplitter())) {
    final m = _func.firstMatch(line);
    if (m != null) {
      current = m.group(1)!;
      continue;
    }
    if (current.isEmpty || _layer.hasMatch(current)) continue;
    for (final c in _call.allMatches(line)) {
      final callee = c.group(1)!;
      if (_layer.hasMatch(callee) && !_nonBlocking.hasMatch(callee)) {
        found.add(current);
      }
    }
  }
  if (await p.exitCode != 0) {
    stderr.writeln('report_parking: `wasm-tools print $wasm` failed');
    exit(2);
  }
  return found;
}

/// Drop monomorphisation hashes so the output is legible. Not a demangler —
/// just enough that a human can tell which function is named.
String _readable(String s) => s.replaceAll(RegExp(r'Cs[A-Za-z0-9]{16}_'), '');

void _requireWasmTools() {
  final r = Process.runSync('wasm-tools', ['--version']);
  final version = '${r.stdout}'.trim();
  if (r.exitCode == 0 && version.contains(_wasmToolsVersion)) return;
  stderr.writeln(
    'report_parking: needs wasm-tools $_wasmToolsVersion (found: '
    '${version.isEmpty ? 'nothing' : version}).\nThis parses `wasm-tools '
    'print` output, so the version is pinned — a format change between\n'
    'releases would quietly change what it reports. Run:\n\n'
    '  cargo install wasm-tools --version $_wasmToolsVersion\n',
  );
  exit(2);
}

void unawaited(Future<void> f) {}
