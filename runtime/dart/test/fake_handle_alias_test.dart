/// One raw, several Dart handle objects: does the fake's registry survive it?
///
/// A **real** transport never meets this. Rust mints a fresh raw per response
/// and Dart only ever decodes handles out of *responses*, so one raw has
/// exactly one Dart handle object and one finalizer, and retiring on that
/// finalizer is exactly right.
///
/// A fake inverts the direction: its harness decodes *requests*. A handle
/// nested in a struct parameter — `Widget { doc: TextDoc }` — is decoded by the
/// ordinary generated `_decWidget`, which constructs a second `TextDoc` for a
/// raw the caller is still holding, with a second finalizer attached. If the
/// registry retired on the first finalization, collecting that temporary would
/// pull the object out from under the caller's live handle, and the next call
/// on it would fail with "a handle the fake never minted" — a wrong answer with
/// no visible cause. So attachments are counted, and an entry survives until
/// every Dart object attached to it is gone.
///
/// **Its own file, and VM-only**, for the reason `gc_finalizer_test.dart` is:
/// GC timing is the one genuinely non-deterministic thing in this suite and
/// must not be able to destabilize a suite that is deterministic. Dart promises
/// only that a finalizer *may* run, and on web only that.
///
/// **Why this cannot pass vacuously.** Two controls run beside the aliased raw
/// and both must be retired before its survival is read. One has a single
/// abandoned handle: retiring it proves finalization happened at all. The other
/// has *two* abandoned handles: retiring it proves the count walks down to zero
/// and then retires, so "the aliased raw survived" cannot mean "this registry
/// never retires anything". The aliased raw differs from that second control in
/// exactly one way — one of its two handles is still held.
@TestOn('vm')
library;

import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/testing.dart';
import 'package:test/test.dart';

final class _SilentBridge extends FakeBridge {
  _SilentBridge() : super(BigInt.zero);

  @override
  Uint8List answerSync(int fnId, BinaryReader request) =>
      fakeThrown(UnimplementedError(), 'fn#$fnId');

  @override
  Future<Uint8List> answerAsync(int fnId, BinaryReader request) async =>
      fakeThrown(UnimplementedError(), 'fn#$fnId');
}

/// A stand-in for a generated opaque class: all a handle contributes here is
/// one `HandleDrop.attach` at construction and one finalizer.
final class _Handle extends OpaqueHandle {
  _Handle(super.raw, super.drop);
}

/// Construct a handle for [raw] and abandon it — the shape a harness's
/// `_decWidget(r)` produces for a nested handle parameter.
///
/// Never inlined: a local in the test body can stay live on the frame for the
/// rest of the method, which would keep the object reachable and make the
/// result meaningless.
@pragma('vm:never-inline')
void _abandonAlias(FakeRuntime rt, int raw) {
  final h = _Handle(raw, rt.handleDrop('frustrate_drop_Thing'));
  if (h.isDisposed) throw StateError('a fresh handle cannot be disposed');
}

bool _retired(FakeRuntime rt, int raw) {
  try {
    rt.resolveHandle(raw);
    return false;
  } on StateError {
    return true;
  }
}

/// Allocation pressure until [reclaimed], or null if the limit ran out.
/// Returns the elapsed time so a pass still says how hard the VM had to be
/// pushed.
Future<Duration?> _pressureUntil(
  bool Function() reclaimed, {
  Duration limit = const Duration(seconds: 10),
}) async {
  final sw = Stopwatch()..start();
  var sink = 0;
  while (sw.elapsed < limit) {
    for (var i = 0; i < 32; i++) {
      final junk = List<int>.filled(1 << 14, i);
      sink += junk[junk.length - 1];
    }
    // Yield: Finalizer callbacks are delivered on the message loop, so a tight
    // synchronous loop could allocate forever without running one.
    await Future<void>.delayed(Duration.zero);
    if (reclaimed()) return sw.elapsed;
  }
  expect(
    sink,
    isNonZero,
    reason: 'the pressure loop must not be optimized out',
  );
  return null;
}

void main() {
  test('an entry outlives every alias but not the last one', () async {
    final rt = FakeRuntime(_SilentBridge());
    final drop = rt.handleDrop('frustrate_drop_Thing');

    final once = rt.mintHandle(Object());
    final twice = rt.mintHandle(Object());
    final aliased = rt.mintHandle(Object());

    // The caller's own handle on `aliased`; every other handle below is
    // abandoned the moment it is made.
    final live = _Handle(aliased, drop);

    _abandonAlias(rt, once);
    _abandonAlias(rt, twice);
    _abandonAlias(rt, twice);
    _abandonAlias(rt, aliased);

    final took = await _pressureUntil(
      () => _retired(rt, once) && _retired(rt, twice),
    );
    expect(
      took,
      isNotNull,
      reason:
          'no finalizer ran under 10s of pressure, so this test proves '
          'nothing either way — see the file comment',
    );

    expect(
      _retired(rt, aliased),
      isFalse,
      reason:
          'one of this raw\'s two handles is still held, and the entry '
          'must outlive the other one',
    );
    // Reading it here is also what keeps it reachable across the pressure loop.
    expect(live.isDisposed, isFalse);
  });

  test('dispose retires the entry however many handles alias the raw', () {
    // `dispose()` means free it, whoever else is looking — the same semantics
    // the real bridge has, where a second handle on a dropped raw is exactly
    // the use-after-free `handleValue` cannot detect.
    final rt = FakeRuntime(_SilentBridge());
    final drop = rt.handleDrop('frustrate_drop_Thing');
    final raw = rt.mintHandle(Object());
    final a = _Handle(raw, drop);
    final b = _Handle(raw, drop);
    a.dispose();
    expect(_retired(rt, raw), isTrue);
    expect(b.isDisposed, isFalse, reason: 'the other handle is not told');
  });
}
