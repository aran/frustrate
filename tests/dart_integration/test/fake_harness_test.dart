/// The generated typed harness, driven through the real bindings with no
/// library loaded — on the VM and in the browser, from one body.
///
/// Nothing here calls `initBridge()`. That is the claim: the generated client
/// encodes a request, a generated arm decodes it with the same walkers, a
/// plain Dart object answers, and the answer comes back through the client's
/// own decoders. Every call kind the codegen can emit is exercised, because a
/// harness that quietly omitted one would be worse than no harness — the test
/// that needed it would pass against a shape the real bridge does not have.
///
/// The rows, and where each is below:
///
///   * sync / async free function, and an `async fn` with a cancel token
///   * a typed error, thrown by the fake and caught as its generated class
///   * a constructor (sync and async), an opaque method, and dispose
///   * a Rust → Dart stream with pause, resume and cancel
///   * a void Dart callback
///   * a value-returning Dart closure, and one that refuses as a value
///   * a `dart_interface`, and a `Vec` of them nested in a struct
///   * handles nested in a struct (a `Fanout` of two controllers)
///   * an actor: spawn, plain method, deferred completion with a token, and
///     the synthetic drop `dispose()` dispatches
///   * trait handles, in both directions, including a concrete implementor
///   * a type that declares two representations, answered from both
///     places: its value half on the crate fake, its handle half on its own
///   * a member the fake did not implement, which must be loud
///
/// What a fake cannot show is stated on `FakeRuntime`: Rust's codec, its locks,
/// its schedulers. Those rows run against the real library in this same
/// directory.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/testing.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

// ------------------------------------------------------------- the fake --

final class _Docs extends FakeTextDoc {
  _Docs(this.text_);
  String text_;

  /// The sink `watch` was handed, kept so a test can drive the producer.
  FakeStreamSink<TextPatch>? watcher;

  /// Every callback registered through `on_change`.
  final List<void Function(int)> changed = [];

  @override
  String text() => text_;

  @override
  int lenChars() => text_.length;

  @override
  void watch({required FakeStreamSink<TextPatch> sink}) {
    // `TextDoc::watch` stores the sink in `self.watchers`, so the channel
    // outlives the call. In Rust that declaration is the store itself; here it
    // is this line, because Dart has no Drop for the harness to observe.
    sink.retain();
    watcher = sink;
  }

  @override
  void onChange({required void Function(int) cb}) => changed.add(cb);

  @override
  List<TextPatch> splice({
    required int index,
    required int delete,
    required String insert,
  }) {
    text_ = text_.replaceRange(index, index + delete, insert);
    return [TextPatchSplice(index: index, text: insert)];
  }
}

/// The fake behind a `Snapshot` handle. Handed out as a *field* of a returned
/// value, which is the shape this exists for.
final class _Snapshot extends FakeSnapshot {
  _Snapshot(this.words);
  final int words;

  @override
  int wordCount() => words;
}

/// The fake behind a consumed handle. Its members are the ones the *Rust*
/// side has, so a consuming receiver is an ordinary method here — `take()` is
/// the client's spelling, and the harness sees only the handle it was given.
final class _Slip extends FakeSlip {
  _Slip(this.entries);
  final List<int> entries;

  @override
  int intoTotal() => entries.fold(0, (a, b) => a + b);

  @override
  void add({required int n}) => entries.add(n);

  @override
  int count() => entries.length;
}

final class _Counter extends FakeCounter {
  int value = 0;

  @override
  Future<int> add({required int delta}) async => value += delta;

  @override
  Future<int> holdWriteAsking({required Future<int> Function(int) f}) async {
    value = await f(value);
    return value;
  }
}

final class _Miner extends FakeMiner {
  _Miner(this.label_);
  final String label_;

  /// The outstanding `deferred_wait`, released by `release`. A deferred
  /// method's completion is detached from the executor, which is exactly what
  /// holding a completer here models.
  Completer<int>? waiting;
  bool disposedWhileWaiting = false;

  @override
  Future<String> label() async => label_;

  @override
  Future<int> deferredWait({required int token}) {
    final c = Completer<int>();
    waiting = c;
    return c.future;
  }

  @override
  Future<void> release({required int value}) async {
    waiting?.complete(value + 1);
    waiting = null;
  }

  @override
  Future<int> withdrawFrom({required int amount}) async =>
      throw WithdrawErrorException(WithdrawErrorInsufficient(shortBy: amount));

  @override
  Future<void> report({required void Function(String) cb}) async {
    cb('mined');
    cb('again');
  }
}

final class _Tally extends FakeTally {
  _Tally(this.kind, this.step);
  final String kind;
  final int step;
  int n = 0;

  @override
  int bump() => n += step;

  @override
  int total() => n;

  @override
  String describe() => '$kind($n)';

  @override
  int settle() => n * 100;
}

final class _Abacus extends FakeAbacus {
  int n = 0;

  @override
  int bump() => ++n;

  @override
  int total() => n;

  @override
  String describe() => 'abacus($n)';

  @override
  int settle() => n * 1000;
}

final class _Greeter extends FakeGreeter {
  _Greeter(this.kind);
  final String kind;

  @override
  String greet({required String name}) => '$kind: $name';

  @override
  Future<List<String>> greetMany({required List<String> names}) async => [
    for (final n in names) greet(name: n),
  ];

  /// A member that returns the trait it lives on: the arm mints a fresh handle
  /// for the object this returns, and the caller decodes the dyn implementor.
  @override
  Future<FakeGreeter> louder() async => _Greeter(kind.toUpperCase());
}

/// The **handle** half of a type that declares two representations. Its
/// members are answered here; the same type's *value* members are answered by
/// the crate fake, because a value receiver has no handle to resolve.
final class _NoteHandle extends FakeNoteHandle {
  _NoteHandle(this.title, this.body);
  final String title;
  String body;

  @override
  Future<int> append({required String more}) async {
    body = '$body $more';
    return body.split(' ').where((w) => w.isNotEmpty).length;
  }

  @override
  Future<Note> snapshot() async => Note(title: title, body: body);
}

/// The crate-level fake: free functions, statics and every constructor.
final class _Api extends FakeTestApi {
  /// The objects handed out, so a test can reach the fake behind a handle it
  /// is holding.
  final List<_Docs> docs = [];
  _Miner? miner;
  _Counter? counter;

  /// What `splitParity` was given, kept so the test can feed both sinks after
  /// the call returned — which is what a Rust body that stored them does.
  FakeFanout? fanout;

  /// Left never-completing on purpose: the cancel row needs a call that is
  /// still in flight when the token fires.
  final Completer<int> neverAnswers = Completer<int>();

  @override
  int addI32({required int a, required int b}) => a + b;

  /// A data type's members land here, not on a `Fake<Type>`: the receiver is
  /// a value on the wire with no handle to resolve, so it arrives as the
  /// first argument.
  @override
  double pointNorm({required Point self}) => self.x.abs() + self.y.abs();

  @override
  String colorHex({required Color self}) => self.name;

  /// The *value* half of a type that also declares a handle half. It lands
  /// here, with the receiver as the leading argument, exactly as any other
  /// data type's member does — the declaration's second representation
  /// changes nothing about this one.
  @override
  int noteWordCount({required Note self}) =>
      self.body.split(' ').where((w) => w.isNotEmpty).length;

  @override
  FakeNoteHandle noteReopen({required Note n}) => _NoteHandle(n.title, n.body);

  /// Generic data types reach the fake as ordinary concrete types: the fake
  /// sees `Page<Item>`, never a template. Only the shape that reaches a handle
  /// differs, and it differs the same way every handle-bearing type does —
  /// `FakePage<FakeTextDoc>`, built out of `Fake<Type>` objects.
  @override
  Page<Item> itemPage({required int n}) => Page<Item>(
    items: [for (var i = 0; i < n; i++) Item(id: i, label: 'item$i')],
    total: n,
  );

  @override
  Page<int> doublePage({required Page<int> p}) =>
      Page<int>(items: [for (final x in p.items) x * 2], total: p.total * 2);

  @override
  int eitherSum({required Either<int, Item> e}) => switch (e) {
    EitherLeft(:final field0) => field0,
    EitherRight(:final field0) => field0.id,
  };

  @override
  FakePage<FakeTextDoc> docPage({required int n}) => FakePage<FakeTextDoc>(
    items: [for (var i = 0; i < n; i++) _Docs('doc$i')],
    total: n,
  );

  /// A returned value carrying handles: the fake builds it out of its own
  /// `Fake<Type>` objects and the harness registers one handle per field,
  /// exactly as the real Rust mints one per field.
  @override
  FakeWorkspace openWorkspace({
    required String label,
    required List<String> words,
  }) => FakeWorkspace(
    label: label,
    snapshot: _Snapshot(words.length),
    counter: _Counter(),
  );

  @override
  Future<int> sumSquares({required int n}) async {
    var total = 0;
    for (var i = 1; i <= n; i++) {
      total += i * i;
    }
    return total;
  }

  @override
  int withdraw({required int balance, required int amount}) => amount > balance
      ? throw WithdrawErrorException(
          WithdrawErrorInsufficient(shortBy: amount - balance),
        )
      : balance - amount;

  @override
  Future<int> withdrawAwaiting({required int balance, required int amount}) =>
      neverAnswers.future;

  @override
  FakeTextDoc textDocNew() {
    final d = _Docs('');
    docs.add(d);
    return d;
  }

  @override
  Future<FakeTextDoc> textDocLoad({required String initial}) async {
    final d = _Docs(initial);
    docs.add(d);
    return d;
  }

  @override
  FakeCounter counterNew() => counter = _Counter();

  /// The consuming shapes. Every handle the harness hands over is retired from
  /// its registry as it is read, so the raw the client wrote resolves to
  /// nothing afterwards — the fake's stand-in for the object being gone.
  final List<_Slip> slips = [];

  @override
  FakeSlip slipNew() {
    final s = _Slip([]);
    slips.add(s);
    return s;
  }

  @override
  int ledgerTotal({required FakeSlip l}) =>
      (l as _Slip).entries.fold(0, (a, b) => a + b);

  @override
  int ledgerTotalAll({required List<FakeSlip> ls}) =>
      ls.fold(0, (a, l) => a + (l as _Slip).entries.fold(0, (x, y) => x + y));

  @override
  int slipAbsorbPairs({required List<(FakeSlip, FakeSlip)> ps}) {
    var n = 0;
    for (final (keep, gone) in ps) {
      (keep as _Slip).entries.addAll((gone as _Slip).entries);
      n += keep.entries.length;
    }
    return n;
  }

  @override
  int slipTotalSet({required Set<FakeSlip> ss}) =>
      ss.fold(0, (a, s) => a + (s as _Slip).entries.fold(0, (x, y) => x + y));

  /// A struct carrying both handle kinds: the opaque arrives as the object,
  /// the channel as the sink bound to the caller's controller. One decode
  /// reaches both, which is what the client's one encode wrote.
  @override
  void drainFeed({required FakeFeed feed}) {
    for (final n in (feed.source as _Slip).entries) {
      feed.out.add(n);
    }
    feed.out.close();
  }

  /// An **inbound** struct arrives here as the mirror class, holding the
  /// objects themselves where the client class held `Consumed<…>` tokens: the
  /// fake IS the Rust side, so what it receives is what Rust would have
  /// adopted.
  @override
  String deliver({required FakeDelivery d}) {
    var total = (d.primary as _Slip).entries.fold(0, (x, y) => x + y);
    for (final s in d.rest) {
      total += (s as _Slip).entries.fold(0, (x, y) => x + y);
    }
    final opt = d.optional == null
        ? -1
        : (d.optional! as _Slip).entries.fold(0, (x, y) => x + y);
    return '${d.label}=$total,$opt';
  }

  @override
  String slipTotalByName({
    required Map<String, FakeSlip> ss,
  }) => (ss.entries.toList()..sort((a, b) => a.key.compareTo(b.key)))
      .map(
        (e) =>
            '${e.key}=${(e.value as _Slip).entries.fold(0, (x, y) => x + y)}',
      )
      .join(',');

  @override
  Future<FakeMiner> minerNew({required String label}) async =>
      miner = _Miner(label);

  /// Adds and returns *without* closing — the `count_to` shape, where the
  /// channel ends because the Rust body let the sink go rather than closing it.
  @override
  Future<void> countTo({
    required int n,
    required FakeStreamSink<int> sink,
  }) async {
    for (var i = 1; i <= n; i++) {
      // The producer's own question, and the honest one: `add` answers false
      // once the consumer has cancelled, exactly as Rust's does.
      if (!sink.add(i)) return;
    }
  }

  /// Closes explicitly, the other half of the pair: a body that ends the
  /// stream itself rather than letting it end by going out of scope.
  @override
  void emitTwo({required FakeStreamSink<int> sink}) {
    sink.add(1);
    sink.add(2);
    sink.close();
  }

  @override
  Future<void> notifyN({required int n, required void Function(int) cb}) async {
    for (var i = 0; i < n; i++) {
      cb(i);
    }
  }

  @override
  Future<int> transform({
    required int x,
    required Future<int> Function(int) f,
  }) => f(x);

  @override
  Future<String> audit({
    required int amount,
    required String currency,
    required FakeAuditor a,
  }) async {
    a.note('auditing $amount $currency');
    if (!await a.approve(amount, currency)) return 'refused';
    try {
      return 'reserved ${await a.reserve(amount)}';
    } on RefusalErrorException catch (e) {
      return 'refused as a value: ${e.error}';
    }
  }

  @override
  Future<int> runAudit({required FakeAuditRun run, required int amount}) async {
    var total = 0;
    for (final a in run.auditors) {
      a.note(run.label);
      total += await a.reserve(amount);
    }
    return total;
  }

  @override
  Future<void> splitParity({required int n, required FakeFanout out}) async {
    // Kept, so the test can feed both sinks after the call — the shape a Rust
    // body that stashes them has.
    out.evens.retain();
    out.odds.retain();
    fanout = out;
    for (var i = 0; i < n; i++) {
      (i.isEven ? out.evens : out.odds).add(i);
    }
  }

  @override
  Future<int> applyTransform({required int x, required FakeTransforms t}) =>
      t.double(x);

  @override
  FakeTally newTally({required String kind, required int step}) =>
      _Tally(kind, step);

  @override
  int bumpTwice({required FakeTally t}) {
    t.bump();
    return t.bump();
  }

  @override
  FakeAbacus abacusNew() => _Abacus();

  @override
  int settleTally({required FakeTally t}) => t.settle();

  @override
  int tallySum({required List<FakeTally> ts}) =>
      ts.fold(0, (a, t) => a + t.total());

  @override
  int settleTallies({required List<FakeTally> ts}) =>
      ts.fold(0, (a, t) => a + t.settle());

  @override
  List<FakeGreeter> greeters({required List<String> kinds}) => [
    for (final k in kinds) _Greeter(k),
  ];
}

/// The Dart side of the bridged `Auditor` interface — the caller's object, not
/// the fake's.
final class _Auditor implements Auditor {
  final List<String> notes = [];
  bool approves = true;
  int? refuseAfterMs;

  @override
  void note(String message) => notes.add(message);

  @override
  bool approve(int amount, String currency) => approves;

  @override
  int reserve(int amount) {
    final ms = refuseAfterMs;
    if (ms != null)
      throw RefusalErrorException(RefusalErrorBusy(retryInMs: ms));
    return amount;
  }
}

// ------------------------------------------------------------------ tests --

void main() {
  late _Api api;

  setUp(() {
    api = _Api();
    // Nothing is installed, so this is the only runtime the bindings will
    // ever see in this suite — no library is opened, on either platform.
    Frustrate.activate(FakeRuntime(FakeTestApiBridge(api)));
  });

  tearDown(() {
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason: 'open: ${Frustrate.instance.openChannelLabels}',
    );
    // Refuses unless the fake is quiescent, which is the same rule a real
    // bridge swap is held to.
    Frustrate.reset();
  });

  test('the harness answers only for the interface it was generated from', () {
    expect(checkFrustrateSchema, returnsNormally);
    // The fake's own facts, fixed rather than configurable: nothing here runs
    // on a second thread, so a knob could only let a test assert a lie.
    expect(Frustrate.instance.asyncIsParallel, isFalse);
    expect(Frustrate.instance.hardwareParallelism, 1);
  });

  test(
    'free functions: sync, async, and an async fn with a cancel token',
    () async {
      expect(addI32(a: 2, b: 3), 5);
      expect(await sumSquares(n: 3), 14);

      final token = FrustrateCancelToken();
      final pending = withdrawAwaiting(balance: 100, amount: 1, cancel: token);
      expect(Frustrate.instance.inFlightCallCount, 1);
      token.cancel();
      await expectLater(pending, throwsA(isA<CancelledCallException>()));
      expect(Frustrate.instance.inFlightCallCount, 0);
      // The fake's own future is still outstanding; its answer lands on a call
      // nobody holds and is dropped rather than double-settling anything.
      api.neverAnswers.complete(0);
    },
  );

  test(
    'a returned value carrying handles reaches the caller as real handles',
    () async {
      final w = openWorkspace(label: 'ws', words: ['a', 'b', 'c']);
      expect(w.label, 'ws');
      // Each field arrived as a live handle over the fake object behind it.
      // Both members are portable: this file compiles as one body for the VM
      // and the browser, so a native-only member (`blockingGet`, whose
      // acquisition waits by declaration) cannot be named here at all. The real
      // bridge covers those under their own native gate in
      // native_blocking_test.dart.
      expect(w.snapshot.wordCount(), 3);
      expect(await w.counter.add(delta: 0), 0);
      w.snapshot.dispose();
      w.counter.dispose();
    },
  );

  test('a data type`s members are answered by the crate fake', () {
    // The client still writes `p.norm()`; the receiver crosses as a value and
    // reaches the fake as its leading argument. A unit-only enum takes the
    // same route.
    expect(const Point(x: -3, y: 4, label: null).norm(), 7.0);
    expect(Color.green.hex(), 'green');
  });

  test(
    'a type with two representations is answered from both places',
    () async {
      // The value half is a method on the crate fake, taking the receiver as an
      // argument; the handle half is a `Fake<Type>` the wire resolves from a
      // handle id. One Rust struct, two answering places, and the client writes
      // the same two calls it writes against the real bridge.
      expect(const Note(title: 't', body: 'a b c').wordCount(), 3);
      final h = noteReopen(
        n: const Note(title: 't', body: 'a b'),
      );
      expect(await h.append(more: 'c'), 3);
      expect((await h.snapshot()).body, 'a b c');
      h.dispose();
    },
  );

  test('a typed error the fake throws arrives as its generated exception', () {
    expect(withdraw(balance: 100, amount: 10), 90);
    expect(
      () => withdraw(balance: 10, amount: 100),
      throwsA(
        isA<WithdrawErrorException>().having(
          (e) => (e.error as WithdrawErrorInsufficient).shortBy,
          'shortBy',
          90,
        ),
      ),
    );
  });

  test('a throw the member does not declare crosses as a panic naming it', () {
    // `lenChars` is not overridden by `_Docs`... it is; `splice` past the end
    // is the undeclared failure. Either way the arm turns it into a panic
    // envelope that names the member, which is the difference between a
    // debuggable test failure and a wrong answer.
    final doc = TextDoc.new_();
    expect(
      () => doc.splice(index: 99, delete: 0, insert: 'x'),
      throwsA(
        isA<BridgePanicException>().having(
          (e) => e.message,
          'message',
          contains('TextDoc.splice'),
        ),
      ),
    );
    doc.dispose();
  });

  group('a consumed handle', () {
    test('a consuming receiver reaches the fake and retires the handle', () {
      final s = Slip.new_();
      s.add(n: 2);
      s.add(n: 3);
      expect(s.take().intoTotal(), 5);
      // The client half: the handle is spent, exactly as against the library.
      expect(s.isDisposed, isTrue);
      // The harness half: the wire's registry no longer holds it, so a second
      // call carrying the same raw is refused rather than answered.
      expect(
        () => ledgerTotal(l: Consumed<Slip>(s)),
        throwsA(isA<StateError>()),
      );
    });

    test('a consumed parameter, bare and in a list', () {
      final a = Slip.new_()..add(n: 1);
      final b = Slip.new_()..add(n: 2);
      expect(ledgerTotal(l: a.take()), 1);
      expect(ledgerTotalAll(ls: [b.take()]), 2);
      expect(a.isDisposed, isTrue);
      expect(b.isDisposed, isTrue);
    });

    /// One element lent and one taken: the harness resolves the first and
    /// retires the second, from one `[handle][handle]` pair.
    test('a container that lends and takes in the same element', () {
      final keep = Slip.new_()..add(n: 1);
      final gone = Slip.new_()..add(n: 2);
      expect(slipAbsorbPairs(ps: [(keep, gone.take())]), 2);
      expect(gone.isDisposed, isTrue);
      expect(keep.isDisposed, isFalse);
      expect(keep.count(), 2);
    });

    /// An inbound struct: its handle fields are read out of the registry as
    /// the harness decodes the request, so every one of them is retired by the
    /// call exactly as a bare `Consumed<Slip>` parameter is.
    test('an inbound struct retires every handle it carried', () {
      final primary = Slip.new_()..add(n: 1);
      final rest = Slip.new_()..add(n: 2);
      final opt = Slip.new_()..add(n: 4);
      expect(
        deliver(
          d: Delivery(
            label: 'x',
            primary: primary.take(),
            rest: [rest.take()],
            optional: opt.take(),
          ),
        ),
        'x=3,4',
      );
      expect(primary.isDisposed, isTrue);
      expect(rest.isDisposed, isTrue);
      expect(opt.isDisposed, isTrue);
      // The harness gave them up as it read them, so a raw it has already
      // handed over resolves to nothing.
      expect(
        () => deliver(
          d: Delivery(
            label: 'y',
            primary: Consumed<Slip>(primary),
            rest: const [],
            optional: null,
          ),
        ),
        throwsA(isA<StateError>()),
      );
    });

    /// Both handle kinds through one struct decode: the fake takes the object
    /// out of its registry and binds the channel to the caller's controller,
    /// from the one request the client encoded.
    test('a channel field beside a handle field', () async {
      final source = Slip.new_()
        ..add(n: 1)
        ..add(n: 2);
      final out = StreamController<int>();
      final got = <int>[];
      var done = false;
      out.stream.listen(got.add, onDone: () => done = true);
      drainFeed(
        feed: Feed(source: source.take(), out: out),
      );
      await pumpEventQueue();
      expect(got, [1, 2]);
      expect(done, isTrue);
      expect(source.isDisposed, isTrue);
      expect(Frustrate.instance.openChannelCount, 0);
    });

    test('a consumed set and a consumed map, each element retired', () {
      final a = Slip.new_()..add(n: 1);
      final b = Slip.new_()..add(n: 2);
      expect(slipTotalSet(ss: {a.take(), b.take()}), 3);
      expect(a.isDisposed, isTrue);
      expect(b.isDisposed, isTrue);
      // The harness gave both up as it read them, so a raw it has already
      // handed over resolves to nothing.
      expect(
        () => slipTotalSet(ss: {Consumed<Slip>(a)}),
        throwsA(isA<StateError>()),
      );

      final c = Slip.new_()..add(n: 4);
      expect(slipTotalByName(ss: {'c': c.take()}), 'c=4');
      expect(c.isDisposed, isTrue);
    });

    /// A consumed **trait** position is `[impl tag][handle]` on the wire. A
    /// fake has one registry, so the tag is read and dropped; what this pins
    /// is that it is read at all — a harness that skipped it would take the
    /// tag byte for the top of the handle.
    test('a trait receiver and a trait parameter, both tagged', () {
      final t = newTally(kind: 'steps', step: 3);
      t.bump();
      expect(t.take().settle(), 300);
      expect(t.isDisposed, isTrue);

      // Through the interface's own extension, holding an implementor: the
      // static type is `Consumed<Tally>`, so this is the tagged route.
      final a = Abacus.new_();
      a.bump();
      a.bump();
      final Consumed<Tally> token = a.take();
      expect(token.settle(), 2000);

      final b = Abacus.new_();
      b.bump();
      expect(settleTally(t: b.take()), 1000);
      expect(b.isDisposed, isTrue);
      // Retired from the harness's registry as it was read.
      expect(
        () => settleTally(t: Consumed<Abacus>(b)),
        throwsA(isA<StateError>()),
      );
    });
  });

  test('a member the fake never implemented is loud, and says which', () async {
    // `_Counter` implements two of Counter's members. Reaching a third is the
    // ordinary way a fake is incomplete, and it must name the member rather
    // than answer a plausible zero.
    final counter = Counter.new_();
    await expectLater(
      counter.get(),
      throwsA(
        isA<BridgePanicException>().having(
          (e) => e.message,
          'message',
          contains('FakeCounter.get is not implemented'),
        ),
      ),
    );
    counter.dispose();
  });

  test('constructors, an opaque method, and dispose', () async {
    final made = TextDoc.new_();
    expect(made.text(), '');
    final loaded = await TextDoc.load(initial: 'hello');
    expect(loaded.text(), 'hello');
    expect(loaded.lenChars(), 5);
    expect(api.docs, hasLength(2));

    loaded.dispose();
    made.dispose();
    // The handle is gone from the fake's registry, so a resurrected raw does
    // not silently answer — the fake's stand-in for a dangling pointer.
    expect(loaded.isDisposed, isTrue);
  });

  test('a Rust to Dart stream, with pause, resume and cancel', () async {
    final doc = TextDoc.new_();
    final patches = StreamController<TextPatch>();
    doc.watch(sink: patches);
    final sink = api.docs.single.watcher!;

    final got = <TextPatch>[];
    final sub = patches.stream.listen(got.add);
    expect(sink.add(const TextPatchClear()), isTrue);
    expect(got, isEmpty, reason: 'delivery is never inline with the producer');
    await pumpEventQueue();
    expect(got, hasLength(1));

    sub.pause();
    await pumpEventQueue();
    expect(
      sink.isPaused,
      isTrue,
      reason: 'the consumer paused; Rust would park',
    );
    sub.resume();
    await pumpEventQueue();
    expect(sink.isPaused, isFalse);

    await sub.cancel();
    expect(sink.isCancelled, isTrue);
    expect(
      sink.add(const TextPatchClear()),
      isFalse,
      reason: 'the cooperative flag, exactly as Rust`s add reports it',
    );
    doc.dispose();
  });

  test('a sink the fake let go is ended by the call that handed it over', () async {
    // The `count_to` shape: the body adds items and returns *without* closing,
    // and the stream ends because the sink went out of scope. Under the fake
    // it ends because the request scope retired it, which is the same fact
    // with the declaration moved (Dart has no Drop).
    //
    // What this deliberately does NOT assert is whether `onDone` lands before
    // or after the caller's await resumes. That interleaving is transport
    // physics and the two real platforms already disagree about it — measured
    // on `count_to`, native reports done-then-answered and web
    // answered-then-done — so a test that depended on it would be pinned to one
    // platform, and a fake matching either would be matching an accident.
    var done = false;
    final c = StreamController<int>();
    final got = <int>[];
    c.stream.listen(got.add, onDone: () => done = true);
    await countTo(n: 2, sink: c);
    await pumpEventQueue();
    expect(got, [1, 2]);
    expect(done, isTrue);
    expect(Frustrate.instance.openChannelCount, 0);
  });

  test('a stream the producer closes ends the consumer`s stream', () async {
    final sink = StreamController<int>();
    final got = <int>[];
    final done = sink.stream.listen(got.add).asFuture<void>();
    emitTwo(sink: sink);
    await done;
    expect(got, [1, 2]);
  });

  test('a void Dart callback', () async {
    final seen = <int>[];
    await notifyN(n: 3, cb: seen.add);
    await pumpEventQueue();
    expect(seen, [0, 1, 2]);
  });

  test('a value-returning Dart closure round-trips', () async {
    expect(await transform(x: 20, f: (x) => x + 1), 21);

    final counter = Counter.new_();
    expect(await counter.holdWriteAsking(f: (v) => v + 7), 7);
    expect(api.counter!.value, 7);
    counter.dispose();
  });

  test(
    'a dart_interface: one channel per method, and a refusal as a value',
    () async {
      final auditor = _Auditor();
      expect(await audit(amount: 5, currency: 'EUR', a: auditor), 'reserved 5');
      expect(auditor.notes, ['auditing 5 EUR']);

      auditor.refuseAfterMs = 30;
      final refused = await audit(amount: 5, currency: 'EUR', a: auditor);
      expect(refused, contains('refused as a value'));

      auditor.approves = false;
      expect(await audit(amount: 5, currency: 'EUR', a: auditor), 'refused');
    },
  );

  test('a Vec of interfaces nested in a struct', () async {
    final a = _Auditor();
    final b = _Auditor();
    expect(
      await runAudit(
        run: AuditRun(label: 'q1', auditors: [a, b]),
        amount: 4,
      ),
      8,
    );
    expect(a.notes, ['q1']);
    expect(b.notes, ['q1']);
  });

  test('handles nested in a struct reach the fake as their own sinks', () async {
    final evens = StreamController<int>();
    final odds = StreamController<int>();
    final e = <int>[];
    final o = <int>[];
    evens.stream.listen(e.add);
    odds.stream.listen(o.add);

    await splitParity(
      n: 5,
      out: Fanout(evens: evens, odds: odds),
    );
    await pumpEventQueue();
    expect(e, [0, 2, 4]);
    expect(o, [1, 3]);

    // Two channels, two sinks, each independently cancellable — the property a
    // single shared mirror would lose.
    api.fanout!.evens.close();
    api.fanout!.odds.close();
    await pumpEventQueue();
  });

  test('a closure nested in a struct', () async {
    expect(
      await applyTransform(x: 21, t: Transforms(double: (x) => x * 2)),
      42,
    );
  });

  test(
    'an actor: spawn, a plain method, a deferred completion, dispose',
    () async {
      final miner = await Miner.new_(label: 'fake');
      expect(await miner.label(), 'fake');

      final token = FrustrateCancelToken();
      final waiting = miner.deferredWait(token: 1, cancel: token);
      // The executor was released, so a later call answers first — the whole
      // point of `Deferred<T>`.
      await miner.release(value: 9);
      expect(await waiting, 10);

      await expectLater(
        miner.withdrawFrom(amount: 1 << 20),
        throwsA(isA<WithdrawErrorException>()),
      );

      // The synthetic drop is an ordinary host call; the arm retires the
      // registry entry, and the host stops.
      await miner.dispose();
      expect(Frustrate.instance.inFlightCallCount, 0);
    },
  );

  test('a deferred completion outstanding at dispose is cancelled', () async {
    final miner = await Miner.new_(label: 'fake');
    final waiting = miner.deferredWait(token: 1);
    await pumpEventQueue();
    // The handler goes on *before* the dispose that rejects it: a rejected
    // future nobody is listening to for a turn is an unhandled async error,
    // which is ordinary Dart rather than anything the fake adds — and the same
    // order a cancelling caller has to use against the real bridge.
    final rejected = expectLater(
      waiting,
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('Miner'),
        ),
      ),
    );
    await miner.dispose();
    await rejected;
  });

  test('an actor callback registers on the host that carries it', () async {
    final miner = await Miner.new_(label: 'fake');
    final lines = <String>[];
    await miner.report(cb: lines.add);
    await pumpEventQueue();
    expect(lines, ['mined', 'again']);
    await miner.dispose();
  });

  test('trait handles, in both directions', () async {
    // Returned as the trait: the caller gets the generated dyn implementor,
    // and passing it back resolves to the same fake object.
    final tally = newTally(kind: 'steps', step: 3);
    expect(tally.bump(), 3);
    expect(bumpTwice(t: tally), 9);
    expect(tally.describe(), 'steps(9)');
    tally.dispose();

    // A concrete implementor is usable where the trait is, on both sides: the
    // generated `Abacus` implements `Tally`, and `FakeAbacus` implements
    // `FakeTally`.
    final abacus = Abacus.new_();
    expect(bumpTwice(t: abacus), 2);
    abacus.dispose();

    // A trait lent and taken from *inside* a container: each element carries
    // its own impl tag, which the harness reads and drops — one registry.
    final mixed = newTally(kind: 'steps', step: 2);
    mixed.bump();
    final abacus2 = Abacus.new_();
    abacus2.bump();
    expect(tallySum(ts: [mixed, abacus2]), 3);
    expect(settleTallies(ts: [mixed.take(), abacus2.take()]), 200 + 1000);
    expect(mixed.isDisposed, isTrue);
    expect(abacus2.isDisposed, isTrue);

    // A Vec of trait handles: one mint per element.
    final gs = greeters(kinds: ['loud', 'soft']);
    expect(gs[0].greet(name: 'ana'), 'loud: ana');
    expect(await gs[1].greetMany(names: ['bo']), ['soft: bo']);
    // A trait member that returns the trait: minted here, decoded there.
    final shouty = await gs[0].louder();
    expect(shouty.greet(name: 'ana'), 'LOUD: ana');
    shouty.dispose();
    for (final g in gs) {
      g.dispose();
    }
  });

  test('generic data types reach the fake as concrete types', () async {
    // The template is invisible here: the fake declares `Page<Item>`, and the
    // harness decodes with the same per-instantiation codecs the client
    // encodes with.
    final p = itemPage(n: 2);
    expect(p.items.map((i) => i.label), ['item0', 'item1']);
    expect(doublePage(p: const Page<int>(items: [1, 2], total: 3)).items, [
      2,
      4,
    ]);
    expect(eitherSum(e: EitherLeft<int, Item>(5)), 5);
    expect(eitherSum(e: EitherRight<int, Item>(Item(id: 9, label: 'nine'))), 9);

    // The one shape that differs: an instantiation reaching a handle gets a
    // mirror class, generic in the same parameter as the client's.
    final docs = docPage(n: 2);
    expect(docs.items, hasLength(2));
    expect(docs.items[0].text(), 'doc0');
    for (final d in docs.items) {
      d.dispose();
    }
  });
}
