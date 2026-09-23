/// "One place that sees every bridge call" — checked against a real bridge, on
/// both platforms, for every kind of call the codegen can emit.
///
/// The claim is easy to make and easy to get wrong in one specific way: a
/// generated actor never touches `Frustrate.instance`. It routes every method,
/// every deferred completion and the synthetic drop its `dispose()` dispatches
/// through the host it was constructed with, so a decorator that wrapped only
/// the runtime would look correct on free functions and opaque members and
/// quietly miss an entire concurrency model. Two more crossings have no call at
/// all: an object's drop (eager or GC), and everything Rust sends *to* Dart,
/// which is registered during request encoding rather than dispatched.
///
/// So this file enumerates the kinds rather than sampling them, and the census
/// below records what the decorator was actually handed — the member name from
/// the generated `frustrateMemberNames`, plus the flags — so a row that stops
/// being observed fails here rather than being discovered by an interceptor
/// that silently under-reports.
///
/// The one row it observes the *arming* of rather than the firing is the GC
/// finalizer: `attach`/`detach` are what arm and disarm it and are asserted
/// here, while whether an abandoned handle is really reclaimed is
/// gc_finalizer_test.dart's question, in its own target, because collection
/// timing is the one thing in this suite that is not deterministic.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/intercept.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// Everything the decorator saw, in order, as short strings a test can compare
/// against. Names come from the generated table, indexed by the same fn id the
/// hook is handed — so this doubles as the proof that the table is usable for
/// what it exists for.
final class _Census extends DelegatingRuntime {
  final List<String> seen = [];

  _Census(super.inner);

  /// The table is keyed by dispatch id, so a lookup is nullable. An id with no
  /// entry would mean a hook saw a member this interface does not have, which
  /// the test should show rather than swallow.
  String _name(int fnId) =>
      frustrateMemberNames[fnId] ?? 'UNKNOWN MEMBER (fn id $fnId)';

  @override
  BinaryReader aroundSync(int fnId, BinaryReader Function() next) {
    seen.add('sync ${_name(fnId)}');
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
    seen.add(
      'async ${_name(fnId)}'
      '${host != null ? ' @host' : ''}'
      '${deferred ? ' deferred' : ''}'
      '${cancel != null ? ' cancel' : ''}',
    );
    return next();
  }

  @override
  HandleDrop handleDrop(String symbol) =>
      _CensusDrop(this, symbol, inner.handleDrop(symbol));

  @override
  Future<ActorHost> spawnActorHost({String? debugName}) async {
    seen.add('spawn $debugName');
    return _CensusHost(this, await inner.spawnActorHost(debugName: debugName));
  }

  @override
  void checkSchemaHash(BigInt expected) {
    seen.add('checkSchemaHash');
    inner.checkSchemaHash(expected);
  }

  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    seen.add('openObject $label');
    return inner.openObject(
      methods,
      onError,
      onDone,
      label: label,
      reclaim: reclaim,
    );
  }

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    seen.add('openStream $label');
    return inner.openStream(
      onItem,
      onError,
      onDone,
      label: label,
      reclaim: reclaim,
    );
  }

  @override
  int openFunction(
    BinaryWriter Function(BinaryReader args) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  }) {
    // `onDeclaredError` is supplied only for a *fallible* closure, so an
    // interceptor can tell the two apart without decoding anything.
    seen.add(
      'openFunction $label${onDeclaredError != null ? ' fallible' : ''}',
    );
    return inner.openFunction(
      onInvoke,
      label: label,
      onDeclaredError: onDeclaredError,
      reclaim: reclaim,
    );
  }

  @override
  void cancelStream(int id) {
    seen.add('cancelStream');
    inner.cancelStream(id);
  }

  @override
  void pauseStream(int id) {
    seen.add('pauseStream');
    inner.pauseStream(id);
  }

  @override
  void resumeStream(int id) {
    seen.add('resumeStream');
    inner.resumeStream(id);
  }
}

/// An actor executor seen through the census. Subclassed rather than used
/// as-is because the reaper registration is a crossing too — it is what frees
/// the executor when a handle is collected without `dispose()`.
final class _CensusHost extends DelegatingActorHost {
  _CensusHost(_Census super.owner, super.inner);

  _Census get _census => owner as _Census;

  @override
  void attachReaper(Object o) {
    _census.seen.add('attachReaper');
    super.attachReaper(o);
  }

  @override
  void detachReaper(Object o) {
    _census.seen.add('detachReaper');
    super.detachReaper(o);
  }

  @override
  Future<void> shutdown() {
    _census.seen.add('shutdown');
    return super.shutdown();
  }

  // A channel opened by an actor member registers on the *host*, not on the
  // transport: on web the producer's cancel flag lives in that worker's own
  // wasm instance, so the two are genuinely different registries. A decorator
  // that only overrode the runtime's `openObject` would see a free function's
  // sink and not an actor's.
  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    _census.seen.add('host openObject $label');
    return super.openObject(
      methods,
      onError,
      onDone,
      label: label,
      reclaim: reclaim,
    );
  }
}

/// Object lifetime: the crossing with no call behind it. `attach` fires at the
/// mint, `detach`+`drop` at an eager `dispose()`, and `drop` alone when the GC
/// finalizer reclaims an abandoned handle.
final class _CensusDrop extends DelegatingHandleDrop {
  final _Census census;
  final String symbol;

  _CensusDrop(this.census, this.symbol, super.inner);

  @override
  void attach(Object owner, int raw) {
    census.seen.add('attach $symbol');
    super.attach(owner, raw);
  }

  @override
  void detach(Object owner) {
    census.seen.add('detach $symbol');
    super.detach(owner);
  }

  @override
  void drop(int raw) {
    census.seen.add('drop $symbol');
    super.drop(raw);
  }
}

void main() {
  late _Census census;

  setUpAll(() async {
    await initBridge();
    // Wrapping the *installed* object, after init: no init parameter, and web's
    // private `WebRuntime._()` never has to be reachable. A decorator is the
    // same bridge, so this needs no quiescence and strands nothing.
    census = _Census(Frustrate.instance);
    Frustrate.activate(census);
  });

  tearDownAll(Frustrate.reset);

  setUp(() => census.seen.clear());

  tearDown(() {
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason: 'open: ${Frustrate.instance.openChannelLabels}',
    );
  });

  test('free functions: sync, async, and async with a cancel token', () async {
    expect(addI32(a: 2, b: 3), 5);
    expect(await sumSquares(n: 3), 14);
    final token = FrustrateCancelToken();
    expect(await withdrawAwaiting(balance: 100, amount: 1, cancel: token), 99);
    expect(census.seen, [
      'sync add_i32',
      'async sum_squares',
      'async withdraw_awaiting cancel',
    ]);
  });

  test('a typed error still travels through the funnel', () async {
    // The decoder rides with the call, so an interceptor that replaced the
    // call rather than wrapping it would lose it. `next` is called; the
    // exception comes back out of it.
    expect(
      () => withdraw(balance: 10, amount: 100),
      throwsA(isA<WithdrawErrorException>()),
    );
    await expectLater(
      withdrawAsync(balance: 10, amount: 100),
      throwsA(isA<WithdrawErrorException>()),
    );
    expect(census.seen, ['sync withdraw', 'async withdraw_async']);
  });

  test('constructors and object lifetime', () async {
    final sync = TextDoc.new_();
    final async = await TextDoc.load(initial: 'hi');
    sync.dispose();
    async.dispose();
    expect(census.seen, [
      'sync TextDoc.new',
      'attach frustrate_drop_TextDoc',
      'async TextDoc.load',
      'attach frustrate_drop_TextDoc',
      'detach frustrate_drop_TextDoc',
      'drop frustrate_drop_TextDoc',
      'detach frustrate_drop_TextDoc',
      'drop frustrate_drop_TextDoc',
    ]);
  });

  test(
    'a Rust to Dart stream, with its backpressure and cancel levers',
    () async {
      final doc = TextDoc.new_();
      final patches = StreamController<TextPatch>();
      doc.watch(sink: patches);
      final sub = patches.stream.listen((_) {});
      // pause/resume/cancel are wired to the controller by the generated
      // binding, so driving the subscription is what drives the levers.
      sub.pause();
      sub.resume();
      await sub.cancel();
      doc.dispose();
      // The registration lands *inside* the call, because that is when it
      // happens: a channel is opened while the request is being encoded, before
      // Rust has the call at all (which is why a call that fails on the way in
      // has to retire what it opened). So the funnel entry precedes it.
      expect(
        census.seen,
        containsAllInOrder([
          'sync TextDoc.watch',
          'openObject TextDoc.watch',
          'pauseStream',
          'resumeStream',
          'cancelStream',
        ]),
      );
    },
  );

  test('a void Dart callback registers on the actor host that carries it', () async {
    final miner = await Miner.new_(label: 'census');
    census.seen.clear();
    final lines = <String>[];
    await miner.report(cb: lines.add);
    await miner.dispose();
    // The registration goes through the *host*, not the transport, because the
    // channel has to reach the worker instance that will feed it.
    expect(
      census.seen,
      containsAllInOrder([
        'async Miner.report @host',
        'host openObject Miner.report',
      ]),
    );
  });

  test(
    'a value-returning Dart function registers through openFunction',
    () async {
      final counter = Counter.new_();
      expect(await counter.holdWriteAsking(f: (x) => x + 1), 1);
      counter.dispose();
      expect(
        census.seen,
        containsAllInOrder([
          'async Counter.hold_write_asking',
          'openFunction Counter.hold_write_asking',
        ]),
      );
    },
  );

  test('a dart_interface registers one channel per method', () async {
    // `Auditor` is a Dart-implemented interface: each method is its own
    // channel, and each is labelled with the member that opened it plus the
    // method name — which is what makes a leak attributable to one of three.
    final result = await audit(
      amount: 5,
      currency: 'EUR',
      a: _Auditor(),
      cancel: FrustrateCancelToken(),
    );
    expect(result, isNotEmpty);
    expect(census.seen, [
      'async audit cancel',
      // Three methods, three channels, each labelled with the member that
      // opened it plus the method — which is what makes a leak attributable to
      // one of the three rather than to "an Auditor".
      'openObject audit Auditor.note',
      'openFunction audit Auditor.approve',
      'openFunction audit Auditor.reserve fallible',
    ]);
  });

  test(
    'an actor: spawn, plain method, deferred with a token, typed error',
    () async {
      final miner = await Miner.new_(label: 'census');
      expect(await miner.label(), 'census');

      final token = FrustrateCancelToken();
      final waiting = miner.deferredWait(token: 1, cancel: token);
      await miner.release(value: 9);
      expect(await waiting, 10);

      await expectLater(
        miner.withdrawFrom(amount: 1 << 40),
        throwsA(isA<WithdrawErrorException>()),
      );
      await miner.dispose();

      expect(census.seen, [
        'spawn Miner',
        // The reaper is armed by the handle's constructor, which runs on the
        // constructing call's answer — so it follows the call rather than the
        // spawn.
        'async Miner.new @host',
        'attachReaper',
        'async Miner.label @host',
        'async Miner.deferred_wait @host deferred cancel',
        'async Miner.release @host',
        'async Miner.withdraw_from @host',
        'detachReaper',
        // The synthetic drop is an ordinary host call, so it lands in the same
        // funnel as any method — which is why refusing at the host would have
        // broken dispose rather than protecting it.
        'async Miner.__frustrate_drop_Miner @host',
        'shutdown',
      ]);
    },
  );

  test('the schema guard and the platform facts are forwarded', () {
    // `checkFrustrateSchema` already ran at init, before this decorator
    // existed; running it again is what observes the forwarding.
    checkFrustrateSchema();
    expect(census.seen, ['checkSchemaHash']);
    // Read by ActorPool for its default width, and by apps deciding where to
    // put work. A decorator that answered for itself would misreport the
    // platform.
    expect(Frustrate.instance.asyncIsParallel, census.inner.asyncIsParallel);
    expect(Frustrate.instance.hardwareParallelism, greaterThanOrEqualTo(1));
  });
}

/// A Dart implementation of the bridged `Auditor` interface — three methods,
/// three channels.
final class _Auditor implements Auditor {
  @override
  void note(String message) {}

  @override
  bool approve(int amount, String currency) => true;

  @override
  int reserve(int amount) => amount;
}
