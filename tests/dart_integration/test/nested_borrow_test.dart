/// A borrow inside a type: several handles lent by one parameter, and the
/// value borrows (`&str`, `&[u8]`, `&Point`) that ride the same machinery.
///
/// The Dart side of a borrowed container is the ordinary Dart type — `Vec<&Doc>`
/// is `List<Doc>` — so what these check is that the objects arrive, that the
/// count really is the caller's (zero, one and many all work), and that the
/// refusals a container makes possible name the element they are about.
///
/// The locked members here are all dispatched, so a contended call waits
/// rather than refusing; the synchronous try-lock over the same containers is
/// in locked_sync_test.dart.
library;

import 'dart:typed_data';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  group('a confined container', () {
    test('lends every element, at any count', () {
      expect(ledgerSumAll(ls: []), 0);
      expect(ledgerSumAll(ls: [Ledger.new_(total: 3)]), 3);
      expect(
        ledgerSumAll(ls: [Ledger.new_(total: 3), Ledger.new_(total: 4)]),
        7,
      );
    });

    test('two shared borrows of one object are legal', () {
      final l = Ledger.new_(total: 5);
      expect(ledgerSumAll(ls: [l, l]), 10);
    });

    test('mutates through every element', () {
      final a = Ledger.new_(total: 1);
      final b = Ledger.new_(total: 2);
      expect(ledgersBumpAll(ls: [a, b], by: 10), 2);
      expect(a.total(), 11);
      expect(b.total(), 12);
    });

    test('one object twice, mutably, is refused naming both elements', () {
      final l = Ledger.new_(total: 1);
      expect(
        () => ledgersBumpAll(ls: [l, l], by: 1),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('ledgers_bump_all'),
              contains('ls[0]'),
              contains('ls[1]'),
            ),
          ),
        ),
      );
    });

    test('an element aliasing a separate borrowed parameter is refused', () {
      final l = Ledger.new_(total: 1);
      expect(
        () => ledgersBumpTo(ls: [l], target: l),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('ledgers_bump_to'),
              contains('ls[0]'),
              contains('target'),
            ),
          ),
        ),
      );
    });
  });

  /// Dart's own check, which runs before anything is spent. Rust has one too,
  /// but by the time it fires the token is gone — so an element of a lent list
  /// colliding with a consumed handle has to be caught here or the object ends
  /// up with nobody left to free it.
  test('a lent element colliding with a consumed handle is caught before the spend', () {
    final s = Slip.new_();
    s.add(n: 4);
    expect(
      () => slipMergeAll(gone: s.take(), keep: [s]),
      throwsA(
        isA<ArgumentError>().having(
          (e) => e.message,
          'message',
          allOf(
            contains('slip_merge_all'),
            contains('gone'),
            contains('keep[0]'),
          ),
        ),
      ),
    );
    // Nothing was spent, so the handle is still the caller's.
    expect(s.count(), 1);
    final other = Slip.new_();
    expect(slipMergeAll(gone: s.take(), keep: [other]), 1);
    expect(other.count(), 1);
  });

  group('the option and tuple spellings', () {
    test('an Option lends or does not', () {
      expect(ledgerOrZero(l: null), 0);
      expect(ledgerOrZero(l: Ledger.new_(total: 7)), 7);
    });

    test('a tuple lends beside a value', () {
      expect(ledgerScaled(t: (Ledger.new_(total: 6), 3)), 18);
    });

    test('a tuple lends beside a borrowed string', () {
      expect(ledgerNamed(t: (Ledger.new_(total: 4), 'n')), 'n=4');
    });

    test('an Option nested under a list', () {
      expect(
        ledgerSumMaybe(
          ls: [Ledger.new_(total: 2), null, Ledger.new_(total: 5)],
        ),
        7,
      );
    });

    test('an owned value beside a lent handle, inside a list', () {
      expect(
        ledgersNamedAll(
          ls: [(Ledger.new_(total: 1), 'a'), (Ledger.new_(total: 2), 'b')],
        ),
        'a=1,b=2',
      );
    });
  });

  group('a frozen container', () {
    test('lends on the sync arm, spelled as a slice', () {
      final a = Tape.new_(marks: ['x']);
      final b = Tape.new_(marks: ['y', 'z']);
      expect(tapeMarksAll(ts: [a, b]), 3);
      // Uncounted references, so the objects are untouched by the call.
      expect(tapeMarksAll(ts: [a, b]), 3);
    });

    test('lends on the pool arm, holding a clone per element', () async {
      final a = Tape.new_(marks: ['x']);
      final b = Tape.new_(marks: ['y', 'z']);
      expect(await tapeMarksAllAsync(ts: [a, b]), 3);
    });
  });

  group('a locked container', () {
    test('acquires every element and reads them all', () async {
      final a = Vault.new_(balance: 10);
      final b = Vault.new_(balance: 32);
      expect(await vaultsTotal(vs: [a, b]), 42);
    });

    /// The inversion `handle::lock_plan` exists to prevent, over a count the
    /// caller decides: two calls naming the same objects in opposite list
    /// orders acquire the shared locks in the same order regardless, so
    /// neither can hold the other's next lock. Both complete.
    test(
      'two calls naming the same objects in opposite orders both finish',
      () async {
        final a = Vault.new_(balance: 10);
        final b = Vault.new_(balance: 32);
        final both = await Future.wait([
          vaultsTotal(vs: [a, b]),
          vaultsTotalReversed(vs: [b, a]),
        ]);
        expect(both, [42, 42]);
      },
    );

    test('mutates through every element', () async {
      final a = Vault.new_(balance: 1);
      final b = Vault.new_(balance: 2);
      expect(await vaultsBumpAll(vs: [a, b], by: 10), 2);
      expect(await a.readBalance(), 11);
      expect(await b.readBalance(), 12);
    });

    test('a locked receiver plans with the elements beside it', () async {
      final keep = Vault.new_(balance: 1);
      final a = Vault.new_(balance: 2);
      final b = Vault.new_(balance: 3);
      expect(await keep.drainInto(others: [a, b]), 6);
      expect(await a.readBalance(), 0);
      expect(await b.readBalance(), 0);
    });
  });

  group('a set or a map of lent handles', () {
    /// Both stage flat, so the container the body sees is a set or a map of
    /// *references* built after the ids were read — the Dart type is the
    /// ordinary one, because the caller keeps the objects either way.
    test('a set lends every element', () {
      expect(ledgersSumSet(ls: {}), 0);
      final a = Ledger.new_(total: 3);
      final b = Ledger.new_(total: 4);
      expect(ledgersSumSet(ls: {a, b}), 7);
    });

    test('a map lends its values', () {
      expect(
        ledgersSumByName(
          ls: {'a': Ledger.new_(total: 1), 'b': Ledger.new_(total: 2)},
        ),
        'a=1,b=2',
      );
      expect(ledgersSumByName(ls: {}), '');
    });

    test('a map lends its keys, mutably', () {
      final a = Ledger.new_(total: 1);
      final b = Ledger.new_(total: 2);
      expect(ledgersBumpKeys(ls: {a: 10, b: 20}), 2);
      expect(a.total(), 11);
      expect(b.total(), 22);
    });

    /// A Dart `Set` and `Map` key on identity and a generated handle class
    /// does not override `==`, so one handle cannot occupy two positions of
    /// one container — the duplicate the mutable-element rule exists for is
    /// unreachable from here. The Rust check still runs over the runtime
    /// count; what this pins is that the ordinary call is served.
    test('one handle cannot sit twice in one set', () {
      final a = Ledger.new_(total: 5);
      expect({a, a}.length, 1);
      expect(ledgersSumSet(ls: {a, a}), 5);
    });
  });

  group('a bridged trait inside a container', () {
    /// Each element carries its own impl tag, so one call can mix the dyn
    /// handle with a bridged implementor — different registries, different
    /// Rust types behind one `&dyn Tally`.
    test('mixes the dyn handle and an implementor in one confined list', () {
      final dyn_ = newTally(kind: 'step', step: 5);
      dyn_.bump();
      final a = Abacus.new_();
      a.bump();
      expect(tallySum(ts: [dyn_, a]), 15);
      expect(tallySum(ts: []), 0);
      expect(tallySum(ts: [a]), 10);
    });

    test('an Option carries the tag at count zero and one', () {
      expect(tallyOrZero(t: null), -1);
      final a = Abacus.new_();
      a.bump();
      expect(tallyOrZero(t: a), 10);
    });

    test('mutates through every element, tag by tag', () {
      final dyn_ = newTally(kind: 'step', step: 2);
      final a = Abacus.new_();
      expect(tallyBumpAll(ts: [dyn_, a]), 12);
      expect(dyn_.total(), 2);
      expect(a.total(), 10);
    });

    test('one object twice, mutably, is refused naming both elements', () {
      final a = Abacus.new_();
      expect(
        () => tallyBumpAll(ts: [a, a]),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('tally_bump_all'),
              contains('ts[0]'),
              contains('ts[1]'),
            ),
          ),
        ),
      );
    });

    test('a frozen list clones one Arc per element for the pool', () async {
      final g = newGreeter(kind: 'pirate');
      final r = RobotGreeter.build(id: 3);
      expect(await greetEach(gs: [g, r], name: 'ana'), [
        'ahoy ana',
        'BEEP ana [unit 3]',
      ]);
      expect(await greetEach(gs: [], name: 'ana'), <String>[]);
    });

    test(
      'a locked list takes a guard per element, in handle-id order',
      () async {
        final s = await openStore(kind: 'mem');
        await s.put(key: 'a', value: '1');
        final c = CountingStore.fresh();
        await c.put(key: 'x', value: 'y');
        await c.put(key: 'z', value: 'w');
        // Two calls naming the same objects in opposite orders must not invert.
        final both = await Future.wait([
          storeSizes(ss: [s, c]),
          storeSizes(ss: [c, s]),
        ]);
        expect(both, [
          [1, 2],
          [2, 1],
        ]);
      },
    );
  });

  group('value borrows', () {
    test('a list of strings', () {
      expect(joinParts(parts: ['a', 'b'], sep: '-'), 'a-b');
      expect(joinParts(parts: [], sep: '-'), '');
    });

    test('an optional string', () {
      expect(labelOr(name: null), 'none');
      expect(labelOr(name: 'hi'), 'hi');
    });

    test('optional bytes', () {
      expect(byteLenOr(b: null), -1);
      expect(byteLenOr(b: Uint8List.fromList([1, 2, 3])), 3);
    });

    test('a list of borrowed data structs', () {
      expect(
        sumXBorrowed(
          ps: [
            Point(x: 1.5, y: 0, label: null),
            Point(x: 2.5, y: 0, label: null),
          ],
        ),
        4.0,
      );
    });
  });

  group('a borrow in the return', () {
    test('an optional borrowed string is copied into the response', () {
      expect(Tape.new_(marks: []).firstMark(), isNull);
      expect(Tape.new_(marks: ['a', 'b']).firstMark(), 'a');
    });

    test('a list of borrowed strings is copied into the response', () {
      expect(Tape.new_(marks: ['a', 'b']).marksRef(), ['a', 'b']);
    });
  });
}
