/// The request path carries no copy: the Rust body reads the caller's buffer.
///
/// This is the native analogue of `call_entry_count_test.dart`'s wasm-entry
/// counting, and it exists for the same reason: a copy is invisible from the
/// API. Every fixture in the suite round-trips identically whether the bytes
/// were handed straight to the body or memcpy'd into an owned `Vec` first, so
/// nothing else here would notice a copy appearing on the hottest path in the
/// runtime. A pointer identity notices, because it does not count copies — it
/// proves there is nowhere a copy could hide.
///
/// The fixture is `head_ptr(data: &[u8]) -> u64`, which returns the address
/// the body saw for its borrowed argument. The test drives `frustrate_call_sync`
/// **directly over FFI**, so it owns the request buffer and knows exactly where
/// it put the bytes. If the generated decode copies (`read_bytes()` → `to_vec`),
/// the body sees an unrelated heap address; if it borrows, it sees the caller's
/// buffer at the wire offset.
///
/// **Why separate `req` and `out` allocations.** The ABI takes them as two
/// pointers; that the native transport happens to carve both out of one block
/// is its private business, and a test that recomputed the block layout would
/// be pinning a constant it does not own (the same argument
/// `response_slab_test.dart` makes about the slab size). Two allocations state
/// the ABI-level fact and nothing more.
///
/// **The `+ 8`** is the wire's own length prefix: `read_bytes` reads an i64
/// length, then the payload. So the payload begins 8 bytes into the request.
///
/// Native prerequisite: `cargo build -p test_api`.
@TestOn('vm')
library;

import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

/// A member's dispatch id, looked up in the generated table.
///
/// Not a literal: ids are derived from each member's wire facts, so a literal
/// would be a copy of a hash — stale the first time anyone touched the
/// signature, and stale in a way that still *runs*. The lookup throws instead,
/// naming the member it could not find.
///
/// A wrong id could not pass silently either way: an id belonging to no sync
/// member hits the generated `panic!("frustrate: unknown sync fn_id …")` and
/// comes back as a panic envelope, and an id belonging to a *different* member
/// cannot return a value that tracks the caller's buffer address — which is
/// what the assertions below check, at two different addresses.
int _fnId(String member) => frustrateMemberNames.entries
    .firstWhere(
      (e) => e.value == member,
      orElse: () => throw StateError('no bridged member named $member'),
    )
    .key;

final int _headPtrFnId = _fnId('head_ptr');
final int _requestFloorStrFnId = _fnId('request_floor_str');

typedef _CallSyncC = Int32 Function(
  Uint32 fnId,
  Pointer<Uint8> req,
  Uint64 reqLen,
  Pointer<Uint8> out,
  Uint64 outCap,
);
typedef _CallSyncDart = int Function(
  int fnId,
  Pointer<Uint8> req,
  int reqLen,
  Pointer<Uint8> out,
  int outCap,
);

/// Enough for a `u64` answer and its status byte many times over; the fixture's
/// response can never overflow it, so the lease branch is out of scope here.
const int _outCap = 64;

void main() {
  late _CallSyncDart callSync;

  setUpAll(() async {
    await initBridge();
    callSync = DynamicLibrary.open(bridgeLibraryPath())
        .lookupFunction<_CallSyncC, _CallSyncDart>('frustrate_call_sync');
  });

  /// Stage `[i64 len][payload]` in a fresh buffer, dispatch `head_ptr` against
  /// it, and return (the address the body saw, the address the payload is at).
  /// A buffer put in [hold] is not freed here, so the caller can keep it live
  /// across a later probe; the caller frees it.
  ({int seen, int staged}) probe(
    int payloadLength, {
    List<Pointer<Uint8>>? hold,
  }) {
    final reqLen = 8 + payloadLength;
    final req = malloc<Uint8>(reqLen);
    final out = malloc<Uint8>(_outCap);
    try {
      final view = req.asTypedList(reqLen);
      ByteData.sublistView(view).setInt64(0, payloadLength, Endian.little);
      for (var i = 0; i < payloadLength; i++) {
        view[8 + i] = (i * 31 + 7) & 0xff;
      }
      final n = callSync(_headPtrFnId, req, reqLen, out, _outCap);
      expect(
        n,
        greaterThan(0),
        reason:
            'the answer must fit the out buffer; a lease here means the '
            'fixture changed shape',
      );
      final resp = out.asTypedList(n);
      // The envelope is one status byte, then the encoded return value.
      expect(
        resp[0],
        0,
        reason:
            'status byte: a non-zero status is a Rust panic, which for '
            'this fixture means the fn_id is wrong',
      );
      final seen = ByteData.sublistView(resp).getUint64(1, Endian.little);
      return (seen: seen, staged: req.address + 8);
    } finally {
      if (hold == null) {
        malloc.free(req);
      } else {
        hold.add(req);
      }
      malloc.free(out);
    }
  }

  test(
    'a sync &[u8] body reads the caller\'s request buffer, not a copy of it',
    () {
      for (final n in [1, 64, 1024, 64 * 1024]) {
        final r = probe(n);
        expect(
          r.seen,
          r.staged,
          reason:
              'at $n bytes the body saw 0x${r.seen.toRadixString(16)} but '
              'the payload was staged at 0x${r.staged.toRadixString(16)}. A '
              'mismatch means the generated decode copied the request into an '
              'owned value before calling the body.',
        );
      }
    },
  );

  test('the identity tracks the buffer, so it cannot be a coincidence', () {
    // Two live buffers at once, so the allocator cannot hand back the same
    // address twice: an implementation that returned some fixed or stale
    // pointer would satisfy one probe and not both.
    final hold = <Pointer<Uint8>>[];
    final a = probe(4096, hold: hold);
    final b = probe(8192, hold: hold);
    hold.forEach(malloc.free);
    expect(a.seen, a.staged);
    expect(b.seen, b.staged);
    expect(
      a.staged,
      isNot(b.staged),
      reason:
          'the two probes must not share an address, or the pair proves '
          'nothing beyond the single case',
    );
  });

  group('the &str twin actually runs', () {
    // `read_str_borrowed` is the half nothing else reaches: every other
    // borrowed fixture takes `&[u8]`. Compiling is not running, and this one
    // carries an obligation the byte decoder does not — UTF-8 validation.
    test('a borrowed &str decodes to the same string that was sent', () {
      for (final s in ['', 'ascii', 'héllo wörld', '🦀🦀🦀', '\u{feff}bom']) {
        final expected =
            utf8.encode(s).length + (s.startsWith('\u{feff}') ? 1 : 0);
        expect(requestFloorStr(s: s), expected, reason: 'for "$s"');
      }
    });

    test('invalid UTF-8 in a borrowed &str is an attributable codec panic', () {
      // Staged by hand, because the Dart encoder cannot produce invalid UTF-8:
      // a lone 0xFF is not a valid sequence, and the borrowed decoder must
      // reject it exactly as `read_string` does rather than hand a body
      // something unchecked.
      final reqLen = 8 + 1;
      final req = malloc<Uint8>(reqLen);
      // Roomier than [_outCap]: a panic envelope carries the whole message and
      // does not fit the 64 bytes a u64 answer needs, so the small buffer would
      // send this down the lease path and test the wrong thing.
      const panicCap = 4096;
      final out = malloc<Uint8>(panicCap);
      try {
        final view = req.asTypedList(reqLen);
        ByteData.sublistView(view).setInt64(0, 1, Endian.little);
        view[8] = 0xFF;
        final n = callSync(_requestFloorStrFnId, req, reqLen, out, panicCap);
        expect(
          n,
          greaterThan(0),
          reason: 'the panic envelope must fit $panicCap bytes',
        );
        final resp = out.asTypedList(n);
        expect(
          resp[0],
          isNot(0),
          reason: 'a non-zero status: the decode must have panicked',
        );
        expect(
          utf8.decode(resp.sublist(1), allowMalformed: true),
          contains('invalid UTF-8'),
          reason: 'and the panic must name what went wrong',
        );
      } finally {
        malloc.free(req);
        malloc.free(out);
      }
    });
  });

  test('an empty borrowed argument is answerable and never dereferenced', () {
    // The zero-length case has no payload byte to point at; what matters is
    // that it answers at all (`request_slice` short-circuits on an empty
    // request, and a borrowed decode must not read past the length prefix).
    final r = probe(0);
    expect(
      r.seen,
      isNot(0),
      reason:
          'an empty slice still has a non-null dangling-but-aligned '
          'pointer; a zero here would mean the body got a null slice',
    );
  });
}
