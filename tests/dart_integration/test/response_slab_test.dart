/// The sync response slab boundary, swept.
///
/// `frustrate_call_sync` writes its response envelope into a buffer the
/// transport hands it, and falls back to leasing a Rust `Vec` only when the
/// envelope does not fit. That is a size-dependent branch on the hottest path
/// in the runtime, and it is invisible from the API: both outcomes must decode
/// to exactly the same value, so nothing else in the suite would notice if one
/// of them were wrong.
///
/// So this file sweeps *every* response size across the boundary rather than
/// asserting at a computed one. The slab size is a transport-private constant
/// (it may be retuned, and native and web may choose differently), and the
/// envelope adds a status byte plus a length prefix on top of the payload —
/// so a test that computed "the exact boundary" would be pinning arithmetic it
/// does not own. A sweep does not care where the boundary is; it only cares
/// that no size is wrong.
///
/// What a sweep catches that a spot check does not: a copy of `n - 1` bytes, a
/// length returned one too large, an off-by-one in the fits/does-not-fit
/// comparison, a response written at the wrong offset within the block, and a
/// request that moved when the slab was put in front of it.
///
/// Native prerequisite: `cargo build -p test_api`.
/// Web prerequisite: `dart run tool/build_web_fixture.dart`.
library;

import 'dart:typed_data';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// Content that changes with both the index and the length, so a short copy, a
/// shifted copy and a stale-buffer read are all visible rather than
/// accidentally equal.
Uint8List _pattern(int n) =>
    Uint8List.fromList(List<int>.generate(n, (i) => (i * 31 + n) & 0xff));

/// The widest response the sweep covers. Comfortably past any slab a transport
/// would choose for the crossing floor (a few hundred bytes at most — the
/// point of the slab is small envelopes), so both sides of the branch are
/// swept whatever the constant is.
const int _sweepTo = 320;

void main() {
  setUpAll(initBridge);

  test(
    'a byte response round-trips at every size across the slab boundary',
    () {
      for (var n = 0; n <= _sweepTo; n++) {
        final sent = _pattern(n);
        final got = echoBytes(data: sent);
        expect(got.length, n, reason: 'echoBytes length at $n bytes');
        expect(got, sent, reason: 'echoBytes content at $n bytes');
      }
    },
  );

  test('a string response round-trips at every size across the slab boundary', () {
    for (var n = 0; n <= _sweepTo; n++) {
      // The last character differs with n, so a response that decoded one byte
      // short would still be caught at the sizes where UTF-8 is single-byte.
      final sent = String.fromCharCodes(
        List<int>.generate(n, (i) => 0x41 + ((i + n) % 26)),
      );
      expect(echoString(s: sent), sent, reason: 'echoString at $n chars');
    }
  });

  test('a multi-byte-UTF-8 response round-trips across the boundary', () {
    // Two bytes per character, so the payload length and the character count
    // move at different rates and a boundary tuned to one is wrong for the
    // other.
    for (var n = 0; n <= _sweepTo ~/ 2; n++) {
      final sent = 'é' * n;
      expect(echoString(s: sent), sent, reason: 'echoString at $n × 2 bytes');
    }
  });

  test('an empty request and an empty response are both legal', () {
    // The request pointer sits one past the end of the block when the request
    // is empty; nothing may dereference it. `noArgsNoRet` is the narrowest
    // case in the fixture — no request bytes, and a bare status byte back.
    for (var i = 0; i < 100; i++) {
      noArgsNoRet();
    }
    expect(u64Extremes(), isNotEmpty);
  });

  test('an error envelope crosses back through the slab too', () {
    // Non-OK statuses take the same buffer; a slab that only worked for
    // STATUS_OK would still pass every test above.
    expect(
      () => parseNumber(s: 'not a number'),
      throwsA(isA<BridgeException>()),
    );
    expect(parseNumber(s: '  41  '), 41);
  });

  test('leased responses are released, not leaked or double-freed', () {
    // Every response here overflows any plausible slab, so every call takes
    // the lease path and hands a Rust `Vec` back. Freeing one with the wrong
    // capacity corrupts the allocator's bookkeeping rather than failing at the
    // call, so the fence is the repetition: 500 leases with a moving size,
    // whose results must all still be exact.
    for (var i = 0; i < 500; i++) {
      final n = 1024 + (i % 97) * 37;
      final sent = _pattern(n);
      final got = echoBytes(data: sent);
      expect(got.length, n, reason: 'lease $i at $n bytes');
      if (i % 50 == 0) {
        expect(got, sent, reason: 'lease $i content at $n bytes');
      }
    }
  });
}
