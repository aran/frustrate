/// What happens to a *real* handle when the bridge under it is swapped out —
/// against the real transport, on both platforms.
///
/// `handle_bridge_identity_test.dart` in runtime/dart pins the rule against
/// stub transports, which is where the combinations are cheap to enumerate.
/// This file pins the thing that rule exists to prevent, using handles whose
/// raws are genuine Rust pointers and whose drop hooks are genuine exports:
/// after `Frustrate.activate(fake)`, sending one of those pointers through the
/// fake would hand it a value the fake never issued, and dropping one through
/// the fake would leak the Rust object while the fake "freed" a registry entry
/// it never had. Neither shows up as a Dart error — the wire carries a bare
/// u64 — so the refusal has to be on this side.
///
/// The positions a handle reaches the wire from are covered here, because the
/// check sits at the one place they all pass through — `handleValue` on the
/// base class, whatever the static type: a **sync** method's receiver
/// (`TextDoc.text`), an **async** one's (`Vault.readBalance`), a handle
/// **parameter** (`docStartsWith`), a **trait-typed** value whose class is not
/// the parameter's type (`bumpTwice(Tally)` given an `Abacus`), and an
/// **actor** receiver (`Miner.label`), which is the only one that is not an
/// `OpaqueHandleBase` at all and so runs the same comparison in generated
/// code. A handle nested in a struct is the same call — the generated struct
/// encoder reads `handleValue` like every other site — and has no fixture
/// here, because FR0031 makes a handle-bearing struct argument-only and
/// test_api declares none.
///
/// **A receiver and a parameter are refused at different moments**, which the
/// fake below is built to tell apart. A receiver is read into a local *before*
/// the call, so the throw happens with the transport untouched. A parameter is
/// read inside the encoder, and the encoder runs inside the transport — so the
/// call is entered, and what the refusal guarantees is that the request never
/// finishes encoding and is therefore never dispatched. The fake counts
/// completed encodes and every test requires that count to be zero, so
/// "refused" here means "no foreign raw reached the wire", not merely "a
/// StateError came back".
///
/// Three properties, and the second is the one that is easy to get wrong:
///
///   * a stranded handle **refuses** to reach the wire, naming its type;
///   * a stranded handle is still **disposable**, and disposing it reaches the
///     transport that minted it. For an actor this is why the check sits on
///     the generated receiver read and not on `ActorHost.call`: `dispose()`
///     dispatches a drop *through the host*, and Rust's `Msg::Stop` does not
///     drop the object, so a refusal at the host would leak every actor
///     disposed after a reset;
///   * a stranded handle is **valid again** once its bridge is active. Nothing
///     was invalidated, only put out of reach.
@Timeout(Duration(minutes: 2))
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// A different bridge. It answers only what a refusal needs, and it is built to
/// catch the failure that matters: [encoded] records any call whose request
/// *finished* being written, which is exactly a call that carried a foreign
/// raw. Every test requires it to stay empty.
final class _OtherBridge implements FrustrateRuntime {
  /// fn ids whose encoder ran to completion under this bridge. Always empty.
  final List<int> encoded = [];

  @override
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  }) {
    encode(BinaryWriter(sizeHint));
    encoded.add(fnId);
    throw StateError('fn $fnId finished encoding under a foreign bridge');
  }

  @override
  Future<BinaryReader> callAsync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    FrustrateCancelToken? cancel,
  }) async {
    encode(BinaryWriter(sizeHint));
    encoded.add(fnId);
    throw StateError('fn $fnId finished encoding under a foreign bridge');
  }

  @override
  Object get bridgeIdentity => this;

  @override
  int get inFlightCallCount => 0;

  @override
  int get openChannelCount => 0;

  @override
  List<String> get openChannelLabels => const [];

  @override
  String toString() => 'a fake bridge';

  @override
  dynamic noSuchMethod(Invocation invocation) => throw StateError(
    'the fake bridge was reached: ${invocation.memberName} — nothing here '
    'should need a transport that serves no calls',
  );
}

Matcher _stranded(String type) => throwsA(
  isA<StateError>().having(
    (e) => e.message,
    'message',
    allOf(contains(type), contains('different bridge')),
  ),
);

void main() {
  late _OtherBridge other;

  setUpAll(initBridge);

  setUp(() => other = _OtherBridge());

  // Every test leaves the real transport active, whatever it did in between.
  tearDown(() {
    expect(
      other.encoded,
      isEmpty,
      reason:
          'a request that finished encoding under the wrong bridge is '
          'one that carried a foreign raw to the wire',
    );
    Frustrate.reset();
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason: 'open: ${Frustrate.instance.openChannelLabels}',
    );
  });

  test(
    'a sync receiver is refused, and the handle is valid again after reset',
    () {
      final doc = TextDoc.new_();
      expect(doc.text(), isEmpty);

      Frustrate.activate(other);
      // Synchronous, so this is a synchronous throw — raised before the request
      // buffer is allocated and before any channel could register.
      expect(doc.text, _stranded('TextDoc'));
      expect(
        doc.isDisposed,
        isFalse,
        reason:
            'refusing a use is not a teardown; the Rust object is '
            'untouched and still owned by the transport that made it',
      );

      Frustrate.reset();
      doc.splice(index: 0, delete: 0, insert: 'hello');
      expect(doc.text(), 'hello');
      doc.dispose();
    },
  );

  test(
    'an async receiver is refused through the future, not synchronously',
    () async {
      final a = Vault.new_(balance: 100);
      final b = Vault.new_(balance: 5);
      expect(await a.merge(other: b), 105);

      Frustrate.activate(other);
      // `callAsync` never throws synchronously and this sits inside that
      // contract: the receiver read happens in the member's async body, so the
      // refusal rejects the returned future like any other issue failure.
      await expectLater(a.readBalance(), _stranded('Vault'));
      await expectLater(a.merge(other: b), _stranded('Vault'));

      Frustrate.reset();
      expect(await a.readBalance(), 105);
      a.dispose();
      b.dispose();
    },
  );

  test('a handle parameter is refused while its request is being written', () {
    // A free function, so there is no receiver to throw first: the only handle
    // is the parameter, and it is read inside the encoder. The call therefore
    // *is* entered, and what the tearDown's `encoded` assertion establishes is
    // the part that matters — the request never finished, so nothing carrying
    // this raw was ever dispatched.
    final doc = TextDoc.new_();
    doc.splice(index: 0, delete: 0, insert: 'hello');
    expect(docStartsWith(doc: doc, prefix: 'hel'), isTrue);

    Frustrate.activate(other);
    expect(() => docStartsWith(doc: doc, prefix: 'hel'), _stranded('TextDoc'));

    Frustrate.reset();
    expect(docStartsWith(doc: doc, prefix: 'hel'), isTrue);
    doc.dispose();
  });

  test('a trait-typed handle is refused through the interface it implements', () {
    // The static type is `Tally`, the class is `Abacus`, and the value reaches
    // the wire through the same inherited `handleValue`. Worth its own row
    // because a trait parameter writes an impl tag *before* the handle, so the
    // encoder has already put a byte in the request when the refusal fires —
    // which is precisely why the guarantee has to be about the request never
    // completing rather than about nothing having been written.
    final abacus = Abacus.new_();
    final first = bumpTwice(t: abacus);

    Frustrate.activate(other);
    expect(() => bumpTwice(t: abacus), _stranded('Abacus'));

    Frustrate.reset();
    // The refused call never reached Rust, so the counter advanced exactly
    // once — a refusal that had encoded and dispatched would show up here as
    // a double step rather than as a passing throw.
    expect(bumpTwice(t: abacus), first * 2);
    abacus.dispose();
  });

  test(
    'a stranded opaque handle still disposes through its own transport',
    () async {
      final vault = Vault.new_(balance: 42);
      expect(await vault.readBalance(), 42);

      Frustrate.activate(other);
      // The fake serves no drop hook at all (`handleDrop` hits its
      // noSuchMethod), so returning normally is a positive observation that the
      // drop went to the real export rather than merely an absence of
      // complaint.
      expect(vault.dispose, returnsNormally);
      expect(vault.isDisposed, isTrue);
    },
  );

  test('a handle minted under a fake is refused by the real bridge', () {
    // The symmetric half: the rule is about disagreement, not about which
    // bridge is the "real" one. This is what keeps a fake's ids out of
    // `frustrate_drop_*` once the fake is gone.
    Frustrate.activate(other);
    final drop = _RecordingDrop();
    // Minted the way generated code mints — a raw plus the drop hook — since
    // nothing generated can mint under a fake that serves no calls.
    final handle = _Minted(0xDEAD, drop);
    expect(handle.handleValue, 0xDEAD);

    Frustrate.reset();
    expect(() => handle.handleValue, _stranded('_Minted'));
    handle.dispose();
    expect(drop.dropped, [
      0xDEAD,
    ], reason: 'dispose goes to the hook the handle was minted with');
  });

  test(
    'an actor is refused on its receiver, and disposes through its host',
    () async {
      final miner = await Miner.new_(label: 'stranded');
      expect(await miner.label(), 'stranded');

      Frustrate.activate(other);
      await expectLater(miner.label(), _stranded('Miner'));

      // The trap the design names: `dispose()` reads `_raw` directly and
      // dispatches the drop *through the host*, whose transport is still the
      // real one. Refusing at `ActorHost.call` instead of at the receiver read
      // would turn this into a leaked Rust object on a released executor,
      // because `Msg::Stop` deliberately does not drop it.
      await miner.dispose();
    },
  );

  test('an actor stranded and then restored keeps working', () async {
    final miner = await Miner.new_(label: 'restored');
    Frustrate.activate(other);
    await expectLater(miner.label(), _stranded('Miner'));
    Frustrate.reset();
    expect(await miner.label(), 'restored');
    await miner.dispose();
  });

  test('a swap is refused while a call is in flight', () async {
    final vault = Vault.new_(balance: 9);
    final inFlight = vault.readBalance();
    expect(
      Frustrate.instance.inFlightCallCount,
      greaterThan(0),
      reason: 'the tally counts what the transport still owes',
    );
    expect(
      () => Frustrate.activate(_OtherBridge()),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('in flight'),
        ),
      ),
    );
    expect(await inFlight, 9);
    expect(Frustrate.instance.inFlightCallCount, 0);
    vault.dispose();
  });

  test('a swap is refused while a channel is open, and names it', () async {
    final doc = TextDoc.new_();
    doc.onChange(cb: (_) {});
    expect(Frustrate.instance.openChannelCount, 1);
    // The label is the same one a leak report carries — the member as the
    // bridge declares it — so a refusal and a leak name the channel the same
    // way.
    expect(
      () => Frustrate.activate(_OtherBridge()),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('TextDoc.on_change'),
        ),
      ),
    );

    // Dropping the document retires the stored callback, which is what makes
    // the transport quiescent again — but the retirement arrives as a posted
    // terminal, so it lands a turn later rather than inside dispose().
    doc.dispose();
    await pumpEventQueue();
    expect(Frustrate.instance.openChannelCount, 0);
    Frustrate.activate(other);
  });
}

/// A handle minted by hand, standing in for a generated one where no generated
/// class can be reached (nothing calls through a fake that serves no calls).
/// Identical in shape: a raw and the active transport's drop hook.
final class _Minted extends OpaqueHandle {
  _Minted(super.raw, super.drop);
}

final class _RecordingDrop implements HandleDrop {
  final List<int> dropped = [];

  @override
  void attach(Object owner, int raw) {}

  @override
  void detach(Object owner) {}

  @override
  void drop(int raw) => dropped.add(raw);
}
