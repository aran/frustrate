/// The delegating bases: what forwards, what funnels, and what a decorator
/// changes about the bridge (nothing).
///
/// `intercept_every_kind_test.dart` in tests/dart_integration is the
/// counterpart that runs a counting decorator against a real bridge and
/// requires it to see every kind of call the codegen can emit. This file is the
/// unit half — the parts that are about *forwarding* rather than about a call
/// actually happening, and which a stub inner can therefore say more precisely:
///
///   * a call reaches the funnel exactly once, with the id and the flags;
///   * a funnel that does not call `next` prevents the call, which is the
///     property a fault injector needs;
///   * an actor host's calls funnel through the runtime that spawned it, which
///     is the difference between "sees every call" and "sees the calls that go
///     through the singleton";
///   * a decorator is the same bridge, so it can be activated and reset with
///     live work outstanding.
@TestOn('vm')
library;

import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/intercept.dart';
import 'package:test/test.dart';

/// One thing that happened, in the order it happened.
typedef _Event = (String kind, int fnId);

/// The inner transport: it records nothing itself, answers every call with an
/// empty reader, and hands out a stub host.
final class _Inner implements FrustrateRuntime {
  final List<_Event> log;
  int inFlight = 0;

  _Inner(this.log);

  BinaryReader _answer() => BinaryReader(Uint8List(0));

  @override
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  }) {
    log.add(('inner.sync', fnId));
    encode(BinaryWriter(sizeHint));
    return _answer();
  }

  @override
  Future<BinaryReader> callAsync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    FrustrateCancelToken? cancel,
  }) async {
    log.add(('inner.async', fnId));
    encode(BinaryWriter(sizeHint));
    return _answer();
  }

  @override
  Future<ActorHost> spawnActorHost({String? debugName}) async {
    log.add(('inner.spawn', 0));
    return _InnerHost(this);
  }

  @override
  HandleDrop handleDrop(String symbol) => _InnerDrop(log);

  @override
  Object get bridgeIdentity => this;

  @override
  int get inFlightCallCount => inFlight;

  @override
  int get openChannelCount => 0;

  @override
  List<String> get openChannelLabels => const [];

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    log.add(('inner.openStream', reclaim == null ? 0 : 1));
    return 7;
  }

  @override
  void cancelStream(int id) => log.add(('inner.cancelStream', id));

  @override
  dynamic noSuchMethod(Invocation invocation) =>
      throw StateError('inner: unexpected ${invocation.memberName}');
}

final class _InnerHost implements ActorHost {
  final _Inner rt;

  _InnerHost(this.rt);

  @override
  Future<BinaryReader> call(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    bool deferred = false,
    FrustrateCancelToken? cancel,
  }) async {
    rt.log.add(('inner.host', fnId));
    encode(BinaryWriter(sizeHint));
    return BinaryReader(Uint8List(0));
  }

  @override
  Object get bridgeIdentity => rt.bridgeIdentity;

  @override
  Future<void> shutdown() async => rt.log.add(('inner.shutdown', 0));

  @override
  void attachReaper(Object owner) => rt.log.add(('inner.attachReaper', 0));

  @override
  void detachReaper(Object owner) => rt.log.add(('inner.detachReaper', 0));

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    rt.log.add(('inner.host.openStream', reclaim == null ? 0 : 1));
    return 7;
  }

  @override
  void cancelStream(int id) => rt.log.add(('inner.host.cancelStream', id));

  @override
  dynamic noSuchMethod(Invocation invocation) =>
      throw StateError('inner host: unexpected ${invocation.memberName}');
}

final class _InnerDrop implements HandleDrop {
  final List<_Event> log;

  _InnerDrop(this.log);

  @override
  void attach(Object owner, int raw) => log.add(('inner.attach', raw));

  @override
  void detach(Object owner) => log.add(('inner.detach', 0));

  @override
  void drop(int raw) => log.add(('inner.drop', raw));
}

/// A decorator that records both funnels and, optionally, refuses to call
/// `next` — the fault-injection shape.
final class _Counting extends DelegatingRuntime {
  final List<_Event> log;

  /// fn ids the funnels will refuse instead of forwarding.
  final Set<int> refuse;

  _Counting(super.inner, this.log, {this.refuse = const {}});

  @override
  BinaryReader aroundSync(int fnId, BinaryReader Function() next) {
    log.add(('around.sync', fnId));
    if (refuse.contains(fnId)) throw StateError('refused $fnId');
    return next();
  }

  @override
  Future<BinaryReader> aroundAsync(
    int fnId,
    Future<BinaryReader> Function() next, {
    bool deferred = false,
    FrustrateCancelToken? cancel,
    ActorHost? host,
  }) {
    log.add((
      'around.async'
          '${host != null ? '.host' : ''}'
          '${deferred ? '.deferred' : ''}'
          '${cancel != null ? '.cancel' : ''}',
      fnId,
    ));
    if (refuse.contains(fnId)) {
      // Rejected through the future, never thrown: `callAsync`'s contract runs
      // through here unchanged.
      return Future.error(StateError('refused $fnId'), StackTrace.current);
    }
    return next();
  }
}

void main() {
  late List<_Event> log;
  late _Inner inner;

  setUp(() => log = <_Event>[]);
  _Counting decorate({Set<int> refuse = const {}}) {
    inner = _Inner(log);
    return _Counting(inner, log, refuse: refuse);
  }

  test('a sync call passes the funnel once, then the inner transport', () {
    decorate().callSync(3, 8, (w) => w.writeI64(1));
    expect(log, [('around.sync', 3), ('inner.sync', 3)]);
  });

  test('an async call carries its flags into the funnel', () async {
    final d = decorate();
    final token = FrustrateCancelToken();
    await d.callAsync(11, 8, (w) {}, cancel: token);
    await d.callAsync(12, 8, (w) {});
    expect(log, [
      ('around.async.cancel', 11),
      ('inner.async', 11),
      ('around.async', 12),
      ('inner.async', 12),
    ]);
  });

  test('a funnel that does not forward prevents the call', () async {
    final d = decorate(refuse: {5});
    expect(() => d.callSync(5, 0, (w) {}), throwsStateError);
    await expectLater(d.callAsync(5, 0, (w) {}), throwsStateError);
    // Refused twice, forwarded never — the property a fault injector needs.
    expect(log.where((e) => e.$1.startsWith('inner.')), isEmpty);
  });

  test('an actor host funnels through the runtime that spawned it', () async {
    final d = decorate();
    final host = await d.spawnActorHost(debugName: 'Miner');
    expect(host, isA<DelegatingActorHost>());

    await host.call(20, 8, (w) {});
    await host.call(
      21,
      8,
      (w) {},
      deferred: true,
      cancel: FrustrateCancelToken(),
    );
    // The synthetic drop an actor's dispose() dispatches is an ordinary host
    // call, so it lands in the same funnel.
    await host.call(22, 8, (w) {});
    await host.shutdown();

    expect(log, [
      ('inner.spawn', 0),
      ('around.async.host', 20),
      ('inner.host', 20),
      ('around.async.host.deferred.cancel', 21),
      ('inner.host', 21),
      ('around.async.host', 22),
      ('inner.host', 22),
      ('inner.shutdown', 0),
    ]);
  });

  test('handle lifetime is visible, and the wrapper keeps the hook alive', () {
    final d = decorate();
    final drop = d.handleDrop('frustrate_drop_Doc');
    expect(drop, isA<DelegatingHandleDrop>());
    drop.attach(Object(), 0x99);
    drop.drop(0x99);
    expect(log, [('inner.attach', 0x99), ('inner.drop', 0x99)]);
  });

  test('channel plumbing forwards on both the runtime and the host', () async {
    final d = decorate();
    expect(d.openStream((_) {}, (_, __) {}, () {}, label: 'X.watch'), 7);
    d.cancelStream(7);
    final host = await d.spawnActorHost();
    expect(host.openStream((_) {}, (_, __) {}, () {}), 7);
    host.cancelStream(7);
    // The reclaim rides the hop. A decorator that dropped it would strand
    // every handle an absorbed item carries — silently, because nothing else
    // observes a minted object with no wrapper. The 1s below are that
    // parameter arriving; see the reclaim-forwarding test underneath.
    // A host's channel members reach the *host*, not the runtime: on web the
    // producer's cancel flag lives in that worker's own wasm instance, so a
    // decorator that quietly redirected them to the transport would signal the
    // wrong registry.
    expect(log, [
      ('inner.openStream', 0),
      ('inner.cancelStream', 7),
      ('inner.spawn', 0),
      ('inner.host.openStream', 0),
      ('inner.host.cancelStream', 7),
    ]);
  });

  /// The reclaim is the one channel argument whose loss is invisible: the
  /// stream still works, the items still arrive, and only the handles in the
  /// ones that *do not* arrive are stranded. So it is pinned on the hop rather
  /// than left to the signature.
  test('a decorator forwards the reclaim it was given', () async {
    final d = decorate();
    d.openStream((_) {}, (_, __) {}, () {}, reclaim: [(_) {}]);
    final host = await d.spawnActorHost();
    host.openStream((_) {}, (_, __) {}, () {}, reclaim: [(_) {}]);
    expect(log.where((e) => e.$1.endsWith('openStream')).toList(), [
      ('inner.openStream', 1),
      ('inner.host.openStream', 1),
    ], reason: 'a dropped reclaim leaks every absorbed item, silently');
  });

  test('a decorator is the same bridge as what it wraps', () {
    final d = decorate();
    expect(identical(d.bridgeIdentity, inner.bridgeIdentity), isTrue);

    Frustrate.install(inner, source: inner, description: 'the inner transport');
    // Live work outstanding: a bridge change would be refused, and a decorator
    // is not one. This is the property that makes a production interceptor
    // installable at all.
    inner.inFlight = 4;
    Frustrate.activate(d);
    expect(identical(Frustrate.instance, d), isTrue);
    expect(identical(Frustrate.activeBridge, inner.bridgeIdentity), isTrue);
    Frustrate.reset();
    expect(identical(Frustrate.instance, inner), isTrue);
  });
}
