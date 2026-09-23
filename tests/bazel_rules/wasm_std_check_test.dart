/// The std-facility check, run against modules a real toolchain produced.
///
/// **Why this exists alongside the scanner's own unit tests.** Those cover the
/// decision logic against hand-built modules: given these names and these
/// imports, what does it say. They cannot cover the assumption the whole check
/// rests on — that a real rustc, building real Rust at the optimization level
/// this repo builds at, still emits the symbols being looked for. That
/// assumption has an expiry date: `SystemTime::now` on this platform is a
/// one-line `panic!`, so it is inline-eligible, and a toolchain that started
/// folding it away would turn the check into a silent no-op with every unit
/// test still green.
///
/// So this test is the tripwire's tripwire. If it goes red, the check stopped
/// working — it did not merely stop being needed.
///
/// The three modules are built by //tests/bazel_rules:probe_shared, one crate
/// under three configurations:
///
/// | module | platform | what it proves |
/// | --- | --- | --- |
/// | `probe.wasm` | wasm32-unknown-unknown | both facilities are caught, and named |
/// | `probe_wasi.wasm` | wasm32-wasip1 | the same source passes where std is served |
/// | `probe_stripped.wasm` | `-Cstrip=symbols` | unreadable is an error, not a pass |
///
/// The middle row is the one that keeps the check honest: it is the same
/// source, so a check that fired on the mere presence of the symbol rather than
/// on the absence of a host would fail it.
///
/// One limit worth stating: `-Cstrip=symbols` leaves the name section *empty*
/// (measured: 0 names), so this fixture exercises only the emptiness half of
/// the scanner's readability guard. The other half — a name section that is
/// full but demangled, which is what a wasm-bindgen post-pass leaves behind —
/// has no fixture here and is covered by the scanner's own unit tests.
@TestOn('vm')
library;

import 'dart:io';

import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

String _rlocation(String path) =>
    Platform.environment.containsKey('TEST_SRCDIR')
    ? Runfiles.create().rlocation('_main/$path')
    : path;

void main() {
  final checker = _rlocation('bazel/wasm_std_check/wasm_std_check');

  /// Runs the real scanner over a built module, exactly as the rule does.
  /// Returns (exit code, stderr) — the marker path is a scratch file nothing
  /// reads.
  (int, String) check(String module) {
    final marker = File(
      '${Directory.systemTemp.createTempSync('std_check').path}/marker',
    );
    final r = Process.runSync(checker, [
      _rlocation('tests/bazel_rules/$module'),
      '//tests/bazel_rules:$module',
      marker.path,
    ]);
    return (r.exitCode, r.stderr as String);
  }

  group('a module whose platform serves nothing', () {
    late int code;
    late String err;
    setUpAll(() {
      final r = check('probe.wasm');
      code = r.$1;
      err = r.$2;
    });

    test('fails the build', () => expect(code, isNot(0), reason: err));

    test('names every facility it reached', () {
      // All three, because a facility the scanner claims to cover but no
      // fixture reaches is a claim nothing checks. `probe.rs` calls each one.
      expect(err, contains('std::time::SystemTime::now'));
      expect(err, contains('println!'));
      expect(err, contains('eprintln!'));
    });

    test('says what each one does today, and they differ', () {
      // The two failure modes are not the same and the message must not
      // flatten them: one traps and is at least reportable through the
      // panic-attribution shim, the other produces nothing at all.
      expect(err, contains('traps at run time'));
      expect(err, contains('no output, no trap, no error'));
    });

    test('names the platform attribute that fixes it', () {
      // The charter's bar for an undeclared hazard: the build error names the
      // opt-in and its contract, not just the symptom.
      expect(err, contains('platform = "@frustrate//bazel:wasm32_wasi"'));
    });

    test('names the target, so the failure is attributable', () {
      expect(err, contains('//tests/bazel_rules:probe.wasm'));
    });

    test(
      'offers the opt-out',
      () => expect(err, contains('std_check = "off"')),
    );
  });

  group('the same source under v0 symbol mangling', () {
    test('is still caught', () {
      // Today this module is *mixed*: std ships precompiled and stays
      // legacy-mangled whatever the consumer asks for, so only the crate's own
      // symbols are v0. What this pins is that the mixture does not confuse the
      // scan — and, if a Rust distribution ever ships a v0-mangled std, that
      // the facilities are still found. The two spellings differ: v0 writes a
      // separator `_` when the identifier itself starts with one, so std's
      // `_print` is `6__print` there and `6_print` under legacy.
      final (code, err) = check('probe_v0.wasm');
      expect(code, isNot(0), reason: 'v0-mangled module not caught');
      expect(err, contains('std::time::SystemTime::now'));
      expect(err, contains('println!'));
      expect(err, contains('eprintln!'));
    });
  });

  group('the same source where std is served', () {
    test('passes', () {
      final (code, err) = check('probe_wasi.wasm');
      expect(
        code,
        0,
        reason:
            'probe_wasi.wasm reaches the same two facilities as '
            'probe.wasm, but on wasm32-wasip1 they are genuinely served. A '
            'failure here means the check is keying on the symbol alone '
            'rather than on the absence of a host behind it, which would '
            'make //bazel:wasm32_wasi unusable.\n$err',
      );
    });
  });

  group('a module whose symbols were stripped', () {
    late int code;
    late String err;
    setUpAll(() {
      final r = check('probe_stripped.wasm');
      code = r.$1;
      err = r.$2;
    });

    test('is an error, not a pass', () {
      // The failure mode the check exists to remove, turned on the check
      // itself: with no names to read, every facility scan comes back empty,
      // which is byte-for-byte what a clean module looks like.
      expect(
        code,
        isNot(0),
        reason: 'a module the scanner cannot read reported success',
      );
      expect(err, contains('cannot verify'));
    });

    test('says what to do about it', () {
      expect(err, contains('strip=symbols'));
      expect(err, contains('std_check = "off"'));
    });
  });
}
