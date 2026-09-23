/// A handle carries the bridge it was minted under, and refuses to reach the
/// wire under a different one.
///
/// This is the sharpest edge in the seams work, and it is not a Dart-level
/// error when it goes wrong. A raw handle value is a pointer the *minting*
/// transport issued. Encoding it into a call on another transport hands that
/// value to a receiver that never issued it: on native a garbage dereference,
/// on a fake a real object entered into a registry that will "drop" it by
/// forgetting it. Neither is detectable downstream — the wire carries a `u64`
/// and nothing else — so the refusal has to happen on this side, at the one
/// place every handle value reaches the wire (`OpaqueHandleBase.handleValue`:
/// the receiver of a method, a parameter, a handle nested in a struct or a
/// list, a trait-typed value).
///
/// The check is keyed on **bridge identity**, not on the active runtime
/// object, and that distinction is the whole design. A decorator over a
/// transport is the same bridge — same raws, same drop exports — so putting
/// one on or taking it off must leave every live handle valid. Keying on the
/// runtime object instead would make a production tracing decorator invalidate
/// every handle in the app the moment it was installed.
///
/// **This file runs with asserts on, and so does every vehicle that runs it**
/// (`dart test` on the VM, `--enable-asserts` on the web one). That is the
/// build the check exists in: the stamp and the comparison are both inside
/// `assert`s, and a build without them is kept safe a different way — it
/// cannot change bridge identity at all, because `Frustrate.activate` refuses
/// to. What a build without asserts carries and what it refuses are measured
/// by `tests/dart_integration/tool/handle_bridge_release_check.dart`, which is
/// a separate vehicle because no test runner can turn asserts off.
@TestOn('vm')
library;

import 'dart:isolate';

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

/// A transport that answers only what these tests read. `bridgeIdentity`
/// defaults to the runtime itself — the answer both real transports give.
final class _Bridge implements FrustrateRuntime {
  final String name;
  final _Drop drop = _Drop();

  _Bridge(this.name);

  @override
  Object get bridgeIdentity => this;

  @override
  HandleDrop handleDrop(String symbol) => drop;

  @override
  int get inFlightCallCount => 0;

  @override
  int get openChannelCount => 0;

  @override
  List<String> get openChannelLabels => const [];

  @override
  String toString() => 'bridge $name';

  @override
  dynamic noSuchMethod(Invocation invocation) =>
      throw StateError('$this: unexpected ${invocation.memberName}');
}

/// Records what it was asked to drop, so "dispose still reaches the transport
/// that minted it" is an observation rather than an absence of a throw.
final class _Drop implements HandleDrop {
  final List<int> dropped = [];
  final List<Object> attached = [];

  @override
  void attach(Object owner, int raw) => attached.add(owner);

  @override
  void detach(Object owner) => attached.remove(owner);

  @override
  void drop(int raw) => dropped.add(raw);
}

/// The minimal decorator: a different runtime object, the same bridge. Stands
/// in for `DelegatingRuntime`, which does not exist yet — what is under test
/// here is the *rule*, and the rule is that this shape changes nothing.
final class _Decorator implements FrustrateRuntime {
  final FrustrateRuntime inner;

  _Decorator(this.inner);

  @override
  Object get bridgeIdentity => inner.bridgeIdentity;

  @override
  HandleDrop handleDrop(String symbol) => inner.handleDrop(symbol);

  @override
  int get inFlightCallCount => inner.inFlightCallCount;

  @override
  int get openChannelCount => inner.openChannelCount;

  @override
  List<String> get openChannelLabels => inner.openChannelLabels;

  @override
  dynamic noSuchMethod(Invocation invocation) =>
      throw StateError('decorator: unexpected ${invocation.memberName}');
}

/// A generated opaque handle, reduced to the part the runtime owns: it is
/// minted with a raw and the transport's drop hook, and everything else is
/// inherited.
final class _Doc extends OpaqueHandle {
  _Doc(super.raw, super.drop);
}

/// Mint a handle exactly the way generated code does — the drop hook read from
/// `Frustrate.instance` at the moment of the mint, never from a per-class
/// static.
_Doc mint(int raw) =>
    _Doc(raw, Frustrate.instance.handleDrop('frustrate_drop_Doc'));

Matcher get _staleBridge => throwsA(
  isA<StateError>().having(
    (e) => e.message,
    'message',
    allOf(contains('_Doc'), contains('different bridge'), contains('reset')),
  ),
);

void main() {
  final a = _Bridge('A');
  final b = _Bridge('B');

  setUpAll(() => Frustrate.install(a, source: a, description: 'bridge A'));

  tearDown(Frustrate.reset);

  test('a handle minted under the active bridge reaches the wire', () {
    final h = mint(0x1111);
    expect(h.handleValue, 0x1111);
  });

  test('a decorator over the same bridge leaves live handles valid', () {
    final h = mint(0x2222);
    Frustrate.activate(_Decorator(a));
    expect(
      h.handleValue,
      0x2222,
      reason:
          'a decorator is the same bridge; it mints the same raws and '
          'drops through the same exports',
    );
    // ...and so does one minted while the decorator is active.
    final under = mint(0x2223);
    expect(under.handleValue, 0x2223);
    Frustrate.reset();
    expect(h.handleValue, 0x2222);
    expect(under.handleValue, 0x2223);
  });

  test('a handle from another bridge is refused, disposable, and valid again', () {
    final h = mint(0x3333);
    Frustrate.activate(b);

    // Refused: this raw belongs to A, and B would dereference a value it never
    // issued.
    expect(() => h.handleValue, _staleBridge);
    expect(
      h.isDisposed,
      isFalse,
      reason:
          'refusing a use is not a teardown; the object is still alive '
          'on the transport that owns it',
    );

    // Still disposable. `dispose()` reads the raw and the drop hook directly,
    // deliberately: a handle stranded by a swap must not become unfreeable,
    // and the hook it holds is the one A handed out.
    h.dispose();
    expect(a.drop.dropped, [0x3333]);
    expect(b.drop.dropped, isEmpty);

    // A handle minted under B, used under B, is fine — the rule is about
    // disagreement, not about which bridge is "real".
    final onB = mint(0x4444);
    expect(onB.handleValue, 0x4444);

    Frustrate.reset();
    // ...and now it is B's handle that is stranded, symmetrically.
    expect(() => onB.handleValue, _staleBridge);
    final again = mint(0x5555);
    expect(again.handleValue, 0x5555);
  });

  test('a disposed handle says so, whichever bridge is active', () {
    final h = mint(0x6666);
    h.dispose();
    expect(
      () => h.handleValue,
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('used after dispose'),
        ),
      ),
    );
    Frustrate.activate(b);
    expect(
      () => h.handleValue,
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('used after dispose'),
        ),
      ),
      reason: 'disposed is the more specific fact and stays the reported one',
    );
  });

  test('the actor receiver check refuses with the same message', () {
    // Actors have no `OpaqueHandleBase` — the generated `_handle` getter runs
    // the identical comparison against its host's bridge and calls this. One
    // thrower, so both halves of the rule read the same to a user.
    expect(
      () => staleBridgeHandle('Miner'),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          allOf(contains('Miner'), contains('different bridge')),
        ),
      ),
    );
  });

  test(
    'a bridge swapped by way of nothing still strands its handles',
    () async {
      expect(await Isolate.run(_swapThroughNothing), isEmpty);
    },
  );
}

/// Runs in its own isolate, because it needs "no platform transport was ever
/// installed" — the ordinary shape of a `package:test` file that fakes the
/// bridge, and unreachable in an isolate that has installed one.
///
/// The route it walks is the one a naive "did the active bridge change?" flag
/// misses. With nothing installed, `reset()` empties the active slot rather
/// than restoring anything, so going from one fake to another is
/// `F -> null -> G`, and **neither step is a change from one bridge to a
/// different bridge**. A flag that only noticed those would still be false
/// when G is active and F's handles are live, and every one of them would
/// then be waved through into G's `frustrate_drop_*`.
///
/// So the definition is "moved away from a non-null bridge", null included,
/// and this is what says so. It also walks back: F's handles are stranded, not
/// invalidated, and re-activating F makes them work again.
List<String> _swapThroughNothing() {
  final complaints = <String>[];
  final f = _Bridge('F');
  final g = _Bridge('G');

  Frustrate.activate(f);
  final onF = mint(0xF00D);
  final alsoF = mint(0xF00E);

  Frustrate.reset();
  if (Frustrate.activeBridge != null) {
    complaints.add('reset with nothing installed left a bridge active');
  }
  Frustrate.activate(g);

  try {
    onF.handleValue;
    complaints.add('a handle minted under F reached the wire under G');
  } on StateError catch (e) {
    if (!e.message.contains('different bridge')) {
      complaints.add('F handle refused under G, but not as a stale bridge: $e');
    }
  }

  final onG = mint(0x600D);
  if (onG.handleValue != 0x600D) {
    complaints.add('a handle minted under G was refused under G');
  }

  // Stranded, not invalidated: the drop still goes to the transport that
  // minted it, which is the property that keeps a swap from leaking.
  onF.dispose();
  if (f.drop.dropped.length != 1 || f.drop.dropped.first != 0xF00D) {
    complaints.add(
      'disposing a stranded handle did not reach F: '
      '${f.drop.dropped}',
    );
  }
  if (g.drop.dropped.isNotEmpty) {
    complaints.add('disposing a stranded handle reached G: ${g.drop.dropped}');
  }

  Frustrate.reset();
  Frustrate.activate(f);
  if (alsoF.handleValue != 0xF00E) {
    complaints.add('a handle minted under F stayed refused once F was back');
  }
  try {
    onG.handleValue;
    complaints.add('a handle minted under G reached the wire under F');
  } on StateError catch (e) {
    if (!e.message.contains('different bridge')) {
      complaints.add('G handle refused under F, but not as a stale bridge: $e');
    }
  }
  return complaints;
}
