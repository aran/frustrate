/// The externally-backed [BinaryWriter]: the encoder writes into memory the
/// caller owns, instead of into a Dart heap buffer that then has to be copied
/// there.
///
/// On native that caller is the transport, and its memory is the FFI block the
/// call is about to hand Rust — so this constructor is what removes the
/// request's staging memcpy from every native call. The buffer therefore has a
/// lifetime (the call frame), which a heap buffer never had, and two of the
/// three tests below are about that lifetime rather than about bytes.
///
/// Pure Dart, no bridge: this pins the writer's own contract on both platforms.
library;

import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

void main() {
  group('an external buffer is written in place', () {
    test('content lands in the caller\'s buffer, not in a copy', () {
      final store = Uint8List(64);
      final w = BinaryWriter.external(store, (_, __) {
        fail('must not grow: 64 bytes is more than this writes');
      });
      w.writeU32(0xDEADBEEF);
      w.writeU8(7);

      expect(w.length, 5);
      // Read the caller's own buffer — not takeBytes — because the point is
      // that the bytes are already where the caller put the storage.
      expect(
        ByteData.sublistView(store).getUint32(0, Endian.little),
        0xDEADBEEF,
      );
      expect(store[4], 7);
    });

    test('takeBytes is bounded by what was written, as on the heap', () {
      final store = Uint8List(64)..fillRange(0, 64, 0xAA);
      final w = BinaryWriter.external(store, (_, __) => fail('no growth'));
      w.writeU8(1);
      w.writeU8(2);
      expect(w.takeBytes(), Uint8List.fromList([1, 2]));
    });

    test('takeBytes copies, so it cannot outlive storage it does not own', () {
      // On the heap `takeBytes` returns a VIEW, which is right: the writer owns
      // that memory and nothing else will touch it. External storage is the
      // opposite — it belongs to a bridge call, a grow frees it mid-encode and
      // the call frame frees it on the way out. A view handed to user code
      // would be a window onto memory about to disappear, and `close()` cannot
      // help, because a view taken BEFORE the close survives it. That is a
      // silent use-after-free, which is the one failure this class is built to
      // make loud.
      //
      // Here the owner reusing its storage stands in for freeing it: on native
      // the bytes would be gone, and in a Dart test they can be overwritten,
      // which is observable in exactly the same way.
      final store = Uint8List(64);
      final w = BinaryWriter.external(store, (_, __) => fail('no growth'));
      w.writeU8(1);
      w.writeU8(2);
      final taken = w.takeBytes();

      store.fillRange(0, store.length, 0xEE);

      expect(
        taken,
        Uint8List.fromList([1, 2]),
        reason: 'the taken bytes must not alias the owner\'s store',
      );
    });
  });

  group('growth hands the whole job to the owner of the memory', () {
    test('the written prefix survives, and later writes land in the new store', () {
      // A deliberately short hint, so growth is forced rather than hoped for:
      // the writer starts with 8 bytes and is asked for 1 KiB.
      var current = Uint8List(8);
      var growCalls = 0;
      final w = BinaryWriter.external(current, (newCap, written) {
        growCalls++;
        expect(
          newCap,
          greaterThanOrEqualTo(written),
          reason: 'a grow must never ask for less than is already written',
        );
        final next = Uint8List(newCap)
          // The callback copies the prefix ITSELF. On native that copy is
          // between two FFI blocks and the old one is freed immediately after,
          // so the writer cannot be the one holding the stale reference.
          ..setRange(0, written, current);
        current = next;
        return next;
      });

      final payload = Uint8List.fromList(
        List<int>.generate(1024, (i) => (i * 31 + 7) & 0xff),
      );
      w.writeI64(0x0123456789ABCDEF);
      w.writeBytes(payload);

      expect(growCalls, greaterThan(0), reason: 'the test must force a grow');

      // Everything decodes back, across the seam.
      final r = BinaryReader(w.takeBytes());
      expect(r.readI64(), 0x0123456789ABCDEF);
      expect(r.readBytes(), payload);
      r.assertConsumed();
    });

    test(
      'the writer follows the new store: nothing keeps writing into the old',
      () {
        final first = Uint8List(8);
        Uint8List? second;
        final w = BinaryWriter.external(first, (newCap, written) {
          final next = Uint8List(newCap)..setRange(0, written, first);
          second = next;
          return next;
        });
        w.writeU32(0xAAAAAAAA); // fits the first store
        w.writeI64(0x1122334455667788); // forces the grow
        final grown = second;
        expect(grown, isNotNull, reason: 'the test must force a grow');
        expect(
          ByteData.sublistView(grown!).getUint32(0, Endian.little),
          0xAAAAAAAA,
          reason: 'the prefix must have been carried across',
        );
        expect(
          ByteData.sublistView(grown).getInt64(4, Endian.little),
          0x1122334455667788,
          reason:
              'the write that triggered the grow must land in the NEW '
              'store; landing in the old one is a write into memory the owner '
              'has already released',
        );
      },
    );
  });

  group('a writer whose memory is gone is loud, never silent', () {
    // The reason this exists at all: on the heap, a codec hook that stashed the
    // writer past its call was harmless garbage. Backed by a transport block it
    // is a write into freed native memory. That must be a named error at the
    // first byte, not a heap stomp discovered later somewhere else.
    test('a write after close is a StateError naming the writer', () {
      final w = BinaryWriter.external(Uint8List(64), (_, __) => fail('no'));
      w.writeU8(1);
      w.close();
      expect(
        () => w.writeU8(2),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            allOf(contains('frustrate'), contains('closed')),
          ),
        ),
      );
      // Every write funnels through the same guard, so a bulk one is covered
      // too — checked because bulk writes take a different path to the buffer.
      expect(() => w.writeBytes(Uint8List(4)), throwsA(isA<StateError>()));
    });

    test('takeBytes after close is a StateError too', () {
      // Reading is as unsound as writing once the memory is released, and it is
      // the likelier mistake: a stashed writer is far more likely to be *read*
      // later than written to.
      final w = BinaryWriter.external(Uint8List(64), (_, __) => fail('no'));
      w.writeU8(1);
      w.close();
      expect(() => w.takeBytes(), throwsA(isA<StateError>()));
    });

    test('a heap writer can be closed too, and behaves identically', () {
      // The flag is not an external-only concept: the runtime closes whatever
      // writer it handed out, and on web that is a heap writer. Same contract,
      // so no platform grows its own rule.
      final w = BinaryWriter(64);
      w.writeU8(1);
      w.close();
      expect(() => w.writeU8(2), throwsA(isA<StateError>()));
    });

    test('closing twice is not an error', () {
      // The runtime closes in a `finally`; a path that closed early and then
      // unwound must not turn a real failure into a confusing second one.
      final w = BinaryWriter(64);
      w.close();
      expect(w.close, returnsNormally);
    });
  });
}
