// Tin Can — direct peer-to-peer chat over iroh, bridged through frustrate.
//
// One file, byte-identical across macOS, iOS, Android and Linux. There is no
// conditional-export seam because there is nothing to branch on: the generated
// native and web surfaces are identical, and `initBridge` already hides the
// per-platform library name.
//
// The shape is `main()` + an app shell + a `NodeController` handed down by an
// `InheritedNotifier`. The cards live in this file rather than in `lib/cards/`
// because `flutter_application` in BUILD.bazel takes `main` and no `srcs`.
//
// Dart does no work: every byte of protocol, crypto and I/O is inside the
// actor, which is a dedicated OS thread on native. The only main-thread cost
// is decoding one small event per message and a `setState`.

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart' show Clipboard, ClipboardData;
import 'package:iroh_bridge/init.dart';
// BridgeException and the other bridge failures come through here: the
// generated binding re-exports what it throws, so catching one by name needs
// no dependency this app never declared. It used to import
// `package:frustrate/frustrate.dart` directly, which resolved only because
// Dart deps happen to be transitive through //bridge:iroh_bridge.
import 'package:iroh_bridge/iroh_rust.frustrate.dart';
import 'package:qr/qr.dart';

/// Which third-party services this build may use: `minimal` or `n0`.
///
/// **Defaults to `minimal`**, which contacts nobody: no relays, no DNS, no
/// pkarr. Two peers on one host or one LAN pair by full ticket and connect
/// directly, so the demo works out of the box with no third party involved.
///
/// `n0` opts into n0's public relays, pkarr and DNS. It is what a browser build
/// wants when no relay of its own is available — a browser cannot send UDP, so
/// `minimal` there needs [kRelayUrl] set — and it is how you pair across the
/// internet with zero configuration.
///
/// Same vocabulary as `peerbot --preset minimal|n0`, so a two-peer run is
/// described by one pair of words rather than two vocabularies.
const String kPreset = String.fromEnvironment('PRESET', defaultValue: 'minimal');

/// [kPreset] as the bridge enum, or `null` if the define is not one of the two.
///
/// `null` rather than a silent fallback: a typo'd `PRESET` that quietly bound
/// n0's relays would be exactly the "reaches a third party without saying so"
/// failure this app exists to make visible. [_TinCanHomeState._open] turns it
/// into a visible refusal.
Preset? get presetArgument => switch (kPreset.trim().toLowerCase()) {
      'minimal' => Preset.minimal,
      'n0' => Preset.n0,
      _ => null,
    };

/// The relay this build uses, or empty for none.
///
/// Passed straight to `Node.open` as its `relay` argument (empty becomes
/// `null`). A URL that is not a relay URL is rejected by the constructor, so a
/// typo here shows up as a visible open failure rather than as an endpoint that
/// never comes online.
///
/// Orthogonal to [kPreset]: this says who carries the bytes, the preset says
/// whether an address lookup service exists at all. Under `n0` a relay here
/// replaces n0's relays and *keeps* their pkarr and DNS — which is a real
/// configuration, but it is not "no third party".
const String kRelayUrl = String.fromEnvironment('RELAY_URL');

/// `kRelayUrl` in the shape the bridge wants: `null`, not `''`.
String? get relayArgument => kRelayUrl.trim().isEmpty ? null : kRelayUrl.trim();

/// A relay URL shortened to fit on a chip, without ever throwing. A URL bad
/// enough that `Uri.parse` rejects it is exactly a URL `Node.open` will also
/// reject, and the app has to keep rendering long enough to say so.
String _relayLabel(String url) {
  try {
    final u = Uri.parse(url);
    if (u.host.isEmpty) return url;
    return u.hasPort ? '${u.host}:${u.port}' : u.host;
  } on FormatException {
    return url;
  }
}

/// What the transport chip says: one cell of `Node.open`'s preset/relay matrix.
///
/// Both relay cases start with `relay `, which is what `//tools:two_peers`
/// asserts on.
String get _transportLabel => switch ((presetArgument, relayArgument)) {
      (Preset.n0, null) => 'n0 relays',
      (Preset.n0, final url?) => 'relay ${_relayLabel(url)} + n0 lookup',
      (Preset.minimal, null) => 'no relay — direct only',
      (Preset.minimal, final url?) => 'relay ${_relayLabel(url)}',
      (null, _) => 'PRESET=$kPreset is not valid',
    };

/// The long form, which exists to keep one distinction from being blurred: a
/// custom relay under `n0` is *not* self-hosting, because pkarr and DNS stay
/// n0's. The chip alone cannot carry that, so the tooltip says it.
String get _transportTooltip => switch ((presetArgument, relayArgument)) {
      (Preset.n0, null) =>
        'n0\'s public relays, pkarr and DNS — a third party carries the bytes '
            'and resolves the addresses. The zero-configuration default. Build '
            'with --dart-define=PRESET=minimal to contact nobody.',
      (Preset.n0, _) =>
        'Bytes go through $kRelayUrl instead of n0\'s relays, but address '
            'lookup is still n0\'s pkarr over HTTPS. This is "n0\'s relays are '
            'out of the data path", not "no n0 service is involved" — for that, '
            'add --dart-define=PRESET=minimal.',
      (Preset.minimal, null) =>
        'No relay, no DNS, no pkarr: nothing outside this machine is contacted. '
            'Peers pair by full ticket and connect directly, which works on one '
            'host or one LAN. Across the internet you need a relay, or PRESET=n0.',
      (Preset.minimal, _) =>
        'Fully self-hosted: $kRelayUrl carries the bytes and no n0 service is '
            'involved at all. Pairing is by full ticket, because there is no '
            'address lookup to resolve a bare identity with.',
      (null, _) =>
        'PRESET was set to "$kPreset", which is neither "minimal" nor "n0", so '
            'this build refuses to open rather than guess who it may contact.',
    };

/// Nickname to open with. A define so an automated run does not have to type.
const String kNickname = String.fromEnvironment('NICKNAME', defaultValue: 'me');

/// Pairing for automated runs: pre-fills the connect field. Pairing is by
/// paste otherwise — the app renders a QR code but deliberately does not scan
/// one, because a camera permission is the one thing rules_flutter cannot
/// express on either Apple platform.
const String kPeerTicket = String.fromEnvironment('PEER_TICKET');

/// How long the app waits for [PeerEventOnline] before saying, out loud, that
/// it has no relay confirmation. Not a timeout — nothing is cancelled — just
/// the point at which silence stops being reported as progress.
const Duration kRelayGrace = Duration(seconds: 10);

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initBridge();
  runApp(const TinCanApp());
}

// =========================================================== the state ==

/// How far the endpoint has got. There is deliberately no `offline`: iroh
/// exposes no verified API that says the relay went away, so the app never
/// claims to know.
enum Phase { opening, listening, online, failed }

/// One remote peer, as far as the event stream has told us.
class PeerState {
  PeerState(this.id);

  final String id;
  String nickname = '';
  bool connected = false;

  /// `null` until the first [PeerEventPath]. `true` is hole-punched, `false`
  /// is relayed — the one thing this app does that only iroh can do.
  bool? direct;

  String get label => nickname.isEmpty ? shortId : '$nickname ($shortId)';
  String get shortId => id.length <= 10 ? id : '${id.substring(0, 10)}…';
}

/// One line in a conversation. `mine` is a local echo — Rust does not send our
/// own messages back — and `error` is a failure rendered where it happened.
class ChatLine {
  ChatLine(this.text, {this.mine = false, this.error = false});

  final String text;
  final bool mine;
  final bool error;
}

/// The one owner of the [Node] handle, the [StreamController] it was opened
/// with, and the subscription to it. Actors have no singleton mechanism, so
/// this object is the singleton.
class NodeController extends ChangeNotifier {
  NodeController(this.nickname);

  String nickname;

  Node? _node;
  StreamController<PeerEvent>? _events;
  StreamSubscription<PeerEvent>? _sub;
  Timer? _relayTimer;
  bool _disposed = false;

  Phase phase = Phase.opening;
  String? ticket;
  String? openError;

  /// True once [kRelayGrace] elapsed with no `Online`. Reported, not acted on.
  bool relayUnconfirmed = false;

  /// The OS told us we were backgrounded. iOS tears UDP sockets down on
  /// suspend and no entitlement prevents it, so this is a platform limit to
  /// display, not a defect to fix.
  bool suspended = false;

  /// True while an open or reopen is in flight; the UI disables the controls
  /// that would race it.
  bool busy = false;

  final Map<String, PeerState> peers = <String, PeerState>{};
  final Map<String, List<ChatLine>> _lines = <String, List<ChatLine>>{};
  final List<String> log = <String>[];
  String? failure;
  String? selected;

  List<PeerState> get peerList => peers.values.toList(growable: false);

  List<ChatLine> linesFor(String? peer) =>
      peer == null ? const <ChatLine>[] : (_lines[peer] ?? const <ChatLine>[]);

  /// A one-line status a human (or a driver) can read without interpreting.
  String get status {
    if (phase == Phase.failed) return 'failed to open — $openError';
    if (suspended) {
      return 'suspended — the OS may have torn our sockets down; '
          'messages resume when the app does';
    }
    if (busy) return 'reopening…';
    switch (phase) {
      case Phase.opening:
        return relayUnconfirmed
            ? 'opening… — no answer from the endpoint after '
                '${kRelayGrace.inSeconds} s'
            : 'opening…';
      case Phase.listening:
        return relayUnconfirmed
            ? 'listening, but no relay confirmation after '
                '${kRelayGrace.inSeconds} s — a peer holding your ticket may '
                'not be able to reach you'
            : 'listening — waiting for relay confirmation';
      case Phase.online:
        return 'online';
      case Phase.failed:
        return 'failed to open — $openError';
    }
  }

  // ------------------------------------------------------------ opening --

  Future<void> open() async {
    // Before anything else, because there is no safe way to proceed: the build
    // asked for a preset that does not exist, and guessing which one it meant
    // would be guessing who this app is allowed to talk to.
    final preset = presetArgument;
    if (preset == null) {
      phase = Phase.failed;
      openError = 'PRESET="$kPreset" is not one of: minimal, n0';
      _fail('open failed: $openError');
      _notify();
      return;
    }

    final events = StreamController<PeerEvent>();
    _events = events;
    // Listen BEFORE opening. The controller must be single-subscription and
    // must not already carry onCancel/onPause/onResume: the generated code
    // installs those itself and throws StateError if we got there first.
    _sub = events.stream.listen(_apply, onError: _onStreamError);
    _armRelayTimer();
    _note('opening endpoint as "$nickname" on $kPreset via '
        '${relayArgument ?? (preset == Preset.n0 ? "n0 relays" : "no relay")}');
    try {
      _node = await Node.open(
          nickname: nickname,
          preset: preset,
          relay: relayArgument,
          events: events);
    } catch (e) {
      // `Result<Self, String>` arrives as a thrown BridgeException; anything
      // else is a bug on this side and is shown just as loudly.
      phase = Phase.failed;
      openError = _msg(e);
      _fail('open failed: $openError');
      _relayTimer?.cancel();
    }
    _notify();
  }

  /// The nickname is a *constructor* argument, so changing it means a new
  /// endpoint: a new ticket, and every peer dropped. Saying so is the only
  /// honest way to have an editable nickname field at all.
  Future<void> reopen(String name) async {
    if (busy || _disposed) return;
    busy = true;
    _notify();
    await _teardown();
    nickname = name;
    ticket = null;
    openError = null;
    phase = Phase.opening;
    peers.clear();
    _lines.clear();
    selected = null;
    await open();
    busy = false;
    _notify();
  }

  void _armRelayTimer() {
    _relayTimer?.cancel();
    relayUnconfirmed = false;
    _relayTimer = Timer(kRelayGrace, () {
      if (_disposed || phase == Phase.online || phase == Phase.failed) return;
      relayUnconfirmed = true;
      _notify();
    });
  }

  // ------------------------------------------------------------ actions --

  Future<void> connect(String raw) async {
    final t = raw.trim();
    if (t.isEmpty) return;
    // Self-dial. Your own ticket is a legal ticket and iroh will happily
    // attempt it, so the failure that came back would be indistinguishable
    // from a real one. String equality only catches an exact paste of the
    // string we rendered — a re-encoded ticket for the same endpoint would
    // still get through — but that is the case a human actually hits.
    if (t == ticket) {
      _fail('that is your own ticket — dialing yourself would not tell you '
          'anything, so nothing was sent');
      return;
    }
    final node = _node;
    if (node == null) {
      _fail('not connected to the endpoint yet');
      return;
    }
    // Reject a mistyped ticket on the click that submits it. Without this the
    // only signal is a `Failed(BadTicket)` event, which arrives after the dial
    // has been posted and reads like a network problem.
    //
    // Asking Rust rather than parsing here is not indirection: the parse is
    // fallible, and every fallible iroh call can wait on a lock — fatal on the
    // browser main thread. `peerIdFor` is `no_block`, which means the parse
    // never runs on this thread where such a wait is possible. The id it
    // returns is not needed: `Dialing` already reports the peer.
    try {
      await peerIdFor(ticket: t);
    } catch (e) {
      _fail('that is not a ticket: ${_msg(e)}');
      return;
    }
    try {
      await node.connect(ticket: t);
    } catch (e) {
      _fail('connect rejected: ${_msg(e)}');
    }
  }

  Future<void> send(String body) async {
    if (body.isEmpty) return;
    final peer = selected;
    final node = _node;
    if (peer == null || node == null) {
      _fail('no peer selected — connect to one first');
      return;
    }
    try {
      await node.send(peer: peer, body: body);
      // Success means *queued*, not delivered: a write failure is
      // asynchronous and arrives later as PeerEventLeft.
      _lineList(peer).add(ChatLine('me: $body', mine: true));
    } catch (e) {
      _lineList(peer).add(ChatLine('!! send failed: ${_msg(e)}', error: true));
      _fail('send failed: ${_msg(e)}');
    }
    _notify();
  }

  Future<void> disconnect(String peer) async {
    final node = _node;
    if (node == null) return;
    try {
      await node.disconnect(peer: peer);
    } catch (e) {
      _fail('disconnect failed: ${_msg(e)}');
    }
  }

  void select(String peer) {
    selected = peer;
    _notify();
  }

  void setSuspended(bool value) {
    if (suspended == value) return;
    suspended = value;
    _note(value ? 'app suspended' : 'app resumed');
  }

  // ------------------------------------------------------------- events --

  /// Runs on the UI isolate, so it does nothing but assign fields. Anything
  /// expensive here would put the work back on the thread the whole design
  /// exists to keep it off.
  void _apply(PeerEvent e) {
    _record(e.toString());
    switch (e) {
      case PeerEventListening(:final ticket):
        this.ticket = ticket;
        if (phase == Phase.opening) phase = Phase.listening;
      case PeerEventOnline():
        phase = Phase.online;
        relayUnconfirmed = false;
        _relayTimer?.cancel();
      case PeerEventDialing(:final peer):
        _peer(peer);
      case PeerEventConnected(:final peer, :final nickname):
        _peer(peer)
          ..nickname = nickname
          ..connected = true;
        selected ??= peer;
        // A connection succeeding retires the last failure. Leaving it up
        // would keep asserting a problem that no longer exists — the event
        // log below it is the permanent record.
        failure = null;
      case PeerEventPath(:final peer, :final direct):
        _peer(peer).direct = direct;
      case PeerEventMessage(:final peer, :final body, :final at):
        _peer(peer);
        _lineList(peer).add(ChatLine('${_peer(peer).label} '
            '[${at.toLocal().toIso8601String().substring(11, 19)}]: $body'));
      case PeerEventLeft(:final peer, :final why):
        _peer(peer).connected = false;
        _fail('${_peer(peer).label} left: $why', notify: false);
      case PeerEventFailed(:final peer, :final kind, :final detail):
        final who = peer == null ? '' : ' (${_peer(peer).label})';
        _fail('${kind.name}$who: $detail', notify: false);
    }
    _notify();
  }

  void _onStreamError(Object error, StackTrace _) {
    _fail('event stream error: ${_msg(error)}');
  }

  PeerState _peer(String id) => peers.putIfAbsent(id, () => PeerState(id));

  List<ChatLine> _lineList(String peer) =>
      _lines.putIfAbsent(peer, () => <ChatLine>[]);

  void _record(String line) {
    log.add(line);
    if (log.length > 24) log.removeRange(0, log.length - 24);
  }

  void _note(String line) {
    _record('· $line');
    _notify();
  }

  void _fail(String line, {bool notify = true}) {
    failure = line;
    _record('!! $line');
    if (notify) _notify();
  }

  String _msg(Object e) => e is BridgeException ? e.message : '$e';

  void _notify() {
    if (_disposed) return;
    notifyListeners();
  }

  // ----------------------------------------------------------- teardown --

  /// Drops the actor and everything hanging off it. Idempotent, because both
  /// `State.dispose` and `AppLifecycleState.detached` call it and either may
  /// come first.
  Future<void> _teardown() async {
    _relayTimer?.cancel();
    final sub = _sub;
    final node = _node;
    final events = _events;
    _sub = null;
    _node = null;
    _events = null;
    // Cancelling first fires the controller's onCancel, which is what tells
    // the Rust producer to stop.
    debugPrint('tincan: teardown started');
    await sub?.cancel();
    // Mandatory. The stored sink pins the isolate alive
    // (RawReceivePort.keepIsolateAlive), so skipping this means a process
    // that never exits.
    await node?.dispose();
    await events?.close();
    // If this line is absent from the logs after a quit, the teardown was
    // abandoned mid-flight — which is the documented risk, not a surprise.
    debugPrint('tincan: teardown finished');
  }

  /// Overrides `ChangeNotifier`'s `void dispose()` with a `Future<void>`,
  /// which Dart allows (`void` is a top type) and which the framework will
  /// never await. Everything after the first `await` below is therefore
  /// best-effort: nothing guarantees the actor is gone before the widget tree
  /// is. Two things make that survivable rather than merely ignored. The
  /// teardown starts synchronously, so on a normal quit the actor's `Drop`
  /// runs while the event loop is still turning; and `super.dispose()` is
  /// called first, so a late `notifyListeners` cannot fire into a dead tree.
  /// The honest version needs an awaited `shutdown()` on a path the framework
  /// actually awaits, and `State.dispose` is not one.
  @override
  Future<void> dispose() async {
    if (_disposed) return;
    _disposed = true;
    super.dispose();
    await _teardown();
  }
}

// ============================================================ the shell ==

class TinCanApp extends StatefulWidget {
  const TinCanApp({super.key});

  @override
  State<TinCanApp> createState() => _TinCanAppState();
}

class _TinCanAppState extends State<TinCanApp> with WidgetsBindingObserver {
  late final NodeController _controller = NodeController(kNickname);

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    // Deliberately not awaited before the first frame: the shell renders
    // "opening…" immediately rather than showing a blank window while
    // sockets bind.
    unawaited(_controller.open());
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.detached) {
      unawaited(_controller.dispose());
      return;
    }
    _controller.setSuspended(state == AppLifecycleState.paused);
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    // Best-effort by construction — see NodeController.dispose.
    unawaited(_controller.dispose());
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Tin Can',
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: Colors.teal),
        useMaterial3: true,
      ),
      home: NodeScope(
        notifier: _controller,
        child: const _HomePage(),
      ),
    );
  }
}

/// Hands the controller down and rebuilds every dependent when it notifies.
class NodeScope extends InheritedNotifier<NodeController> {
  const NodeScope({super.key, required NodeController super.notifier, required super.child});

  static NodeController of(BuildContext context) {
    final scope = context.dependOnInheritedWidgetOfExactType<NodeScope>()!;
    return scope.notifier!;
  }
}

class _HomePage extends StatelessWidget {
  const _HomePage();

  @override
  Widget build(BuildContext context) {
    final c = NodeScope.of(context);
    return Scaffold(
      appBar: AppBar(
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        title: const Text('Tin Can'),
      ),
      body: ListView(
        // Keyed because the dev tool's `app.scrollIntoView` needs a
        // `scrollableKey` to reach a widget the ListView has not built yet,
        // and every card below the fold is one.
        key: const Key('page'),
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 12),
        children: [
          _StatusBanner(c),
          const _IdentityCard(),
          const _ConnectCard(),
          const _PeersCard(),
          const _MessagesCard(),
          const _ActivityCard(),
          const SizedBox(height: 24),
        ],
      ),
    );
  }
}

// ============================================================== chrome ===

class _Card extends StatelessWidget {
  const _Card({required this.title, required this.subtitle, required this.child});

  final String title;
  final String subtitle;
  final Widget child;

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

class _StatusBanner extends StatelessWidget {
  const _StatusBanner(this.c);

  final NodeController c;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final bad = c.phase == Phase.failed || c.relayUnconfirmed || c.suspended;
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
      decoration: BoxDecoration(
        color: bad ? scheme.errorContainer : scheme.secondaryContainer,
        borderRadius: BorderRadius.circular(8),
      ),
      child: Text(
        c.status,
        key: const Key('statusText'),
        style: TextStyle(
          color: bad ? scheme.onErrorContainer : scheme.onSecondaryContainer,
        ),
      ),
    );
  }
}

// ============================================================ identity ===

class _IdentityCard extends StatefulWidget {
  const _IdentityCard();

  @override
  State<_IdentityCard> createState() => _IdentityCardState();
}

class _IdentityCardState extends State<_IdentityCard> {
  final TextEditingController _nickname = TextEditingController(text: kNickname);

  @override
  void dispose() {
    _nickname.dispose();
    super.dispose();
  }

  void _rebind(NodeController c) {
    final name = _nickname.text.trim();
    if (name.isNotEmpty) unawaited(c.reopen(name));
  }

  @override
  Widget build(BuildContext context) {
    final c = NodeScope.of(context);
    final ticket = c.ticket;
    return _Card(
      title: 'You',
      subtitle: 'The nickname is a constructor argument to Node.open, so '
          'changing it rebinds the endpoint: new ticket, peers dropped.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: TextField(
                  key: const Key('nicknameField'),
                  controller: _nickname,
                  enabled: !c.busy,
                  decoration: const InputDecoration(
                    border: OutlineInputBorder(),
                    labelText: 'nickname',
                  ),
                  onSubmitted: (_) => _rebind(c),
                ),
              ),
              const SizedBox(width: 8),
              // A button as well as onSubmitted, because a driver can enter
              // text but cannot press enter — every action the app offers has
              // to be reachable by key.
              FilledButton.tonal(
                key: const Key('rebindButton'),
                onPressed: c.busy ? null : () => _rebind(c),
                child: const Text('Rebind'),
              ),
            ],
          ),
          const SizedBox(height: 12),
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Expanded(
                child: Text(
                  ticket ?? 'no ticket yet',
                  key: const Key('ticketText'),
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
                ),
              ),
              IconButton(
                key: const Key('copyTicketButton'),
                tooltip: 'copy ticket',
                icon: const Icon(Icons.copy),
                onPressed: ticket == null
                    ? null
                    : () => Clipboard.setData(ClipboardData(text: ticket)),
              ),
            ],
          ),
          const SizedBox(height: 12),
          Center(child: _TicketQr(ticket ?? '')),
          const SizedBox(height: 4),
          const Center(
            child: Text(
              'rendered, not scanned — scanning needs a camera permission',
              style: TextStyle(fontSize: 11),
            ),
          ),
        ],
      ),
    );
  }
}

/// The ticket as a QR code, painted from the pure-Dart `qr` encoder. Stateful
/// so the encode happens once per ticket and not on every rebuild — the UI
/// isolate rebuilds on every event.
class _TicketQr extends StatefulWidget {
  const _TicketQr(this.ticket);

  final String ticket;

  @override
  State<_TicketQr> createState() => _TicketQrState();
}

class _TicketQrState extends State<_TicketQr> {
  QrImage? _image;
  String? _error;

  @override
  void initState() {
    super.initState();
    _encode();
  }

  @override
  void didUpdateWidget(_TicketQr old) {
    super.didUpdateWidget(old);
    if (old.ticket != widget.ticket) _encode();
  }

  void _encode() {
    _image = null;
    _error = null;
    if (widget.ticket.isEmpty) return;
    try {
      _image = QrImage(QrCode.fromData(
        data: widget.ticket,
        errorCorrectLevel: QrErrorCorrectLevel.L,
      ));
    } on InputTooLongException catch (e) {
      _error = 'ticket too long to encode as a QR code (${e.message})';
    }
  }

  @override
  Widget build(BuildContext context) {
    const side = 180.0;
    final image = _image;
    if (image == null) {
      return SizedBox(
        width: side,
        height: side,
        child: Center(
          child: Text(
            _error ?? 'no ticket yet',
            key: const Key('ticketQrStatus'),
            textAlign: TextAlign.center,
            style: const TextStyle(fontSize: 11),
          ),
        ),
      );
    }
    return SizedBox(
      width: side,
      height: side,
      child: CustomPaint(
        key: const Key('ticketQr'),
        painter: _QrPainter(image),
      ),
    );
  }
}

class _QrPainter extends CustomPainter {
  _QrPainter(this.image);

  final QrImage image;

  @override
  void paint(Canvas canvas, Size size) {
    // A QR code needs a light background and a quiet zone whatever the app
    // theme is, or a scanner will not see it.
    const quiet = 4;
    final modules = image.moduleCount + quiet * 2;
    final scale = size.shortestSide / modules;
    canvas.drawRect(Offset.zero & size, Paint()..color = Colors.white);
    final dark = Paint()..color = Colors.black;
    for (var row = 0; row < image.moduleCount; row++) {
      for (var col = 0; col < image.moduleCount; col++) {
        if (!image.isDark(row, col)) continue;
        canvas.drawRect(
          Rect.fromLTWH((col + quiet) * scale, (row + quiet) * scale, scale, scale),
          dark,
        );
      }
    }
  }

  @override
  bool shouldRepaint(_QrPainter old) => old.image != image;
}

// ============================================================= connect ===

class _ConnectCard extends StatefulWidget {
  const _ConnectCard();

  @override
  State<_ConnectCard> createState() => _ConnectCardState();
}

class _ConnectCardState extends State<_ConnectCard> {
  final TextEditingController _ticket = TextEditingController(text: kPeerTicket);

  @override
  void dispose() {
    _ticket.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = NodeScope.of(context);
    return _Card(
      title: 'Connect',
      subtitle: 'Paste a peer\'s ticket. Dialing is fire-and-forget: the '
          'outcome arrives as an event, not as a return value.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Align(
            alignment: Alignment.centerLeft,
            child: Tooltip(
              // The chip names every third party this endpoint was *bound*
              // with, which is one cell of Node.open's preset/relay matrix. The
              // distinction it must not blur is n0-plus-a-relay — which looks
              // self-hosted and is not, because pkarr and DNS stay n0's.
              message: _transportTooltip,
              child: Chip(
                avatar: const Icon(Icons.cell_tower, size: 18),
                // The key is on the Text, not on the Chip: the dev tool's
                // getText reads a Text widget or a *direct* child of the
                // matched element, and a Chip's label is several elements
                // down. Keying the Chip makes the badge findable and its
                // text unreadable, which is the wrong half.
                label: Text(
                  _transportLabel,
                  key: const Key('transportChip'),
                ),
              ),
            ),
          ),
          const SizedBox(height: 8),
          TextField(
            key: const Key('connectField'),
            controller: _ticket,
            decoration: const InputDecoration(
              border: OutlineInputBorder(),
              labelText: 'peer ticket',
            ),
            onSubmitted: (v) => unawaited(c.connect(v)),
          ),
          const SizedBox(height: 8),
          FilledButton(
            key: const Key('connectButton'),
            onPressed: () => unawaited(c.connect(_ticket.text)),
            child: const Text('Connect'),
          ),
        ],
      ),
    );
  }
}

// =============================================================== peers ===

class _PeersCard extends StatelessWidget {
  const _PeersCard();

  @override
  Widget build(BuildContext context) {
    final c = NodeScope.of(context);
    final peers = c.peerList;
    return _Card(
      title: 'Peers',
      subtitle: 'The badge reads relay until iroh hole-punches, then direct. '
          'That flip is the whole point.',
      child: Column(
        key: const Key('peerList'),
        crossAxisAlignment: CrossAxisAlignment.start,
        children: peers.isEmpty
            ? const [Text('no peers', key: Key('peerListEmpty'))]
            : [for (final p in peers) _PeerRow(p)],
      ),
    );
  }
}

class _PeerRow extends StatelessWidget {
  const _PeerRow(this.peer);

  final PeerState peer;

  @override
  Widget build(BuildContext context) {
    final c = NodeScope.of(context);
    final selected = c.selected == peer.id;
    return ListTile(
      key: Key('peer:${peer.id}'),
      dense: true,
      selected: selected,
      leading: Icon(peer.connected ? Icons.link : Icons.link_off),
      title: Text(peer.label),
      subtitle: Text(peer.connected ? 'connected' : 'not connected'),
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          _PathBadge(peer),
          IconButton(
            key: Key('disconnect:${peer.id}'),
            tooltip: 'disconnect',
            icon: const Icon(Icons.close),
            onPressed: () => unawaited(c.disconnect(peer.id)),
          ),
        ],
      ),
      onTap: () => c.select(peer.id),
    );
  }
}

/// `relay` → `direct`, driven only by [PeerEventPath]. Before the first one
/// arrives the app does not know, and says so rather than guessing.
class _PathBadge extends StatelessWidget {
  const _PathBadge(this.peer);

  final PeerState peer;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final direct = peer.direct;
    final (text, bg, fg) = switch (direct) {
      true => ('direct', scheme.primary, scheme.onPrimary),
      false => ('relay', scheme.tertiaryContainer, scheme.onTertiaryContainer),
      null => ('path unknown', scheme.surfaceContainerHighest, scheme.onSurfaceVariant),
    };
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(color: bg, borderRadius: BorderRadius.circular(12)),
      child: Text(
        text,
        key: Key('path:${peer.id}'),
        style: TextStyle(color: fg, fontWeight: FontWeight.w600, fontSize: 12),
      ),
    );
  }
}

// ============================================================ messages ===

class _MessagesCard extends StatefulWidget {
  const _MessagesCard();

  @override
  State<_MessagesCard> createState() => _MessagesCardState();
}

class _MessagesCardState extends State<_MessagesCard> {
  final TextEditingController _body = TextEditingController();

  @override
  void dispose() {
    _body.dispose();
    super.dispose();
  }

  void _send(NodeController c) {
    final body = _body.text.trim();
    if (body.isEmpty) return;
    _body.clear();
    unawaited(c.send(body));
  }

  @override
  Widget build(BuildContext context) {
    final c = NodeScope.of(context);
    final peer = c.selected;
    final lines = c.linesFor(peer);
    return _Card(
      title: peer == null
          ? 'Messages'
          : 'Messages — ${c.peers[peer]?.label ?? peer}',
      subtitle: 'send() returning normally means queued, not delivered: a '
          'write failure arrives later as a Left event.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Container(
            width: double.infinity,
            height: 140,
            padding: const EdgeInsets.all(8),
            decoration: BoxDecoration(
              border: Border.all(color: Theme.of(context).dividerColor),
              borderRadius: BorderRadius.circular(4),
            ),
            child: SingleChildScrollView(
              reverse: true,
              child: Text(
                lines.isEmpty
                    ? (peer == null
                        ? 'no peer selected'
                        : 'no messages with this peer yet')
                    : lines.map((l) => l.text).join('\n'),
                key: const Key('messageLog'),
                style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
              ),
            ),
          ),
          const SizedBox(height: 8),
          Row(
            children: [
              Expanded(
                child: TextField(
                  key: const Key('messageField'),
                  controller: _body,
                  decoration: const InputDecoration(
                    border: OutlineInputBorder(),
                    labelText: 'message',
                  ),
                  onSubmitted: (_) => _send(c),
                ),
              ),
              const SizedBox(width: 8),
              FilledButton(
                key: const Key('sendButton'),
                onPressed: () => _send(c),
                child: const Text('Send'),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

// ============================================================ activity ===

class _ActivityCard extends StatelessWidget {
  const _ActivityCard();

  @override
  Widget build(BuildContext context) {
    final c = NodeScope.of(context);
    return _Card(
      title: 'Activity',
      subtitle: 'Every visible state change is caused by one of these, so the '
          'log is a complete explanation of the UI.',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            c.failure ?? 'no failures',
            key: const Key('failureText'),
            style: TextStyle(
              color: c.failure == null ? null : Theme.of(context).colorScheme.error,
            ),
          ),
          const SizedBox(height: 8),
          Text(
            c.log.isEmpty ? 'nothing yet' : c.log.join('\n'),
            key: const Key('eventLog'),
            style: const TextStyle(fontFamily: 'monospace', fontSize: 11),
          ),
        ],
      ),
    );
  }
}
