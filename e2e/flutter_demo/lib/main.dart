// The frustrate demo gallery: one portable Flutter app whose state and logic
// live in Rust, exercised through the generated bindings. This file is
// byte-identical across macOS, web (single-threaded and threaded), and — as
// they land — iOS, Android, and Linux; only the bundled native artifact
// differs. It ramps from the basics through the value & data-type surface, the
// concurrency models, streams & callbacks, trait objects, and external
// (protobuf-style) types, so a reader can see each feature drive real UI, and
// ends by watching the bridge itself — every call the app makes, and every
// record its Rust logs.

import 'dart:async';
import 'dart:typed_data';

import 'package:demo_bridge/demo_rust.frustrate.dart';
import 'package:demo_bridge/fake_plan.dart';
import 'package:demo_bridge/init.dart';
import 'package:demo_bridge/native_features.dart';
import 'package:flutter/material.dart';
import 'package:frustrate/frustrate.dart'
    show
        ActorHost,
        BinaryReader,
        ContentionException,
        Frustrate,
        FrustrateCancelToken;
// The opt-in for a call decorator. Importing it is what makes
// `FrustrateRuntime` polymorphic in this app — an app that never imports it
// links one implementation and the compiler devirtualises every crossing — so
// the import is deliberate and belongs to the card at the bottom of the
// gallery. Nothing here is timed, so the demo pays that willingly.
import 'package:frustrate/intercept.dart';
import 'package:frustrate/workers.dart';

Future<void> main() async {
  await initBridge();
  // Before the first frame, so the record `greet` logs while `_GreetingCard`
  // builds is already on the page when the reader reaches the card that shows
  // it. This is also where an app would do it: once, from `main()`, not from a
  // widget that can be disposed.
  _rustLog.install();
  runApp(const DemoApp());
}

class DemoApp extends StatelessWidget {
  const DemoApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'frustrate demo',
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: Colors.deepOrange),
        useMaterial3: true,
      ),
      home: const GalleryPage(),
    );
  }
}

class GalleryPage extends StatelessWidget {
  const GalleryPage({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        title: const Text('frustrate demo'),
      ),
      body: ListView(
        // What an agent names to scroll a card into view (app.scrollIntoView's
        // scrollableKey); the list builds its children lazily.
        key: const Key('gallery'),
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 12),
        children: const [
          _SectionHeader('Basics'),
          _GreetingCard(),
          _CounterCard(),
          _PrimeCard(),
          _SectionHeader('Values & data types'),
          _ScalarsCard(),
          _HistogramCard(),
          _CollectionsCard(),
          _RecordCard(),
          _DataClassCard(),
          _SectionHeader('Concurrency models'),
          _SnapshotCard(),
          _LedgerCard(),
          _ActorRaceCard(),
          _PoolRaceCard(),
          _AsyncFnCard(),
          _SectionHeader('Streams & callbacks'),
          _LiveDocCard(),
          _MineProgressCard(),
          _AwaitedCallbackCard(),
          _NativeCallbackCard(),
          _RuntimeFailCard(),
          _SectionHeader('Traits'),
          _GreeterCard(),
          _SectionHeader('External types'),
          _PlanCodecCard(),
          _PatchInspectorCard(),
          _SectionHeader('The standard library on web'),
          _StdFacilitiesCard(),
          _SectionHeader('Watching the bridge'),
          _CallTraceCard(),
          _RustLogCard(),
          SizedBox(height: 24),
        ],
      ),
    );
  }
}

// --------------------------------------------------------------- chrome --

class _SectionHeader extends StatelessWidget {
  final String title;
  const _SectionHeader(this.title);

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.only(top: 20, bottom: 4, left: 4),
      child: Text(
        title,
        style: Theme.of(context)
            .textTheme
            .titleSmall
            ?.copyWith(color: Theme.of(context).colorScheme.primary),
      ),
    );
  }
}

class _SectionCard extends StatelessWidget {
  final String title;
  final String subtitle;
  final Widget child;
  const _SectionCard(
      {required this.title, required this.subtitle, required this.child});

  @override
  Widget build(BuildContext context) {
    return Card(
      margin: const EdgeInsets.symmetric(vertical: 6),
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(title, style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 2),
            Text(subtitle, style: Theme.of(context).textTheme.bodySmall),
            const SizedBox(height: 12),
            child,
          ],
        ),
      ),
    );
  }
}

String _fmtPatch(TextPatch p) => switch (p) {
      TextPatchSplice(:final index, :final text) => 'Splice @$index "$text"',
      TextPatchDelete(:final index, :final length) => 'Delete @$index ×$length',
      TextPatchMark(:final field0, :final field1) => 'Mark $field0=$field1',
      TextPatchClear() => 'Clear',
    };

// ================================================================ basics ==

class _GreetingCard extends StatelessWidget {
  const _GreetingCard();

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'greet — sync free function',
      subtitle: 'A String crosses to Rust and back, synchronously.',
      child: Text(greet(name: 'Flutter'),
          key: const Key('greet'), style: const TextStyle(fontSize: 18)),
    );
  }
}

class _CounterCard extends StatefulWidget {
  const _CounterCard();
  @override
  State<_CounterCard> createState() => _CounterCardState();
}

class _CounterCardState extends State<_CounterCard> {
  final Counter _counter = Counter.new_();
  late int _count = _counter.value();

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'Counter — Confined opaque handle',
      subtitle: 'Single-owner state on the UI isolate; sync methods run on '
          'the caller (no shadow cache).',
      child: Row(
        children: [
          Text('$_count',
              key: const Key('counter'),
              style: Theme.of(context).textTheme.headlineSmall),
          const Spacer(),
          FilledButton(
            onPressed: () => setState(() => _count = _counter.increment()),
            child: const Text('increment'),
          ),
        ],
      ),
    );
  }
}

class _PrimeCard extends StatefulWidget {
  const _PrimeCard();
  @override
  State<_PrimeCard> createState() => _PrimeCardState();
}

class _PrimeCardState extends State<_PrimeCard> {
  int _n = 10000;
  late Future<int> _prime = nthPrime(n: _n);

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'nthPrime — async free function',
      subtitle: 'Brute-force CPU work completed via a Future (on the pool '
          'where one exists).',
      child: Row(
        children: [
          Expanded(
            child: FutureBuilder<int>(
              future: _prime,
              builder: (context, snap) => snap.hasData
                  ? Text('The ${_n}th prime is ${snap.data}',
                      key: const Key('prime'))
                  : const LinearProgressIndicator(),
            ),
          ),
          const SizedBox(width: 12),
          FilledButton.tonal(
            onPressed: () => setState(() {
              _n += 10000;
              _prime = nthPrime(n: _n);
            }),
            child: const Text('bigger'),
          ),
        ],
      ),
    );
  }
}

// ==================================================== values & data types ==

class _ScalarsCard extends StatefulWidget {
  const _ScalarsCard();
  @override
  State<_ScalarsCard> createState() => _ScalarsCardState();
}

class _ScalarsCardState extends State<_ScalarsCard> {
  static const _chars = ['A', 'é', '🦀'];
  // crab() mints the astral scalar Rust-side; the picker echoes any single
  // character back through echo_char.
  late String _pick = crab();
  late String _echoed = echoChar(c: _pick);
  final BigIntExtremes _big = bigIntExtremes();

  void _select(String c) => setState(() {
        _pick = c;
        _echoed = echoChar(c: c);
      });

  @override
  Widget build(BuildContext context) {
    final code = _echoed.runes.first;
    final hex = code.toRadixString(16).toUpperCase().padLeft(4, '0');
    return _SectionCard(
      title: 'Scalars — char and 128-bit integers',
      subtitle: 'A Rust char is one Unicode scalar (it crosses as a Dart '
          'String); i128/u128 run past 2^63, so they arrive as BigInt.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SegmentedButton<String>(
            segments: [
              for (final c in _chars) ButtonSegment(value: c, label: Text(c)),
            ],
            selected: {_pick},
            onSelectionChanged: (s) => _select(s.first),
          ),
          const SizedBox(height: 8),
          Text('char echoed by Rust: $_echoed  (U+$hex)',
              key: const Key('charScalar')),
          const SizedBox(height: 8),
          Text('u128 max = ${_big.u128Max}',
              key: const Key('bigIntScalar'),
              style: const TextStyle(fontFamily: 'monospace', fontSize: 12)),
        ],
      ),
    );
  }
}

class _HistogramCard extends StatefulWidget {
  const _HistogramCard();
  @override
  State<_HistogramCard> createState() => _HistogramCardState();
}

class _HistogramCardState extends State<_HistogramCard> {
  final _input = TextEditingController(text: 'hello world');
  late Int32List _counts = letterHistogram(text: _input.text);

  void _recompute(String s) => setState(() => _counts = letterHistogram(text: s));

  @override
  void dispose() {
    _input.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    var maxCount = 0;
    var topIdx = 0;
    for (var i = 0; i < _counts.length; i++) {
      if (_counts[i] > maxCount) {
        maxCount = _counts[i];
        topIdx = i;
      }
    }
    final topLetter = String.fromCharCode('a'.codeUnitAt(0) + topIdx);
    return _SectionCard(
      title: 'Histogram — Vec<i32> as an Int32List',
      subtitle: 'A typed list: the 26 a-z counts cross as an Int32List, its '
          'element type fixed at 32-bit — not a boxed List<int>.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          TextField(
            controller: _input,
            decoration: const InputDecoration(
                border: OutlineInputBorder(), labelText: 'text to count'),
            onChanged: _recompute,
          ),
          const SizedBox(height: 12),
          SizedBox(
            height: 48,
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                for (var i = 0; i < 26; i++)
                  Expanded(
                    child: Padding(
                      padding: const EdgeInsets.symmetric(horizontal: 1),
                      child: Container(
                        height:
                            maxCount == 0 ? 0 : 48.0 * _counts[i] / maxCount,
                        color: Theme.of(context).colorScheme.primary,
                      ),
                    ),
                  ),
              ],
            ),
          ),
          const SizedBox(height: 8),
          Text(
            maxCount == 0
                ? 'no letters yet'
                : 'tallest bucket: $topLetter = $maxCount',
            key: const Key('histogram'),
          ),
        ],
      ),
    );
  }
}

class _CollectionsCard extends StatefulWidget {
  const _CollectionsCard();
  @override
  State<_CollectionsCard> createState() => _CollectionsCardState();
}

/// True under dart2js and DDC — the web dev loop's compiler — where `int` is a
/// JavaScript number and `Int64List` does not exist, so a `Vec<i64>` cannot
/// cross at all. dart2wasm has both. The same test frustrate's runtime makes.
const bool _jsNumbers = bool.fromEnvironment('dart.library.js_interop') &&
    !bool.fromEnvironment('dart.tool.dart2wasm');

class _CollectionsCardState extends State<_CollectionsCard> {
  final _input = TextEditingController(text: '5, 3, 5, 1, 3, 5');
  Map<int, int> _tally = const {};
  Set<int> _distinct = const {};
  List<int> _rotated = const [];

  @override
  void initState() {
    super.initState();
    if (!_jsNumbers) _compute(_input.text);
  }

  void _compute(String s) {
    final xs = Int64List.fromList(s
        .split(',')
        .map((t) => int.tryParse(t.trim()))
        .whereType<int>()
        .toList());
    _tally = tallySorted(xs: xs);
    _distinct = distinctSorted(xs: xs);
    _rotated = rotateLeft(xs: xs, by: 2);
  }

  void _recompute(String s) => setState(() => _compute(s));

  @override
  void dispose() {
    _input.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final tally = _tally.entries.map((e) => '${e.key}:${e.value}').join('  ');
    return _SectionCard(
      title: 'Collections — ordered map, set, and deque',
      subtitle: 'Same numbers, three standard containers. A BTreeMap and '
          'BTreeSet arrive sorted (a HashMap/HashSet would not); a VecDeque '
          'shares the Int64List wire with Vec.',
      child: _jsNumbers
          ? const Text(
              'Not on this build: these cross as Int64List, which a '
              'JavaScript-compiled app does not have. Run it with --wasm or '
              'natively.',
              key: Key('collectionsUnavailable'))
          : Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              TextField(
                controller: _input,
                decoration: const InputDecoration(
                    border: OutlineInputBorder(), labelText: 'comma-separated ints'),
                onChanged: _recompute,
              ),
              const SizedBox(height: 10),
              Text('BTreeMap value:count — $tally', key: const Key('orderedMap')),
              const SizedBox(height: 4),
              Text('BTreeSet distinct — {${_distinct.join(', ')}}',
                  key: const Key('orderedSet')),
              const SizedBox(height: 4),
              Text('VecDeque rotate-left 2 — ${_rotated.join(', ')}',
                  key: const Key('deque')),
            ],
          ),
    );
  }
}

class _RecordCard extends StatefulWidget {
  const _RecordCard();
  @override
  State<_RecordCard> createState() => _RecordCardState();
}

class _RecordCardState extends State<_RecordCard> {
  final _input = TextEditingController(text: 'frustrate');
  late (String, (int, int)) _m = measure(text: _input.text);

  void _recompute(String s) => setState(() => _m = measure(text: s));

  @override
  void dispose() {
    _input.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    // Nested record destructuring: (text, (chars, bytes)).
    final (text, (chars, bytes)) = _m;
    return _SectionCard(
      title: 'Records — a Rust tuple as a Dart record',
      subtitle: 'measure returns (String, (int, int)); Dart reads the nested '
          'record positionally as .\$1 and .\$2.\$1 / .\$2.\$2.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          TextField(
            controller: _input,
            decoration: const InputDecoration(
                border: OutlineInputBorder(), labelText: 'text to measure'),
            onChanged: _recompute,
          ),
          const SizedBox(height: 8),
          Text('"$text" → $chars chars, $bytes bytes',
              key: const Key('record')),
        ],
      ),
    );
  }
}

class _DataClassCard extends StatefulWidget {
  const _DataClassCard();
  @override
  State<_DataClassCard> createState() => _DataClassCardState();
}

class _DataClassCardState extends State<_DataClassCard> {
  // Toggling the nullable field re-runs equality, Set-dedup, and copyWith so
  // the generated methods are exercised live, not dumped once.
  String? _holder = 'Ada';

  void _toggle() =>
      setState(() => _holder = _holder == null ? 'Ada' : null);

  @override
  Widget build(BuildContext context) {
    final pass = Passport(id: 1, holder: _holder, tags: const ['alpha']);
    // Round-trip through Rust: the returned instance is value-equal, so it
    // compares == and dedups in a Set.
    final round = echoPassport(p: pass);
    final passEq = pass == round;
    final passSet = {pass, round};
    final nulled = pass.copyWith(holder: null);
    final renamed = pass.copyWith(id: 9);

    // The no_eq sibling: value-equal but identity-compared, so two never dedup.
    final ticket = Ticket(code: 7, note: 'vip');
    final ticketEcho = echoTicket(t: ticket);
    final ticketEq = ticket == ticketEcho;
    final ticketSet = {ticket, ticketEcho};

    return _SectionCard(
      title: 'Data classes — value equality & copyWith',
      subtitle: 'Passport has generated value equality and copyWith; the '
          '#[bridge(no_eq)] Ticket keeps identity equality for contrast.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('round-trip Passport == local: $passEq · Set keeps '
              '${passSet.length}', key: const Key('valueEq')),
          const SizedBox(height: 4),
          Text('copyWith(id: 9).id = ${renamed.id} · '
              'copyWith(holder: null).holder = ${nulled.holder}',
              key: const Key('copyWith')),
          const SizedBox(height: 4),
          Text('two equal Tickets == : $ticketEq · Set keeps '
              '${ticketSet.length}', key: const Key('noEq')),
          const SizedBox(height: 8),
          FilledButton.tonal(
              onPressed: _toggle, child: const Text('toggle holder')),
        ],
      ),
    );
  }
}

// ==================================================== concurrency models ==

class _SnapshotCard extends StatefulWidget {
  const _SnapshotCard();
  @override
  State<_SnapshotCard> createState() => _SnapshotCardState();
}

class _SnapshotCardState extends State<_SnapshotCard> {
  final Snapshot _snap = Snapshot.build(words: const ['frozen', 'shared', 'immutable']);
  late final int _count = _snap.wordCount();
  late final Future<String> _joined = _snap.join(sep: ' · ');

  @override
  void dispose() {
    _snap.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'Snapshot — Frozen (shared Arc)',
      subtitle: 'One immutable handle: a sync read and an async method share '
          'it — no copy.',
      child: FutureBuilder<String>(
        future: _joined,
        builder: (context, snap) => Text(
          '$_count words — ${snap.data ?? '…'}',
          key: const Key('snapshot'),
        ),
      ),
    );
  }
}

class _LedgerCard extends StatefulWidget {
  const _LedgerCard();
  @override
  State<_LedgerCard> createState() => _LedgerCardState();
}

class _LedgerCardState extends State<_LedgerCard> {
  final Ledger _ledger = Ledger.new_();
  String _status = 'balance 0';
  bool _holding = false;

  @override
  void dispose() {
    _ledger.dispose();
    super.dispose();
  }

  Future<void> _add() async {
    final v = await _ledger.add(delta: 10);
    setState(() => _status = 'balance $v');
  }

  void _holdWrite() {
    setState(() => _holding = true);
    // Not awaited: the write lock is held on the pool for 800ms, giving the
    // sync read below a window to observe contention.
    _ledger.holdWrite(millis: 800).whenComplete(() {
      if (mounted) setState(() => _holding = false);
    });
  }

  void _readNow() {
    try {
      final v = _ledger.tryGet();
      setState(() => _status = 'read balance $v');
    } on ContentionException catch (e) {
      setState(() => _status = 'CONTENDED — ${e.message}');
    }
  }

  @override
  Widget build(BuildContext context) {
    final contended = _status.startsWith('CONTENDED');
    return _SectionCard(
      title: 'Ledger — Locked (RwLock) with contention contract',
      subtitle: 'Async writes take the lock on the pool; the sync tryGet is '
          'marked on_contention="error", so a contended read throws an '
          'attributable ContentionException. It runs on every target, browser '
          'main thread included: the try-lock is a compare-exchange and the '
          'release reaches no wait instruction, so this is the one synchronous '
          'way to touch shared state directly.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(_status,
              key: const Key('ledger'),
              style: TextStyle(
                  color: contended ? Theme.of(context).colorScheme.error : null)),
          const SizedBox(height: 8),
          Wrap(
            spacing: 8,
            children: [
              FilledButton.tonal(onPressed: _add, child: const Text('add 10')),
              // Disabled where async is not parallel — single-threaded web,
              // where a bridged `async fn` runs inline on the one thread, so
              // the write lock is released before `read now` could possibly be
              // pressed. The button would appear to work and never contend,
              // which is a worse demonstration than an obviously unavailable
              // control. The subtitle above scopes the same claim in prose.
              // On single-threaded web `read now` therefore always succeeds:
              // there is nothing that could be holding the lock when it runs.
              FilledButton.tonal(
                onPressed: !Frustrate.instance.asyncIsParallel || _holding
                    ? null
                    : _holdWrite,
                child: Text(_holding ? 'holding write…' : 'hold write 800ms'),
              ),
              FilledButton(onPressed: _readNow, child: const Text('read now')),
            ],
          ),
        ],
      ),
    );
  }
}

class _ActorRaceCard extends StatefulWidget {
  const _ActorRaceCard();
  @override
  State<_ActorRaceCard> createState() => _ActorRaceCardState();
}

class _ActorRaceCardState extends State<_ActorRaceCard> {
  String? _raceResult;
  bool _racing = false;

  // The same CPU work serialized on one Prospector actor, then fanned across
  // an ActorPool of four — each pool instance owns its own executor (a thread
  // on native, a Worker on web), so the fan-out runs genuinely in parallel.
  Future<void> _race() async {
    setState(() {
      _racing = true;
      _raceResult = null;
    });
    const work = 20000;

    final solo = await Prospector.new_();
    final serial = Stopwatch()..start();
    for (var i = 0; i < 4; i++) {
      await solo.dig(n: work);
    }
    serial.stop();
    await solo.dispose();

    final team = await ActorPool.spawn((_) => Prospector.new_(), size: 4);
    final fanned = Stopwatch()..start();
    await Future.wait(List.generate(4, (_) => team.run((p) => p.dig(n: work))));
    fanned.stop();
    await team.dispose();

    final speedup = serial.elapsedMicroseconds / fanned.elapsedMicroseconds;
    setState(() {
      _racing = false;
      _raceResult = '4 actors ran ${speedup.toStringAsFixed(1)}x faster than '
          'one (${serial.elapsedMilliseconds}ms serialized, '
          '${fanned.elapsedMilliseconds}ms fanned out)';
    });
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'Prospector — Actor (web parallelism)',
      subtitle: 'One actor owns one executor. Fan the same work across four via '
          'ActorPool and it runs in parallel — in the browser too, on stable '
          'Rust.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          FilledButton.tonal(
            onPressed: _racing ? null : _race,
            child: const Text('Race 4 actors against 1'),
          ),
          const SizedBox(height: 8),
          if (_racing)
            const LinearProgressIndicator()
          else if (_raceResult != null)
            Text(_raceResult!, key: const Key('race')),
        ],
      ),
    );
  }
}

class _PoolRaceCard extends StatefulWidget {
  const _PoolRaceCard();
  @override
  State<_PoolRaceCard> createState() => _PoolRaceCardState();
}

class _PoolRaceCardState extends State<_PoolRaceCard> {
  String? _result;
  bool _racing = false;

  // The same async work awaited one call at a time, then issued concurrently.
  // Where the pool is real (native threads, threaded-web workers) the batch
  // runs genuinely in parallel; on single-threaded web async is inline and the
  // ratio honestly renders ~1.0x. Same app code everywhere — the runtime
  // detects threading support from the module. (async ≠ parallel.)
  Future<void> _race() async {
    setState(() {
      _racing = true;
      _result = null;
    });
    const n = 20000;
    await nthPrime(n: 100); // spawn the pool before timing it

    final serial = Stopwatch()..start();
    for (var i = 0; i < 4; i++) {
      await nthPrime(n: n);
    }
    serial.stop();

    final concurrent = Stopwatch()..start();
    await Future.wait(List.generate(4, (_) => nthPrime(n: n)));
    concurrent.stop();

    final speedup = serial.elapsedMicroseconds / concurrent.elapsedMicroseconds;
    setState(() {
      _racing = false;
      _result = 'async pool ran ${speedup.toStringAsFixed(1)}x faster than '
          'serial (${serial.elapsedMilliseconds}ms serial, '
          '${concurrent.elapsedMilliseconds}ms concurrent)';
    });
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'Async pool — parallel free-function calls',
      subtitle: 'async ≠ parallel: four async calls serial vs concurrent. Real '
          'speedup on native and threaded web; honestly ~1.0x single-threaded.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          FilledButton.tonal(
            onPressed: _racing ? null : _race,
            child: const Text('Race the async pool'),
          ),
          const SizedBox(height: 8),
          if (_racing)
            const LinearProgressIndicator()
          else if (_result != null)
            Text(_result!, key: const Key('poolRace')),
        ],
      ),
    );
  }
}

class _AsyncFnCard extends StatefulWidget {
  const _AsyncFnCard();
  @override
  State<_AsyncFnCard> createState() => _AsyncFnCardState();
}

class _AsyncFnCardState extends State<_AsyncFnCard> {
  // A real Rust `async fn` (its body `.await`s) surfaced as a Dart Future.
  // cooperativeYield(21, rounds: 3) suspends three times, then returns 42.
  late final Future<int> _one = cooperativeYield(x: 21, rounds: 3);
  String? _batch;
  bool _running = false;

  Future<void> _fanOut() async {
    setState(() {
      _running = true;
      _batch = null;
    });
    const count = 1000;
    final sw = Stopwatch()..start();
    // 1000 async fns in flight at once. On single-threaded web there is ONE
    // thread — they all resolve only because each suspended future is heap
    // data the cooperative executor multiplexes, not a parked thread. A
    // block_on-per-call model would deadlock here; this is the decisive proof
    // the executor is real, running in a real app on every platform.
    final results =
        await Future.wait(
            List.generate(count, (i) => cooperativeYield(x: i, rounds: 2)));
    sw.stop();
    final ok = List.generate(count, (i) => results[i] == i * 2).every((b) => b);
    setState(() {
      _running = false;
      _batch = '$count/$count resolved in ${sw.elapsedMilliseconds}ms '
          '(${ok ? 'all correct' : 'MISMATCH'})';
    });
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'cooperativeYield — Rust async fn (.await)',
      subtitle: 'A true async fn whose body suspends and resumes on the '
          'cooperative executor. Unlike nthPrime (sync body, async dispatch), '
          'it yields to the event loop between polls — so it runs even on '
          'single-threaded web, and thousands multiplex on one thread.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          FutureBuilder<int>(
            future: _one,
            builder: (context, snap) => snap.hasData
                ? Text('cooperativeYield(21, rounds: 3) = ${snap.data}',
                    key: const Key('asyncFn'))
                : const LinearProgressIndicator(),
          ),
          const SizedBox(height: 12),
          FilledButton.tonal(
            onPressed: _running ? null : _fanOut,
            child: const Text('Fire 1000 concurrently'),
          ),
          const SizedBox(height: 8),
          if (_running)
            const LinearProgressIndicator()
          else if (_batch != null)
            Text(_batch!, key: const Key('asyncFnBatch')),
        ],
      ),
    );
  }
}

// ==================================================== streams & callbacks ==

class _LiveDocCard extends StatefulWidget {
  const _LiveDocCard();
  @override
  State<_LiveDocCard> createState() => _LiveDocCardState();
}

class _LiveDocCardState extends State<_LiveDocCard> {
  final TextDoc _doc = TextDoc.new_();
  final List<TextPatch> _log = [];
  int _len = 0;
  // The sink is an ordinary parameter: we own the controller, Rust holds the
  // handle. Cancelling _sub fires the controller's onCancel, which is what
  // stops the Rust producer.
  final StreamController<TextPatch> _patches = StreamController<TextPatch>();
  late final StreamSubscription<TextPatch> _sub;

  @override
  void initState() {
    super.initState();
    _doc.watch(sink: _patches);
    _sub = _patches.stream.listen((p) {
      setState(() {
        _log.add(p);
        if (_log.length > 6) _log.removeAt(0);
      });
    });
    // Closure flavor of the watch pattern: fired with the new length after
    // each splice (never during the splice call).
    _doc.onChange(cb: (len) {
      if (mounted) setState(() => _len = len);
    });
  }

  @override
  void dispose() {
    _sub.cancel();
    _doc.dispose();
    super.dispose();
  }

  void _onChanged(String value) {
    // Sync splice on a Confined handle: replace the whole content. This pushes
    // Delete/Splice patches to the watch stream and fires onChange.
    _doc.splice(index: 0, delete: _doc.lenChars(), insert: value);
  }

  void _appendHi() {
    // A deterministic one-tap splice (append), for when you don't want to
    // type: it pushes a Splice patch to the stream and fires onChange.
    _doc.splice(index: _doc.lenChars(), delete: 0, insert: 'hi');
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'TextDoc — StreamSink watch + DartCallback (automerge pattern)',
      subtitle: 'Editing calls sync splice on a Confined doc; a stored '
          'StreamSink relays each patch (StreamBuilder pane), and a stored Dart '
          'closure reports the new length.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: TextField(
                  key: const Key('liveDocInput'),
                  decoration: const InputDecoration(
                      border: OutlineInputBorder(), hintText: 'type to splice…'),
                  onChanged: _onChanged,
                ),
              ),
              const SizedBox(width: 8),
              FilledButton.tonal(
                  onPressed: _appendHi, child: const Text('append hi')),
            ],
          ),
          const SizedBox(height: 8),
          Text('length (via callback): $_len', key: const Key('liveDoc')),
          const SizedBox(height: 4),
          Text(
            _log.isEmpty
                ? 'patches will stream here'
                : _log.map(_fmtPatch).join('\n'),
            key: const Key('liveDocPatches'),
            style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
          ),
        ],
      ),
    );
  }
}

class _MineProgressCard extends StatefulWidget {
  const _MineProgressCard();
  @override
  State<_MineProgressCard> createState() => _MineProgressCardState();
}

class _MineProgressCardState extends State<_MineProgressCard> {
  Prospector? _actor;
  final List<int> _primes = [];
  bool _running = false;

  Future<void> _mine() async {
    setState(() {
      _running = true;
      _primes.clear();
    });
    final actor = await Prospector.new_();
    _actor = actor;
    // Progress items relay out while the method is still running on the
    // actor's executor — real-time streaming during long CPU work. The sink
    // is a parameter, so the controller is ours and the call is an ordinary
    // Future: listen for items, await the call for completion.
    final progress = StreamController<int>();
    final sub = progress.stream.listen((prime) {
      if (mounted) setState(() => _primes.add(prime));
    });
    await actor.mineProgress(rounds: 5, n: 8000, sink: progress);
    await sub.cancel();
    await actor.dispose();
    if (mounted) setState(() => _running = false);
  }

  @override
  void dispose() {
    _actor?.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'mineProgress — actor progress stream',
      subtitle: 'You pass the StreamController; the actor relays each result '
          'into it as it is computed, not batched at the end.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          FilledButton.tonal(
            onPressed: _running ? null : _mine,
            child: const Text('mine 5 rounds'),
          ),
          const SizedBox(height: 8),
          Text(
            _primes.isEmpty ? '—' : _primes.join(', '),
            key: const Key('mineProgress'),
          ),
        ],
      ),
    );
  }
}

class _AwaitedCallbackCard extends StatefulWidget {
  const _AwaitedCallbackCard();
  @override
  State<_AwaitedCallbackCard> createState() => _AwaitedCallbackCardState();
}

class _AwaitedCallbackCardState extends State<_AwaitedCallbackCard> {
  final _input = TextEditingController(text: '9');
  String _result = '…';

  @override
  void initState() {
    super.initState();
    _run();
  }

  Future<void> _run() async {
    final n = int.tryParse(_input.text.trim()) ?? 0;
    // A Rust `async fn` awaits a value back from this Dart closure via
    // call_async — the portable path, so it resolves on web too.
    final squared = await transform(x: n, f: (x) => x * x);
    if (mounted) setState(() => _result = 'transform($n) = $squared');
  }

  @override
  void dispose() {
    _input.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'transform — awaited value-returning callback (portable)',
      subtitle: 'An async fn .awaits call_async on a Dart closure, so a value '
          'flows back from Dart to Rust on every platform — the web too.',
      child: Row(
        children: [
          SizedBox(
            width: 88,
            child: TextField(
              controller: _input,
              keyboardType: TextInputType.number,
              decoration: const InputDecoration(
                  border: OutlineInputBorder(), labelText: 'x'),
              onSubmitted: (_) => _run(),
            ),
          ),
          const SizedBox(width: 12),
          Expanded(child: Text(_result, key: const Key('awaitedCallback'))),
          const SizedBox(width: 8),
          FilledButton.tonal(
              onPressed: _run, child: const Text('map x→x²')),
        ],
      ),
    );
  }
}

class _NativeCallbackCard extends StatefulWidget {
  const _NativeCallbackCard();
  @override
  State<_NativeCallbackCard> createState() => _NativeCallbackCardState();
}

class _NativeCallbackCardState extends State<_NativeCallbackCard> {
  String _result = '—';

  Future<void> _run() async {
    // A plain pool fn blocks its worker per item while the Dart closure maps
    // it, then sums — the shape codegen keeps off the web surface entirely.
    final sum = await nativeTransformSum(const [1, 2, 3, 4], (x) => x * x);
    setState(() => _result = 'transformSum([1,2,3,4], x→x²) = $sum');
  }

  @override
  Widget build(BuildContext context) {
    if (!kHasNativeReturningCallbacks) {
      return _SectionCard(
        title: 'transform_sum — blocking callback (native-only)',
        subtitle: 'A plain pool fn that blocks a worker per item — not a web '
            'limit on callbacks themselves (transform above proves those cross '
            'on web).',
        child: Text(
          'A blocking value-returning callback parks a pool worker against the '
          'event loop, which is main-thread-fatal on web, so this member is '
          'compile-time absent from the web surface — the structural gate, not '
          'a runtime error. Run on macOS to see it work.',
          key: const Key('nativeCb'),
        ),
      );
    }
    return _SectionCard(
      title: 'transform_sum — blocking callback (native-only)',
      subtitle: 'A plain pool fn blocks its worker per item while a Dart '
          'closure maps it — native-only because it parks a worker, not '
          'because callbacks cannot reach web.',
      child: Row(
        children: [
          Expanded(child: Text(_result, key: const Key('nativeCb'))),
          const SizedBox(width: 12),
          FilledButton.tonal(onPressed: _run, child: const Text('run')),
        ],
      ),
    );
  }
}

class _RuntimeFailCard extends StatefulWidget {
  const _RuntimeFailCard();
  @override
  State<_RuntimeFailCard> createState() => _RuntimeFailCardState();
}

class _RuntimeFailCardState extends State<_RuntimeFailCard> {
  final Ledger _ledger = Ledger.new_();
  String _result = 'tap to read';

  @override
  void dispose() {
    _ledger.dispose();
    super.dispose();
  }

  void _read() {
    // blockingRead is opted onto the web surface with web="runtime_fail": it
    // compiles everywhere, returns the balance on native, and throws a loud
    // UnsupportedError on web — caught and shown here.
    try {
      final v = _ledger.blockingRead();
      setState(() => _result = 'blocking read → balance $v');
    } on UnsupportedError catch (e) {
      setState(() => _result = 'threw → ${e.message}');
    }
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'blockingRead — opt-in runtime-fail on web',
      subtitle: 'A blocking read is native-only, but opted onto the web '
          'surface with web="runtime_fail": present so portable code compiles, '
          'throwing an attributable UnsupportedError if called on web. Native '
          'returns the balance.',
      child: Row(
        children: [
          Expanded(child: Text(_result, key: const Key('runtimeFail'))),
          const SizedBox(width: 12),
          FilledButton.tonal(
              onPressed: _read, child: const Text('read now')),
        ],
      ),
    );
  }
}

// ================================================================ traits ==

class _GreeterCard extends StatefulWidget {
  const _GreeterCard();
  @override
  State<_GreeterCard> createState() => _GreeterCardState();
}

class _GreeterCardState extends State<_GreeterCard> {
  String _kind = 'plain';
  String _out = '—';

  Future<void> _pick(String kind) async {
    setState(() => _kind = kind);
    // One Dart interface, several Rust implementations behind the factory.
    final g = newGreeter(kind: kind);
    final base = g.greet(name: 'Flutter');
    final louder = await g.louder();
    final loud = louder.greet(name: 'Flutter');
    setState(() => _out = '$base   →(louder)→   $loud');
    g.dispose();
    louder.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'Greeter — trait object (Box<dyn Greeter>)',
      subtitle: 'One generated Dart interface backs several Rust impls; '
          'louder() mints a fresh handle.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SegmentedButton<String>(
            segments: const [
              ButtonSegment(value: 'plain', label: Text('plain')),
              ButtonSegment(value: 'pirate', label: Text('pirate')),
              ButtonSegment(value: 'robot', label: Text('robot')),
            ],
            selected: {_kind},
            onSelectionChanged: (s) => _pick(s.first),
          ),
          const SizedBox(height: 8),
          Text(_out, key: const Key('greeter')),
        ],
      ),
    );
  }
}

// ======================================================== external types ==

class _PlanCodecCard extends StatefulWidget {
  const _PlanCodecCard();
  @override
  State<_PlanCodecCard> createState() => _PlanCodecCardState();
}

class _PlanCodecCardState extends State<_PlanCodecCard> {
  final _title = TextEditingController(text: 'roadmap');
  FakePlan _plan = FakePlan('roadmap', 1);

  void _bump() {
    // FakePlan stands in for a protoc-generated message: it crosses as bytes
    // via user codecs on both sides, and the protoc class itself appears in
    // the generated signature.
    setState(() => _plan = bumpPlan(p: FakePlan(_title.text, _plan.revision)));
  }

  @override
  void dispose() {
    _title.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'FakePlan — external (protobuf-style) bytes codec',
      subtitle: 'A type both sides already own crosses as bytes; the Dart '
          'companion class appears directly in the binding signature.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          TextField(
            controller: _title,
            decoration: const InputDecoration(
                border: OutlineInputBorder(), labelText: 'plan title'),
          ),
          const SizedBox(height: 8),
          Text('${_plan.title} — revision ${_plan.revision}',
              key: const Key('planCodec')),
          const SizedBox(height: 8),
          FilledButton.tonal(
              onPressed: _bump, child: const Text('bump revision in Rust')),
        ],
      ),
    );
  }
}

class _PatchInspectorCard extends StatefulWidget {
  const _PatchInspectorCard();
  @override
  State<_PatchInspectorCard> createState() => _PatchInspectorCardState();
}

class _PatchInspectorCardState extends State<_PatchInspectorCard> {
  // Build every variant in Dart, round-trip through Rust, and pattern-match
  // the sealed result — the data-enum story end to end.
  late final List<TextPatch> _got = echoPatches(ps: const [
    TextPatchSplice(index: 2, text: 'hi'),
    TextPatchDelete(index: 0, length: 1),
    TextPatchMark('bold', 1),
    TextPatchClear(),
  ]);

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'TextPatch — data enum as a sealed Dart hierarchy',
      subtitle: 'Every Rust enum variant round-trips and is exhaustively '
          'switch-matched in Dart.',
      child: Text(
        _got.map(_fmtPatch).join('\n'),
        key: const Key('patchInspector'),
        style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
      ),
    );
  }
}

// ============================================== the standard library on web ==

class _StdFacilitiesCard extends StatefulWidget {
  const _StdFacilitiesCard();
  @override
  State<_StdFacilitiesCard> createState() => _StdFacilitiesCardState();
}

/// Five things ordinary Rust does that `wasm32-unknown-unknown` cannot: read a
/// clock, get entropy, print, count cores, sleep. This card renders on every
/// target and reports honestly on each — real numbers on macOS and on a
/// facility web build, `-1` on the default one — because the point is the
/// *difference*, and a card that vanished on the builds without the facilities
/// would hide exactly what it exists to show.
///
/// The four readings are taken in `initState` rather than behind a button: they
/// are sync, cheap, and the entropy one has to be on the page at load for
/// std_facilities.spec.js to compare seeds across two loads. `println!` and the
/// nap have side effects, so those stay buttons.
class _StdFacilitiesCardState extends State<_StdFacilitiesCard> {
  late final bool _present = stdFacilitiesPresent();
  late final int _wall = stdWallMicros();
  late final int _mono = stdMonotonicNanos();
  late final int _seed = stdRandomU64();
  late final int _cores = stdCores();

  int? _printed;
  int? _napped;
  String? _napError;
  bool _napping = false;

  void _println() {
    // Goes to the devtools console on a facility build, through std's own
    // `println!`. On the default web build std's stdout accepts the write and
    // discards it, and this returns 0 — silence, not an error, which is the
    // failure mode worth seeing.
    final n = stdConsoleWrite(msg: 'frustrate demo: println! from Rust');
    setState(() => _printed = n);
  }

  Future<void> _nap() async {
    setState(() {
      _napping = true;
      _napError = null;
      _napped = null;
    });
    final actor = await Prospector.new_();
    try {
      // On the actor's executor — a Worker on web, a dedicated thread on
      // macOS — where waiting is permitted. The same call on the UI isolate is
      // refused by the host rather than freezing the page, which is why this
      // card offers no button to try it there.
      final ms = await actor.nap(millis: 20);
      if (mounted) setState(() => _napped = ms);
    } catch (e) {
      if (mounted) setState(() => _napError = '$e');
    } finally {
      await actor.dispose();
      if (mounted) setState(() => _napping = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    // One line per facility, `name = value`, so the Playwright spec can read
    // them straight out of the aria snapshot.
    final readings = [
      'std.wall  = $_wall',
      'std.mono  = $_mono',
      'std.seed  = $_seed',
      'std.cores = $_cores',
      if (_printed != null) 'std.println = $_printed',
      if (_napped != null) 'std.nap = $_napped',
      if (_napError != null) 'std.nap = refused',
    ].join('\n');

    return _SectionCard(
      title: 'std::time, HashMap, println!, available_parallelism, sleep',
      subtitle: _present
          ? 'This build links a std whose unsupported stubs call the host, so '
              'std\'s own APIs work — in Rust that does not know it is on wasm.'
          : 'This build links the stock std, where these have no OS behind '
              'them: -1 is the sentinel for "no such facility". Build the '
              'module on @frustrate//bazel:wasm32_custom to see real values.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            'facilities: ${_present ? 'present' : 'absent'}',
            key: const Key('stdFacilitiesPresent'),
            style: TextStyle(
              fontWeight: FontWeight.bold,
              color: _present
                  ? Theme.of(context).colorScheme.primary
                  : Theme.of(context).colorScheme.outline,
            ),
          ),
          const SizedBox(height: 8),
          Text(
            readings,
            key: const Key('stdFacilities'),
            style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
          ),
          const SizedBox(height: 12),
          Wrap(
            spacing: 8,
            children: [
              FilledButton.tonal(
                onPressed: _println,
                child: const Text('println! to the console'),
              ),
              FilledButton.tonal(
                onPressed: _napping ? null : _nap,
                child: const Text('sleep 20ms on an actor'),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Text(
            'println! writes to the devtools console — open it to see the line.',
            style: Theme.of(context).textTheme.bodySmall,
          ),
        ],
      ),
    );
  }
}

// ================================================== watching the bridge ==

/// Rust's `log` output, as a Dart stream this app owns.
///
/// Top-level for the same reason the call trace is: the gallery disposes the
/// `State` of a card scrolled out of view, and the records worth seeing are
/// produced by the *rest* of the app — `greet`, `nth_prime`, `Ledger::add`,
/// `new_greeter` all log while doing their own work.
final _RustLog _rustLog = _RustLog();

class _RustLog {
  static const int _capacity = 12;

  /// The most recent records, oldest first.
  final ValueNotifier<List<LogLine>> lines = ValueNotifier(const []);

  /// What went wrong installing, if anything. `install_logging` refuses when
  /// something else already claimed `log`'s one process-wide logger slot.
  final ValueNotifier<String?> failure = ValueNotifier(null);

  /// Hand Rust a `StreamController` and let `log::info!` fill it.
  ///
  /// No microtask dance is needed here, unlike the call trace: a record is
  /// delivered by the stream — a port message on macOS, a microtask on web —
  /// so it can never arrive inside a widget build, however deep in Rust the
  /// `log!` was.
  void install() {
    final sink = StreamController<LogLine>();
    sink.stream.listen((record) {
      final next = [...lines.value, record];
      lines.value = List<LogLine>.unmodifiable(
          next.length > _capacity ? next.sublist(next.length - _capacity) : next);
    });
    try {
      installLogging(sink: sink);
    } catch (e) {
      failure.value = '$e';
    }
  }

  void clear() => lines.value = const [];
}

class _RustLogCard extends StatelessWidget {
  const _RustLogCard();

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: "log::info! — Rust's log crate, arriving as a Dart stream",
      subtitle: 'The bridge crate logs through the ordinary `log` facade, with '
          'no frustrate type in sight; frustrate::logging::install puts a '
          'StreamSink behind that facade, so the records land on a Dart Stream '
          'this app owns. The entries below come from other cards doing their '
          'job, and from the button. A dependency that has never heard of '
          'frustrate reaches the same stream, which is the point of routing '
          'through `log` rather than inventing a logger.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          ValueListenableBuilder<String?>(
            valueListenable: _rustLog.failure,
            builder: (context, failure, _) => failure == null
                ? const SizedBox.shrink()
                : Padding(
                    padding: const EdgeInsets.only(bottom: 8),
                    child: Text('logging not installed — $failure',
                        style: TextStyle(
                            color: Theme.of(context).colorScheme.error)),
                  ),
          ),
          ValueListenableBuilder<List<LogLine>>(
            valueListenable: _rustLog.lines,
            builder: (context, lines, _) => Text(
              lines.isEmpty
                  ? 'no records yet'
                  : lines
                      .map((r) =>
                          '${r.level.padRight(5)} ${r.target}: ${r.message}')
                      .join('\n'),
              key: const Key('rustLog'),
              style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
            ),
          ),
          const SizedBox(height: 12),
          Wrap(
            spacing: 8,
            children: [
              FilledButton.tonal(
                onPressed: () => emitDemoLog(message: 'a line logged on request'),
                child: const Text('log a line from Rust'),
              ),
              TextButton(
                  onPressed: _rustLog.clear, child: const Text('clear')),
            ],
          ),
        ],
      ),
    );
  }
}

/// The rolling record of what crossed the bridge, and the switch that puts the
/// decorator producing it in front of the bindings.
///
/// Top-level rather than owned by the card, for two reasons that are both about
/// what a trace is *for*. The gallery is a `ListView`, which disposes the
/// `State` of a card scrolled out of view — a buffer held there would be
/// emptied every time the reader scrolled up to press a button. And the claim
/// on display is that one place sees every call, so the recorder has to outlive
/// any single card and hear the ones the *other* cards make.
final _CallTrace _callTrace = _CallTrace();

class _CallTrace {
  /// The most recent calls, oldest first. What the card renders.
  final ValueNotifier<List<String>> lines = ValueNotifier(const []);

  /// Whether the decorator is currently active.
  final ValueNotifier<bool> on = ValueNotifier(false);

  static const int _capacity = 12;

  /// Built once and reused. `Frustrate.instance` answers the *active* runtime,
  /// so constructing a second tracer while one is active would wrap the tracer
  /// instead of the transport. `Frustrate.activate` refuses to stack a runtime
  /// on a runtime, so that mistake is caught rather than compounded — but
  /// keeping the one object is what makes this switch a swap rather than a
  /// growing pile.
  _Tracer? _tracer;

  /// Recorded synchronously by the decorator, published on a microtask.
  final List<String> _buffer = [];
  bool _publishScheduled = false;

  void record(String line) {
    _buffer.add(line);
    if (_buffer.length > _capacity) {
      _buffer.removeRange(0, _buffer.length - _capacity);
    }
    if (_publishScheduled) return;
    _publishScheduled = true;
    // A bridge call can happen *during a build*: `_GreetingCard` calls `greet`
    // straight out of `build`, and the std-facility card's readings are `late
    // final` fields first read there. Moving a `ValueNotifier` at that moment
    // is "markNeedsBuild called during build", so the notifier moves on a
    // microtask instead — Flutter's build/layout/paint pass is synchronous
    // within one frame callback, so this lands after it. Coalesced to one per
    // burst, because the async-fn card fires a thousand calls in a single turn
    // and a thousand rebuilds would make watching the bridge the most expensive
    // thing in the app.
    scheduleMicrotask(_publish);
  }

  void _publish() {
    _publishScheduled = false;
    lines.value = List<String>.unmodifiable(_buffer);
  }

  void setTracing(bool want) {
    if (want == on.value) return;
    if (want) {
      Frustrate.activate(_tracer ??= _Tracer(Frustrate.instance));
    } else {
      // Back to the transport the platform init installed. Both directions are
      // allowed with calls in flight and streams open, because a decorator is
      // the *same bridge* — same raws, same drop exports — so no live handle is
      // stranded by the swap. Activating something that is a *different* bridge
      // (a fake, say) is the case that demands quiescence, and that a build
      // with asserts off refuses outright.
      Frustrate.reset();
    }
    on.value = want;
  }

  void clear() {
    _buffer.clear();
    lines.value = const [];
  }
}

/// The decorator: it names what crossed, and how, and changes nothing else.
final class _Tracer extends DelegatingRuntime {
  _Tracer(super.inner);

  /// `frustrateMemberNames` is generated beside the bindings, keyed by the
  /// same fn id the hooks are handed — so a trace reads in the API's own names
  /// instead of in numbers. The lookup is nullable because ids are derived
  /// from each member's wire facts rather than counted, so an id with no entry
  /// would mean a hook saw a member this interface does not have.
  String _name(int fnId) => frustrateMemberNames[fnId] ?? 'fn#$fnId';

  @override
  BinaryReader aroundSync(int fnId, BinaryReader Function() next) {
    _callTrace.record('sync   ${_name(fnId)}');
    return next();
  }

  @override
  Future<BinaryReader> aroundAsync(
      int fnId, Future<BinaryReader> Function() next,
      {bool deferred = false,
      FrustrateCancelToken? cancel,
      ActorHost? host}) {
    // `host` is non-null exactly for a call on an actor's executor, and that is
    // the row worth seeing: a generated actor routes every method through the
    // host it was constructed with and never touches `Frustrate.instance`, so a
    // decorator that wrapped only the runtime would look complete on free
    // functions and silently miss an entire concurrency model.
    _callTrace.record('async  ${_name(fnId)}'
        '${host != null ? '  @actor' : ''}'
        '${deferred ? '  deferred' : ''}'
        '${cancel != null ? '  cancel' : ''}');
    return next();
  }
}

class _CallTraceCard extends StatelessWidget {
  const _CallTraceCard();

  @override
  Widget build(BuildContext context) {
    return _SectionCard(
      title: 'Call trace — one decorator, every bridge call',
      subtitle: 'A DelegatingRuntime (package:frustrate/intercept.dart) goes in '
          'front of the generated bindings with Frustrate.activate and comes '
          'off again with Frustrate.reset — the seam a tracing or metrics layer '
          'uses in production, and legal to swap while the app runs because a '
          'decorator is the same bridge. Turn it on, then scroll up and press '
          'anything: actor methods are marked @actor, because they route '
          'through their own host rather than the singleton.',
      child: ValueListenableBuilder<bool>(
        valueListenable: _callTrace.on,
        builder: (context, on, _) => Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                // A button rather than a Switch: every other control in this
                // gallery is one, and a Switch's on/off state is not something
                // the browser suite can read out of the accessibility tree by
                // name the way it reads a button's label.
                FilledButton.tonal(
                  onPressed: () => _callTrace.setTracing(!on),
                  child: Text(on ? 'stop tracing' : 'start tracing'),
                ),
                const SizedBox(width: 12),
                Text(on ? 'tracing on' : 'tracing off'),
                const Spacer(),
                TextButton(
                    onPressed: _callTrace.clear, child: const Text('clear')),
              ],
            ),
            const SizedBox(height: 8),
            ValueListenableBuilder<List<String>>(
              valueListenable: _callTrace.lines,
              builder: (context, lines, _) => Text(
                lines.isEmpty
                    ? (on
                        ? 'trace: waiting for a call'
                        : 'trace: off — no decorator is installed')
                    : lines.join('\n'),
                key: const Key('callTrace'),
                style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
              ),
            ),
          ],
        ),
      ),
    );
  }
}
