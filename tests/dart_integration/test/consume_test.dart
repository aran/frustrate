/// Consuming a handle: `take()` hands the Rust object to the call, and the
/// handle is spent.
///
/// Every model is here, because what "spent" costs differs by model and only
/// one of the four can refuse. Confined and Actor cannot: a confined object has
/// one isolate and synchronous calls, and an actor's consuming call queues
/// behind everything already sent. Frozen and Locked are shared, so a call
/// still running holds an `Arc` clone the take can see — that case lives in
/// `consume_contention_test.dart`, which needs a call parked on Dart to
/// produce it.
///
/// The refusals in this file are all thrown by the *Dart* side, before
/// anything is spent, so nothing here traps and the file may hold many calls
/// (see `locked_alias_test.dart` for the one-call-per-file rule and why).
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

Slip slipOf(List<int> ns) {
  final s = Slip.new_();
  for (final n in ns) {
    s.add(n: n);
  }
  return s;
}

void main() {
  setUpAll(initBridge);

  group('confined', () {
    test('a consuming receiver takes the object and spends the handle', () {
      final s = slipOf([1, 2, 3]);
      expect(s.take().intoTotal(), 6);
      expect(s.isDisposed, isTrue, reason: 'the call took the object');
      expect(() => s.count(), throwsA(isA<StateError>()));
    });

    test('the message names the member that took it', () {
      final s = slipOf([4]);
      s.take().intoTotal();
      // Under asserts (which `dart test` runs with) the refusal says which
      // member took the object, because "used after dispose()" would send the
      // reader looking for a dispose() they never wrote.
      expect(
        () => s.count(),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('into_total'),
          ),
        ),
      );
    });

    test('dispose() after a consume is a no-op, not a double free', () {
      final s = slipOf([7]);
      expect(s.take().intoTotal(), 7);
      s.dispose();
      s.dispose();
    });

    test('an unused token leaves the handle untouched', () {
      final s = slipOf([5]);
      s.take(); // never passed anywhere
      expect(s.count(), 1);
      s.dispose();
    });

    test('`self: Box<Self>` consumes too', () {
      final s = slipOf([9, 10]);
      expect(s.take().intoFirst(), 9);
      expect(s.isDisposed, isTrue);
    });

    test('a free function takes a handle by value', () {
      final s = slipOf([2, 3]);
      expect(ledgerTotal(l: s.take()), 5);
      expect(s.isDisposed, isTrue);
    });
  });

  group('containers', () {
    test('an Option, present and absent', () {
      final s = slipOf([1, 1]);
      expect(ledgerTotalOpt(l: s.take()), 2);
      expect(s.isDisposed, isTrue);
      expect(ledgerTotalOpt(l: null), -1);
    });

    test('a list takes every element', () {
      final a = slipOf([1]);
      final b = slipOf([2, 3]);
      expect(ledgerTotalAll(ls: [a.take(), b.take()]), 6);
      expect(a.isDisposed, isTrue);
      expect(b.isDisposed, isTrue);
    });

    test('an empty list takes nothing', () {
      expect(ledgerTotalAll(ls: []), 0);
    });

    test('a tuple element beside a value', () {
      final s = slipOf([2, 3]);
      expect(ledgerTotalTagged(t: (s.take(), 10)), 50);
      expect(s.isDisposed, isTrue);
    });

    test('a nested Option<Vec<..>>', () {
      final a = slipOf([4]);
      expect(ledgerTotalNested(ls: [a.take()]), 4);
      expect(a.isDisposed, isTrue);
      expect(ledgerTotalNested(ls: null), -1);
    });

    /// An `Option` inside an `Option` is `FrOption`, not a second nullable —
    /// `T??` is not Dart, and `Some(None)` has to stay distinct from `None`.
    /// The consume walk has to agree with the ordinary encoder about that or
    /// the generated Dart does not compile; running this is the check.
    /// Two containers of one kind at one depth. The encoders name their
    /// locals by depth, so both halves of each pair land at the same name in
    /// the same Dart scope unless each is emitted inside a block of its own —
    /// and the Dart compiler that built this file is what proves they are.
    test('two Option parameters, and two Option fields', () {
      final a = slipOf([1, 2]);
      final b = slipOf([4]);
      expect(slipTotalPair(a: a.take(), b: b.take()), 7);
      expect(a.isDisposed, isTrue);
      expect(slipTotalPair(a: null, b: null), -2);
      expect(joinNotes(n: TwoNotes(first: 'x', second: null)), 'x/');
    });

    test('an Option inside an Option keeps all three states', () {
      final a = slipOf([6]);
      expect(slipTotalNestedOpt(l: FrSome(a.take())), 6);
      expect(a.isDisposed, isTrue);
      expect(slipTotalNestedOpt(l: const FrNone()), -1);
      expect(slipTotalNestedOpt(l: null), -1);
    });

    /// A set and a map stage flat and are rebuilt after every take, so what
    /// the Rust container collapses it collapses on the *objects*. The Dart
    /// side of a taken set is a set of tokens, with identity equality.
    test('a set takes every element', () {
      final a = slipOf([1]);
      final b = slipOf([2, 3]);
      expect(slipTotalSet(ss: {a.take(), b.take()}), 6);
      expect(a.isDisposed, isTrue);
      expect(b.isDisposed, isTrue);
      expect(slipTotalSet(ss: {}), 0);
    });

    /// Two distinct objects that compare equal are one element of a Rust
    /// `HashSet`. Both are still adopted — the loser is dropped, not leaked —
    /// and the sum counts one, which is what the signature asked for.
    test('a set collapses equal objects, after taking both', () {
      final a = slipOf([7]);
      final b = slipOf([7]);
      expect(slipTotalSet(ss: {a.take(), b.take()}), 7);
      expect(a.isDisposed, isTrue);
      expect(b.isDisposed, isTrue);
    });

    test('a map takes its values, and its keys', () {
      final a = slipOf([1, 2]);
      final b = slipOf([4]);
      expect(slipTotalByName(ss: {'a': a.take(), 'b': b.take()}), 'a=3,b=4');
      expect(a.isDisposed, isTrue);

      final k = slipOf([5]);
      expect(slipWeighted(ss: {k.take(): 3}), 15);
      expect(k.isDisposed, isTrue);
    });

    test('a frozen element takes inside a set', () {
      final t = Tape.new_(marks: ['x', 'y']);
      expect(tapeMarksSet(ts: {t.take()}), 2);
      expect(t.isDisposed, isTrue);
    });

    test('one handle twice in a map is refused, naming the entry', () {
      final a = slipOf([1]);
      expect(
        () => slipTotalByName(ss: {'x': a.take(), 'y': a.take()}),
        throwsA(
          isA<ArgumentError>().having(
            (e) => e.message,
            'message',
            allOf(contains('ss[0].value'), contains('ss[1].value')),
          ),
        ),
      );
      expect(a.count(), 1);
      a.dispose();
    });

    /// A container mixing two models: each handle's duplicate-check entry has
    /// to be built for *its* container, because only a `Box`-held object may
    /// forgive a zero-sized type.
    test('a tuple mixing a confined and a frozen handle', () {
      final s = slipOf([1, 2]);
      final t = Tape.new_(marks: ['x']);
      expect(slipAndTape(t: (s.take(), t.take())), 3);
      expect(s.isDisposed, isTrue);
      expect(t.isDisposed, isTrue);
    });
  });

  group('a consume beside another acquisition', () {
    test('one handle for both is refused before anything is spent', () {
      final s = slipOf([1, 2]);
      expect(
        () => ledgerMergeInto(keep: s, gone: s.take()),
        throwsA(
          isA<ArgumentError>().having(
            (e) => e.message,
            'message',
            allOf(contains('keep'), contains('gone')),
          ),
        ),
      );
      // The refusal is the point: both handles are exactly as they were.
      expect(s.count(), 2);
      s.dispose();
    });

    test('two elements of one list, named by index', () {
      final s = slipOf([1]);
      expect(
        () => ledgerTotalAll(ls: [s.take(), s.take()]),
        throwsA(
          isA<ArgumentError>().having(
            (e) => e.message,
            'message',
            allOf(contains('ls[0]'), contains('ls[1]')),
          ),
        ),
      );
      expect(s.count(), 1);
      s.dispose();
    });

    /// A member that both locks and consumes: the guard plan must not count
    /// the consumed handle, or the emitted glue has no guard bound at all.
    test('a locked handle is guarded beside another that is taken', () async {
      final keep = Jar.new_(coins: 10);
      final gone = Jar.new_(coins: 5);
      expect(await keep.absorb(other: gone.take()), 15);
      expect(gone.isDisposed, isTrue);
      expect(keep.take().intoCoins(), 15);
    });

    test('the same locked handle for both is refused, nothing spent', () async {
      final j = Jar.new_(coins: 7);
      await expectLater(
        j.absorb(other: j.take()),
        throwsA(
          isA<ArgumentError>().having(
            (e) => e.message,
            'message',
            allOf(contains('self'), contains('other')),
          ),
        ),
      );
      expect(j.take().intoCoins(), 7);
    });

    test('two distinct handles merge', () {
      final keep = slipOf([1, 2]);
      final gone = slipOf([3]);
      expect(ledgerMergeInto(keep: keep, gone: gone.take()), 3);
      expect(gone.isDisposed, isTrue);
      expect(keep.count(), 3);
      keep.dispose();
    });
  });

  group('async', () {
    test('a consumed confined handle crosses to an async member', () async {
      // Borrowing one there is FR0012 — the owner would keep using the object
      // while the call ran on a pool thread. Consuming leaves no owner.
      final s = slipOf([10, 20]);
      expect(await ledgerTotalAsync(l: s.take()), 30);
      expect(s.isDisposed, isTrue);
    });

    test('a frozen handle is taken on the async arm', () async {
      final t = Tape.new_(marks: ['a', 'b']);
      expect(await tapeCountAsync(t: t.take()), 2);
      expect(t.isDisposed, isTrue);
    });
  });

  group('frozen and locked', () {
    test('an uncontended frozen consume succeeds', () {
      final t = Tape.new_(marks: ['x', 'y', 'z']);
      expect(t.take().intoCount(), 3);
      expect(t.isDisposed, isTrue);
    });

    /// The portability claim, exercised: taking a locked object acquires no
    /// guard, so this member is **synchronous and portable** — it needs no
    /// `on_contention` contract and it is present on the web surface, which is
    /// what running this test under dart2wasm proves.
    test(
      'a locked consume is synchronous and needs no contention contract',
      () {
        final j = Jar.new_(coins: 41);
        expect(j.take().intoCoins(), 41);
        expect(j.isDisposed, isTrue);
      },
    );
  });

  group('one container that lends and takes', () {
    /// The argument is built by moving the taken objects out of the taken
    /// value, while each lent position is acquired from an id gathered before
    /// any of it was adopted — which works because the borrow points at the
    /// object, not into the container the walk consumes.
    test('a shared lend beside a take, at any count', () {
      expect(ledgerSlipPairs(ps: []), 0);
      final l = Ledger.new_(total: 10);
      final s = slipOf([1, 2]);
      expect(ledgerSlipPairs(ps: [(l, s.take())]), 13);
      expect(s.isDisposed, isTrue);
      expect(l.total(), 10, reason: 'the lent handle is untouched');
      l.dispose();
    });

    test('a mutable lend beside a take of the same type', () {
      final keep = slipOf([1]);
      final gone = slipOf([2, 3]);
      expect(slipAbsorbPairs(ps: [(keep, gone.take())]), 3);
      expect(gone.isDisposed, isTrue);
      expect(keep.count(), 3);
      keep.dispose();
    });

    test('one object lent and taken in the same element is refused', () {
      final s = slipOf([1]);
      expect(
        () => slipAbsorbPairs(ps: [(s, s.take())]),
        throwsA(
          isA<ArgumentError>().having(
            (e) => e.message,
            'message',
            allOf(contains(r'ps[0].$1'), contains(r'ps[0].$2')),
          ),
        ),
      );
      // Nothing spent: the refusal runs before any token is given up.
      expect(s.count(), 1);
      s.dispose();
    });

    /// A map lending its key and taking its value. The take leaves it flat:
    /// building the real map earlier would collapse on the id the key still
    /// is, and two zero-sized objects share one id.
    test('a map lends its key and takes its value', () {
      final a = Ledger.new_(total: 1);
      final b = Ledger.new_(total: 2);
      final x = slipOf([10]);
      final y = slipOf([20]);
      expect(ledgerSlipMap(ps: {a: x.take(), b: y.take()}), 33);
      expect(x.isDisposed, isTrue);
      expect(y.isDisposed, isTrue);
      expect(a.total(), 1);
      a.dispose();
      b.dispose();
    });

    test('a locked guard per lent element, beside a fallible take', () async {
      final v = Vault.new_(balance: 5);
      final j = Jar.new_(coins: 7);
      expect(await vaultJarPairs(ps: [(v, j.take())]), 12);
      expect(j.isDisposed, isTrue);
      expect(await v.readBalance(), 12);
    });
  });

  group('bridged traits', () {
    /// The whole reason a consuming trait position is tagged. The extension
    /// is resolved from the *static* type, so a token holding a concrete
    /// implementor, typed as the interface, reaches the **trait's** glue —
    /// which reads a `Box<dyn Tally>` where the implementor's registry holds
    /// a bare `Abacus`. The tag written from the token's handle is what sends
    /// the take to the right registry.
    test('a Consumed<Tally> holding an implementor reaches the trait glue', () {
      final a = Abacus.new_();
      a.bump();
      final Consumed<Tally> t = a.take();
      expect(t.settle(), 1000);
      expect(a.isDisposed, isTrue);
    });

    test('the same handle through its own extension takes untagged', () {
      final a = Abacus.new_();
      a.bump();
      // Static type `Consumed<Abacus>`: Dart picks the more specific
      // extension, which dispatches to the implementor's own fn_id.
      expect(a.take().settle(), 1000);
      expect(a.isDisposed, isTrue);
    });

    test('the dyn handle takes through tag 0', () {
      final t = newTally(kind: 'step', step: 4);
      t.bump();
      expect(t.take().settle(), 400);
      expect(t.isDisposed, isTrue);
    });

    test('a bridged trait by value, both tags', () {
      final dynamic_ = newTally(kind: 'step', step: 2);
      dynamic_.bump();
      expect(settleTally(t: dynamic_.take()), 200);
      expect(dynamic_.isDisposed, isTrue);

      final a = Abacus.new_();
      a.bump();
      a.bump();
      expect(settleTally(t: a.take()), 2000);
      expect(a.isDisposed, isTrue);
    });

    test('a frozen trait consumes on the pool arm', () async {
      final g = newGreeter(kind: 'pirate');
      expect(await dismissGreeter(g: g.take()), 'ahoy you — and goodbye');
      expect(g.isDisposed, isTrue);

      final r = RobotGreeter.build(id: 7);
      expect(await r.take().farewell(), 'BEEP you [unit 7] — and goodbye');
      expect(r.isDisposed, isTrue);
    });

    test('a locked trait consumes synchronously and on the pool arm', () async {
      final s = await openStore(kind: 'mem');
      await s.put(key: 'a', value: '1');
      expect(s.take().drain(), 1);
      expect(s.isDisposed, isTrue);

      final c = CountingStore.fresh();
      await c.put(key: 'x', value: 'y');
      await c.put(key: 'z', value: 'w');
      expect(await closeStore(s: c.take()), 2);
      expect(c.isDisposed, isTrue);
    });

    test('a container takes every element behind its own tag', () {
      final dyn_ = newTally(kind: 'step', step: 3);
      dyn_.bump();
      final a = Abacus.new_();
      a.bump();
      expect(settleTallies(ts: [dyn_.take(), a.take()]), 1300);
      expect(dyn_.isDisposed, isTrue);
      expect(a.isDisposed, isTrue);
      expect(settleTallies(ts: []), 0);
    });

    test('a frozen container takes every element on the pool arm', () async {
      final g = newGreeter(kind: 'pirate');
      final r = RobotGreeter.build(id: 1);
      expect(await dismissGreeters(gs: [g.take(), r.take()]), [
        'ahoy you — and goodbye',
        'BEEP you [unit 1] — and goodbye',
      ]);
      expect(g.isDisposed, isTrue);
      expect(r.isDisposed, isTrue);
    });

    test('one handle twice in a taken container is refused, nothing spent', () {
      final a = Abacus.new_();
      a.bump();
      expect(
        () => settleTallies(ts: [a.take(), a.take()]),
        throwsA(
          isA<ArgumentError>().having(
            (e) => e.message,
            'message',
            allOf(contains('ts[0]'), contains('ts[1]')),
          ),
        ),
      );
      expect(a.total(), 10);
      a.dispose();
    });

    /// A consumed trait handle joins the duplicate check like any other
    /// acquisition, and the token's `handle` is the object the check compares.
    test(
      'one trait handle for two positions is refused before any spend',
      () async {
        final c = CountingStore.fresh();
        await c.put(key: 'k', value: 'v');
        expect(
          () => storeAbsorb(keep: c, gone: c.take()),
          throwsA(isA<ArgumentError>()),
        );
        expect(await c.get(key: 'k'), 'v');
        c.dispose();
      },
    );
  });

  /// `#[bridge(data, inbound)]`: a data class whose handle fields are the
  /// `Consumed<…>` a caller hands over, because the declaration says the class
  /// crosses Dart → Rust only. Everything here is the container group's rules
  /// applied one level in — the struct is a consume position, not a new kind of
  /// one — so what these prove is that it composes rather than that it is
  /// special.
  group('inbound struct', () {
    Delivery deliveryOf(String label, List<Slip> rest, {Slip? optional}) =>
        Delivery(
          label: label,
          primary: slipOf([1, 2]).take(),
          rest: [for (final s in rest) s.take()],
          optional: optional?.take(),
        );

    test('every field is taken, and every handle spent', () {
      final primary = slipOf([1, 2]);
      final a = slipOf([4]);
      final opt = slipOf([10]);
      final d = Delivery(
        label: 'x',
        primary: primary.take(),
        rest: [a.take()],
        optional: opt.take(),
      );
      expect(deliver(d: d), 'x=7,10');
      expect(primary.isDisposed, isTrue);
      expect(a.isDisposed, isTrue);
      expect(opt.isDisposed, isTrue);
    });

    test('an absent Option field, and an empty list field', () {
      final primary = slipOf([5]);
      expect(
        deliver(
          d: Delivery(
            label: 'y',
            primary: primary.take(),
            rest: const [],
            optional: null,
          ),
        ),
        'y=5,-1',
      );
      expect(primary.isDisposed, isTrue);
    });

    /// An inbound struct inside another, and inside a `Vec`: the nesting the
    /// declaration admits. A plain struct may not hold one, which is FR0004 at
    /// the declaration and so has no runtime shape to test.
    test('an inbound struct nests in another, and in a list', () {
      final m = Manifest(
        head: deliveryOf('h', [
          slipOf([3]),
        ]),
        tail: [deliveryOf('t', const [])],
      );
      expect(deliverAll(m: m), 'h=6,-1;t=3,-1');
    });

    /// The refusal a *use* gets, not a compile error: two fields of one struct
    /// holding one object. It is the same check `f(a.take(), a.take())` meets,
    /// and it names the path through the struct — so nothing is spent and the
    /// handle is still the caller's afterwards.
    test('one handle in two fields is refused, naming the field', () {
      final a = slipOf([1]);
      final token = a.take();
      expect(
        () => deliver(
          d: Delivery(
            label: 'z',
            primary: token,
            rest: [token],
            optional: null,
          ),
        ),
        throwsA(
          isA<ArgumentError>().having(
            (e) => e.message,
            'message',
            allOf(contains('d.primary'), contains('d.rest[0]')),
          ),
        ),
      );
      expect(a.count(), 1, reason: 'nothing was spent');
      a.dispose();
    });

    /// A `frozen` field, whose take can fail. Nothing is in flight here, so it
    /// succeeds — what this pins is that the fallible take runs inside the
    /// struct's rebuild at all, which is where a `Vec<Tape>` already runs it.
    test('a frozen field takes inside the struct', () {
      final t = Tape.new_(marks: ['a', 'b', 'c']);
      expect(
        windReel(
          reel: Reel(tape: t.take(), note: 'n'),
        ),
        'n:3',
      );
      expect(t.isDisposed, isTrue);
    });

    /// Both handle kinds in one class. On a plain data struct the two pull in
    /// opposite directions and the declaration is refused; a parameter-shaped
    /// class sends both the one way, so the token is handed over and the
    /// channel opened in the same request.
    test('a channel field beside a handle field', () async {
      final source = slipOf([1, 2, 3]);
      final out = StreamController<int>();
      final seen = out.stream.toList();
      drainFeed(
        feed: Feed(source: source.take(), out: out),
      );
      expect(await seen, [1, 2, 3]);
      expect(source.isDisposed, isTrue);
    });

    /// An unused token leaves its handle alone whether it sat in a struct or
    /// not: building the class commits to nothing, the call is what spends.
    test('building the class spends nothing', () {
      final primary = slipOf([1]);
      Delivery(
        label: 'q',
        primary: primary.take(),
        rest: const [],
        optional: null,
      );
      expect(primary.count(), 1);
      primary.dispose();
    });
  });

  group('actor', () {
    test('a consuming call sees every queued call and releases the executor', () async {
      final k = await Kiln.new_();
      // Not awaited: these queue ahead of the consume on the executor's FIFO,
      // and the consuming body must see their effects.
      final a = k.fire(n: 2);
      final b = k.fire(n: 3);
      expect(await k.take().intoFired(), 5);
      expect(await a, 2);
      expect(await b, 5);
      // The executor is released by the consume, exactly as dispose() releases
      // it, so a later call has nowhere to go.
      expect(() => k.fire(n: 1), throwsA(isA<StateError>()));
    });

    test('dispose() after a consume is a no-op', () async {
      final k = await Kiln.new_();
      expect(await k.take().intoFired(), 0);
      await k.dispose();
    });
  });
}
