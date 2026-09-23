/// The request is encoded into the transport's own block — end to end.
///
/// `request_pointer_identity_test.dart` proves the *Rust* half of this: given a
/// buffer, the body borrows it instead of copying it. It drives
/// `frustrate_call_sync` directly and stages its own request, which is
/// deliberate — it states an ABI-level fact and pins no transport internals.
///
/// The consequence is that it cannot see the Dart half at all. Reintroduce the
/// old two-phase shape in `NativeRuntime.callSync` — encoder fills a Dart heap
/// buffer, transport memcpy's it into the FFI block — and that test still
/// passes, because Rust would still borrow from the block it was handed. The
/// 84% the request-path work took off the 1 MiB floor would silently come back.
///
/// So this file fences the Dart half, and the two assertions below are chosen
/// because between them there is nowhere for a copy to hide:
///
/// 1. **Where the body's pointer lands.** Through the *public* generated
///    function, the address Rust saw must be `block + slab + 8` for the block
///    the transport allocated — not merely *some* address, and not an address
///    in a buffer of its own. Catches a Rust-side copy on the shipping path,
///    and any change that stops handing Rust the block directly.
///
/// 2. **When the bytes arrive.** Read the block from *inside the encode
///    closure*, before it returns. The payload has to be there already. A
///    staging design cannot pass this: its memcpy happens after the encoder
///    returns, so at this instant the block still holds uninitialised `malloc`
///    memory. This is the assertion that makes the difference structural rather
///    than a matter of which address arithmetic happens to agree.
///
/// Both need the block's address, which is why `NativeRuntime` takes an
/// [Allocator] — the same seam `request_block_lifetime_test.dart` counts
/// through, here recording addresses rather than tallying calls.
///
/// **The `+ 8`** is the wire's own i64 length prefix, as in the ABI-level test.
/// **The slab** is named, not spelled `128`: `frustrateRespSlabBytes` is the
/// contract, and the block layout is `[slab][request]`.
///
/// Native-only: web has no FFI block and no allocator to inject. Its analogue
/// is `call_entry_count_test.dart`, which counts wasm entries.
///
/// Native prerequisite: `cargo build -p test_api`.
@TestOn('vm')
library;

import 'dart:ffi';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';
import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

/// `malloc`, remembering the most recent block. A pass-through for the same
/// reason the counting one is: the bytes really are FFI memory and the call
/// really reaches Rust, so this observes the shipping path rather than a model
/// of it.
class _Recording implements Allocator {
  Pointer<Uint8> last = nullptr;
  int lastBytes = 0;

  @override
  Pointer<T> allocate<T extends NativeType>(int byteCount, {int? alignment}) {
    final p = malloc.allocate<T>(byteCount, alignment: alignment);
    last = p.cast<Uint8>();
    lastBytes = byteCount;
    return p;
  }

  @override
  void free(Pointer<NativeType> pointer) => malloc.free(pointer);
}

void main() {
  final recording = _Recording();

  setUpAll(() {
    final rt = NativeRuntime(
      DynamicLibrary.open(bridgeLibraryPath()),
      allocator: recording,
    );
    Frustrate.install(
      rt,
      source: recording,
      description: 'request_no_staging_copy_test\'s recording allocator',
    );
    checkFrustrateSchema();
  });

  /// Where the request payload begins, for the block a call just used.
  int expectedPayloadAddress() =>
      recording.last.address + frustrateRespSlabBytes + 8;

  test('the body reads the transport\'s block, through the public function', () {
    // Two sizes, because a single address proves nothing about arithmetic that
    // happens to agree once. The small one fits the writer's 64-byte floor; the
    // large one is past it, so the hint path allocates to fit.
    for (final n in [8, 4096]) {
      final seen = headPtr(data: Uint8List(n));
      expect(
        seen,
        BigInt.from(expectedPayloadAddress()),
        reason:
            'at $n bytes the body must see the transport\'s own block at '
            'slab+8, not a buffer it or the transport made',
      );
    }
  });

  test('a grown request is still the transport\'s block', () {
    // `headPtr`'s hint is exact, so the ordinary path never grows. Drive the
    // grow deliberately: hint small, write large. The frame republishes `block`
    // and frees whichever is current, and the recording allocator's `last` is
    // the block that survived to the call.
    final payload = Uint8List(64 * 1024);
    final r = Frustrate.instance.callSync(11, 8, (w) {
      w.writeBytes(payload);
    });
    final seen = r.readU64();
    r.assertConsumed();
    expect(
      seen,
      BigInt.from(expectedPayloadAddress()),
      reason:
          'after a grow the body must read the NEW block, at the same '
          'offset — a stale pointer here would be a use-after-free',
    );
  });

  test('the encoder writes into the block before it returns', () {
    // The teeth. A staging design fills a heap buffer here and memcpy's it
    // afterwards, so at the moment this closure runs the block would still hold
    // whatever `malloc` returned.
    //
    // The payload is a non-repeating pattern rather than a constant so that
    // uninitialised memory cannot pass by luck, and it is read back through the
    // block pointer rather than through the writer, which would prove nothing.
    final payload = Uint8List.fromList(
      List<int>.generate(512, (i) => (i * 37 + 11) & 0xff),
    );

    var checked = false;
    final r = Frustrate.instance.callSync(11, 8 + payload.length, (w) {
      w.writeBytes(payload);

      final block = recording.last;
      expect(
        block,
        isNot(nullptr),
        reason: 'the transport allocates before it calls the encoder',
      );
      final inBlock = (block + frustrateRespSlabBytes + 8).asTypedList(
        payload.length,
      );
      expect(
        inBlock,
        payload,
        reason:
            'the encoder must have written straight into the block; if '
            'these differ the bytes are staged somewhere else and copied in '
            'after this closure returns',
      );
      checked = true;
    });
    r.readU64();
    r.assertConsumed();
    expect(checked, isTrue, reason: 'the encode closure must have run');
  });
}
