/// The request block has exactly one owner, and the owner is the call frame.
///
/// The native transport allocates the FFI block *before* the generated encoder
/// runs, because the encoder writes the request straight into it. That opens a
/// window the two-phase protocol never had: a throw between the allocation and
/// the call. It is not hypothetical — `writeU64`'s range check, `writeChar`'s
/// scalar check and any user `BytesCodec.toBytes` can all throw mid-encode, and
/// with a caller-built writer over transport memory there would be no frame
/// left holding the block.
///
/// So the contract is "one allocation and one free per call, whatever happens
/// in between", and the only way to observe it is to count. `NativeRuntime`
/// takes an [Allocator] for that reason and no other; every shipping
/// configuration passes `malloc`.
///
/// **This test has teeth, checked rather than assumed.** Deleting the `finally`
/// in `NativeRuntime.callSync` makes the throwing case report
/// `1 allocation, 0 frees`; deleting it in `_issueAsync` does the same for the
/// async case. Both were run before this file was committed.
///
/// Native-only: it counts native allocations, which is not a thing web has.
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

/// `request_floor`'s dispatch id, looked up rather than written down: ids are
/// derived from each member's wire facts, so a literal would be a copy of a
/// hash that still dispatches somewhere after the member changes. This test
/// only needs *a* sync member taking bytes — the block accounting is the
/// subject — but it needs the right one, or the request never reaches a body.
final int _requestFloorFnId = frustrateMemberNames.entries
    .firstWhere(
      (e) => e.value == 'request_floor',
      orElse: () => throw StateError('no bridged member named request_floor'),
    )
    .key;

/// `malloc`, counted. Deliberately a pass-through rather than a fake heap: the
/// bytes really are FFI memory and the calls really reach Rust, so this counts
/// the shipping path instead of a model of it.
class _Counting implements Allocator {
  int allocs = 0;
  int frees = 0;

  @override
  Pointer<T> allocate<T extends NativeType>(int byteCount, {int? alignment}) {
    allocs++;
    return malloc.allocate<T>(byteCount, alignment: alignment);
  }

  @override
  void free(Pointer<NativeType> pointer) {
    frees++;
    malloc.free(pointer);
  }

  void reset() {
    allocs = 0;
    frees = 0;
  }
}

void main() {
  final counting = _Counting();

  setUpAll(() {
    // A second runtime over the same library, installed for this isolate. The
    // schema check runs against it exactly as `initBridge` would.
    final rt = NativeRuntime(
      DynamicLibrary.open(bridgeLibraryPath()),
      allocator: counting,
    );
    Frustrate.install(
      rt,
      source: counting,
      description: 'request_block_lifetime_test\'s counting allocator',
    );
    checkFrustrateSchema();
  });

  setUp(counting.reset);

  test('a sync call allocates one block and frees it', () {
    expect(requestFloor(data: Uint8List(1024)), 1024);
    expect(counting.allocs, 1, reason: 'one block per sync call');
    expect(
      counting.frees,
      1,
      reason: 'and the frame that took it gives it back',
    );
  });

  test('a sync call whose encoder throws still frees its block', () {
    // `writeU64` rejects a value outside [0, 2^64) — a real, reachable encode
    // failure, raised while the transport holds the block. `u64Extremes` is
    // the wrong shape to drive it, so drive the writer contract directly
    // through a member that takes a u64.
    expect(
      () => Frustrate.instance.callSync(0, 8, (w) {
        w.writeU64(BigInt.from(-1));
      }),
      throwsA(isA<ArgumentError>()),
      reason: 'the encode failure must reach the caller unchanged',
    );
    expect(counting.allocs, 1);
    expect(
      counting.frees,
      1,
      reason:
          'a throw between the allocation and the call must not strand '
          'the block — there is no other frame that could free it',
    );
  });

  test('a growing sync request still nets one live block', () {
    // The hint is deliberately far too small, so the writer grows: each growth
    // allocates a new block and frees the old one, and the count must still
    // balance. (Growth is 1 alloc + 1 free on top of the original pair.)
    expect(
      Frustrate.instance.callSync(_requestFloorFnId, 8, (w) {
        w.writeBytes(Uint8List(64 * 1024));
      }),
      isNotNull,
    );
    expect(
      counting.allocs,
      counting.frees,
      reason: 'every block handed out was handed back',
    );
    expect(
      counting.allocs,
      greaterThan(1),
      reason: 'the test must force a grow',
    );
  });

  test('an async call allocates one block and frees it', () async {
    expect(await requestFloorAsync(data: Uint8List(1024)), 1024);
    expect(counting.allocs, 1);
    expect(counting.frees, 1);
  });

  test('an async call whose encoder throws frees its block and fails its future', () async {
    // And it must fail through the FUTURE, not synchronously: the encode moved
    // inside the issue path, and `callAsync`'s never-throws-synchronously
    // contract has to survive that move.
    late final Future<BinaryReader> f;
    expect(
      () => f = Frustrate.instance.callAsync(10, 8, (w) {
        w.writeU64(BigInt.from(-1));
      }),
      returnsNormally,
    );
    await expectLater(f, throwsA(isA<ArgumentError>()));
    expect(counting.allocs, 1);
    expect(counting.frees, 1);
  });

  test('a writer stashed past its call is loud, not a write into freed memory', () {
    late final BinaryWriter escaped;
    Frustrate.instance.callSync(_requestFloorFnId, 8, (w) {
      escaped = w;
      w.writeBytes(Uint8List(0));
    });
    // The block is freed by now. Anything this writer does next would be a use
    // of released native memory, so it is a named error instead.
    expect(
      () => escaped.writeU8(1),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('closed'),
        ),
      ),
    );
    expect(() => escaped.takeBytes(), throwsA(isA<StateError>()));
  });
}
