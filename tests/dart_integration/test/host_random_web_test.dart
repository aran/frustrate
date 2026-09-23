/// `frustrate::random::fill_bytes` — the host CSPRNG reached from Rust on a
/// **stock** wasm32 module: no custom std, no wasm-bindgen, no `platform`
/// change, and no import the bridge declared itself.
///
/// The claim that needs a browser is that the import is actually served here,
/// on the ordinary fixture. `frustrate.fill_random` is put in the import object
/// unconditionally at every instantiation site, so a module that merely
/// declares it links — and a stubbed or missing CSPRNG fails quietly: a
/// hand-stubbed `getRandomValues` yielded zeros with no error anywhere. So the
/// assertions are the ones that tell "reached the CSPRNG" from "returned a
/// constant": not all zero, and two draws differ.
///
/// `@TestOn('browser')`: `frustrate::random` does not exist off wasm (native
/// has `getrandom`), so `hostRandomBytes` there returns its zeroed buffer and
/// has nothing to say.
@TestOn('browser')
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('a stock wasm module can reach the host CSPRNG', () {
    final bytes = hostRandomBytes(n: 32);
    expect(bytes, hasLength(32));
    expect(
      bytes.every((b) => b == 0),
      isFalse,
      reason:
          'all-zero is what an unserved or stubbed getRandomValues '
          'returns — the failure this test exists for',
    );
  });

  test('two draws differ', () {
    // "Not the sentinel" alone would pass for a source stuck at some other
    // constant; this is the other half. 32 bytes, so a collision from a real
    // CSPRNG is not a flake anyone will ever see.
    expect(hostRandomBytes(n: 32), isNot(equals(hostRandomBytes(n: 32))));
  });

  test('an empty request never reaches the host', () {
    // `as_mut_ptr` on an empty slice is dangling-but-aligned, and the host
    // would build a typed-array view over it — a `RangeError`, not a no-op.
    // The Rust side returns early; this is what says so from outside.
    expect(hostRandomBytes(n: 0), isEmpty);
  });

  test('a request larger than the host chunk is filled throughout', () {
    // crypto.getRandomValues caps at 65536 bytes a call and the glue loops.
    // A loop that filled only the first chunk would leave a zero tail, which
    // is precisely a partially-filled buffer silently weakening its caller.
    final bytes = hostRandomBytes(n: 65536 + 4096);
    expect(bytes, hasLength(65536 + 4096));
    expect(
      bytes.sublist(65536).every((b) => b == 0),
      isFalse,
      reason: 'the tail past one chunk must be filled too',
    );
  });
}
