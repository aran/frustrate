//! The whole bridge surface. This is the only file this crate has frustrate
//! parse (`//bridge:codegen` lists exactly `src/api.rs` in `srcs`), and it is
//! deliberately nothing but signatures: every body is a one-line delegation to
//! `imp`, which is `node.rs` on native and `stub.rs` on wasm.
//!
//! The split began as a workaround, when frustrate had no way to say "this
//! member cannot exist on web" — so the member existed everywhere and the
//! *body* was what differed. `#[bridge(native_only)]` says it directly now,
//! but this crate keeps the split deliberately: `stub.rs` is not a stub any
//! more, it is a real wasm iroh peer, so these members exist on both platforms
//! over two genuinely different transports. That is a member meaning *less* on
//! web, not one that cannot exist there.
//! The Bazel `select()` in `BUILD.bazel` still drops the native-only iroh
//! dependency on the same condition.

use frustrate::{bridge, StreamSink};
use std::time::SystemTime;

#[cfg(not(target_family = "wasm"))]
use crate::node as imp;
#[cfg(target_family = "wasm")]
use crate::stub as imp;

/// Everything the network tells the UI.
///
/// A data enum, so Dart pattern-matches a sealed hierarchy. This is where
/// *typed* errors live: a `Result`'s `E` collapses to a string over the bridge,
/// but a variant keeps its shape, so [`PeerEvent::Failed`] carries a
/// [`FailureKind`] the UI can branch on rather than a message it must parse.
///
/// There is no `Offline` variant. iroh exposes no verified API that says "the
/// relay went away", and a variant no code can produce is a lie in the type.
#[bridge(data)]
pub enum PeerEvent {
    /// The endpoint is bound and the ticket is valid. First event, always.
    ///
    /// Carries the ticket even though [`Node::ticket`] returns the same string,
    /// because a future web transport may not be able to compute one
    /// synchronously — an event can arrive late, a getter cannot.
    Listening { ticket: String },
    /// The endpoint has a working path to the relay network, so a peer that has
    /// only the ticket can now reach us.
    Online,
    /// A dial started. Outcome arrives as `Connected` or `Failed`.
    Dialing { peer: String },
    /// A peer is connected and has told us what to call it.
    Connected { peer: String, nickname: String },
    /// How traffic to a connected peer is flowing: `direct` means hole-punched,
    /// otherwise it is relayed. Can flip either way during a connection.
    Path { peer: String, direct: bool },
    /// An application message arrived. `at` is the *receiver's* clock: peers do
    /// not share one, and a sender-stamped time would be unverifiable.
    Message {
        peer: String,
        body: String,
        at: SystemTime,
    },
    /// A connection ended, gracefully or not. `why` is for humans.
    Left { peer: String, why: String },
    /// Something failed. `peer` is `None` when the failure is not attributable
    /// to one peer (a bad ticket has no peer yet; an accept-loop error has no
    /// peer at all).
    Failed {
        peer: Option<String>,
        kind: FailureKind,
        detail: String,
    },
}

/// Which third-party services this endpoint is allowed to use.
///
/// The vocabulary is peerbot's `--preset minimal|n0`, deliberately: the two
/// halves of a two-peer run are now configured with the same two words, and
/// "fully self-hosted" is `Minimal` plus a relay on both sides rather than a
/// different sentence for each.
///
/// This is orthogonal to the relay URL. The preset decides *whether address
/// lookup exists*; the relay argument decides *who carries the bytes*. Before
/// this enum there was only one knob, welded to `N0`, which put "no third
/// party at all" out of a test's reach.
#[derive(Clone, Copy)]
#[bridge(data)]
pub enum Preset {
    /// A crypto provider and nothing else: no relays, no DNS, no pkarr, and so
    /// no packet to anyone but the peer you name.
    ///
    /// The default, and the only configuration that is honestly offline. Two
    /// peers pair by a full ticket, which carries addresses, so on one host or
    /// one LAN they connect directly with no third party involved at all. In a
    /// browser it needs a relay to be useful, because a browser cannot send UDP.
    Minimal,
    /// n0's public infrastructure: their relays, their pkarr publish/resolve,
    /// and their DNS address lookup.
    ///
    /// Zero configuration and it works across networks, which is what makes it
    /// the right thing for a human trying the demo in a browser. It is also a
    /// dependency on someone else's servers, so nothing automated uses it.
    N0,
}

/// The branchable half of a failure. `detail` on [`PeerEvent::Failed`] carries
/// the unbranchable half.
#[bridge(data)]
pub enum FailureKind {
    /// The ticket did not parse. Local, immediate, and the user's to fix.
    BadTicket,
    /// The ticket parsed but the peer never answered.
    Unreachable,
    /// A connection existed and the wire broke.
    Transport,
    /// A bug on this side.
    Internal,
}

/// One peer-to-peer endpoint: an identity, a bound socket, and every
/// connection made from it.
///
/// An actor, because it owns a tokio runtime and a set of live connections that
/// must be created, used and dropped on one thread — and because `dispose()` is
/// the only thing that closes the sockets promptly. Dart's `Node` is
/// `ActorHandle`; forgetting to dispose it leaks the runtime, its threads and
/// the ports.
///
/// **On web, `dispose()` does not mean the sockets are shut.** Natively `Drop`
/// blocks on a bounded `endpoint.close()`; a wasm instance cannot block, so the
/// close is spawned and `dispose()` returns before it completes — and if the
/// page is navigating away it never runs at all. Peers learn we are gone by
/// timing out, which they must handle regardless.
#[bridge(actor)]
pub struct Node {
    inner: imp::Node,
}

/// Every method claims `no_block`, and the claim is the lesson this demo
/// exists to teach: on threaded web the main thread may not wait, and the way
/// an app obeys that is to put the work behind an actor. `Node` binds sockets,
/// dials peers and holds a `Mutex` over live connections in its native body —
/// all of it on the actor's own executor, a dedicated thread natively and a
/// dedicated Worker on web. Main-thread Dart only serializes the request and
/// posts it, so it cannot be stalled by any of that.
///
/// The lesson is sharper than "put the slow work behind an actor", because
/// this graph leaves no main-thread surface at all: any *fallible* iroh call
/// builds an `n0_error`, whose metadata asks a `OnceLock` whether backtraces
/// are enabled, so even [`Node::connect`]'s local ticket parse — instant, no
/// network — reaches `memory.atomic.wait32` on a threaded web build. It is
/// legal here only because it runs on the actor. //bridge/check_fixture is the
/// measurement.
///
/// So the claim is settled by placement, and `frustrate_block_check` proves it
/// without building anything — which is the only way it could be proved here.
/// This graph pulls wasm-bindgen structurally, and a check artifact of it would
/// carry 2393 exports the check `cfg` cannot suppress. See //bridge:block_check.
#[bridge(no_block)]
impl Node {
    /// Bind an endpoint and start listening.
    ///
    /// The sink is a *constructor* parameter, not the return of a separate
    /// `events()` call, so that no event can be produced before Dart is
    /// listening — there is no window in which `Listening` or an early
    /// `Connected` can be dropped on the floor.
    ///
    /// Blocking here only blocks this actor's own executor, and binding is
    /// sockets-only. Relay reachability is not waited on: it arrives later as
    /// [`PeerEvent::Online`].
    ///
    /// `preset` and `relay` are independent, and between them say exactly what
    /// this endpoint may contact:
    ///
    /// | | `relay = None` | `relay = Some(url)` |
    /// |---|---|---|
    /// | [`Preset::Minimal`] | nothing at all | only that relay |
    /// | [`Preset::N0`] | n0's relays + pkarr + DNS | that relay, **plus** n0's pkarr + DNS |
    ///
    /// The bottom-right cell is the one that surprises: `relay_mode` replaces
    /// the relay transport and nothing else, so a custom relay under `N0` puts
    /// n0's relays out of the *data path* without making the endpoint
    /// air-gapped. `Minimal` is what "touches nothing of n0's" means.
    ///
    /// A URL that is not a relay URL is rejected synchronously, so `Node.open`
    /// throws rather than returning a node that can never come online.
    ///
    /// Under `Minimal` a bare `EndpointId` ticket is also rejected, because
    /// resolving one needs the pkarr that `Minimal` does not have. The
    /// alternative is a dial that hangs until it times out, which is the same
    /// failure with the diagnosis removed.
    ///
    /// **On web this `Result` cannot report a bind failure.** A wasm instance
    /// cannot synchronously await the bind, so the constructor returns before
    /// it resolves and can only ever be `Ok`; a bind failure arrives later as
    /// [`PeerEvent::Failed`]. The Dart signature is identical on both
    /// platforms, so `try { await Node.open(…) } catch (e) { … }` compiles,
    /// looks right, and never fires on web — handle the event, not the throw.
    /// A bad relay URL, rejected synchronously above, does still throw.
    pub fn open(
        nickname: String,
        preset: Preset,
        relay: Option<String>,
        events: StreamSink<PeerEvent>,
    ) -> Result<Self, String> {
        imp::Node::open(nickname, preset, relay, events).map(|inner| Node { inner })
    }

    /// The ticket a peer needs to reach this node. Cheap: the constructor
    /// already computed it, and already delivered it as
    /// [`PeerEvent::Listening`].
    pub fn ticket(&self) -> String {
        self.inner.ticket().to_string()
    }

    /// Dial the peer named by `ticket`. Fire-and-forget: the outcome arrives as
    /// [`PeerEvent::Dialing`] then [`PeerEvent::Connected`] or
    /// [`PeerEvent::Failed`], because a dial can take as long as the network
    /// takes and an actor method holds the executor while it runs.
    pub fn connect(&mut self, ticket: String) {
        self.inner.connect(ticket)
    }

    /// Queue a message to a connected peer.
    ///
    /// `Err` means one thing only — "no such peer" — which is a synchronous,
    /// local fact. A *write* failure is asynchronous and arrives as
    /// [`PeerEvent::Left`], so a successful return means queued, not delivered.
    pub fn send(&mut self, peer: String, body: String) -> Result<(), String> {
        self.inner.send(&peer, body)
    }

    /// Close one connection. Idempotent and silent for an unknown peer: the UI
    /// asking twice is not an error. Confirmation arrives as
    /// [`PeerEvent::Left`].
    pub fn disconnect(&mut self, peer: String) {
        self.inner.disconnect(&peer)
    }
}

/// Who a ticket names, without dialing it.
///
/// Exists so the UI can reject a mistyped ticket on the click that submits it,
/// rather than as an `Unreachable` event minutes later.
///
/// A free function rather than a method of [`Node`], and it does not need a
/// node at all — parsing a ticket is a pure function of the string. It carries
/// `no_block` on the same terms every `Node` method does, by placement: a
/// `#[bridge]` member's body is handed to the pool, so on threaded web it runs
/// on a worker, and on single-threaded web it runs on the caller in a module
/// that has no `memory.atomic.wait32` in it to execute.
///
/// That matters here because the body reaches one. Parsing is *fallible*, and
/// every fallible iroh call builds an `n0_error` whose metadata asks a
/// `OnceLock` whether backtraces are enabled — so a scan of this body would be
/// red, correctly, and the claim is true anyway. It is the plainest example in
/// this repo of why the claim is about placement rather than about what a body
/// reaches.
#[bridge(no_block)]
pub fn peer_id_for(ticket: String) -> Result<String, String> {
    imp::parse_ticket(&ticket).map(|addr| addr.id.to_string())
}

// ------------------------------------------------------------- logging --
//
// iroh is loud, and none of it was reachable from the app.
// `frustrate::logging` is the path: a `log::Log` whose sink is a Dart
// `StreamController`.
//
// What makes it work here is a wiring fact rather than anything in this file.
// `log`'s logger is a `static` per rlib; this module's crate hub resolves a
// `log` of its own, and so does frustrate's. `//bridge:Cargo.toml` names `log`
// so `@iroh_crates//:log` exists, `BUILD.bazel` depends on it, and `.bazelrc`
// points frustrate's `log` build setting at the same alias. Then iroh's records
// and this crate's logger share one slot. Get it wrong and rustc says so at the
// `install` call below, because the two `log`s are two types.

/// One `log` record, in the shape this app chose for it.
///
/// The wire form is the app's, not the runtime's: `frustrate::logging` hands
/// the mapper a `&log::Record` and forwards whatever comes back, so codegen
/// sees an ordinary bridged struct and knows nothing about logging.
///
/// `target` is the module path of the crate that raised the record, which is
/// how a reader tells this crate's diagnostics from `iroh_relay`'s.
#[bridge(data)]
pub struct LogRecord {
    pub level: String,
    pub target: String,
    pub message: String,
}

/// Send every record `log` accepts to `sink`, at `max_level` and below.
///
/// Most of what arrives is not from this crate. iroh, `netwatch`, `quinn` and
/// `rustls` all instrument with `tracing`, whose `log` feature this hub resolves
/// on for every native triple, and this app installs no `tracing` subscriber —
/// so a `tracing` event with no subscriber becomes a `log` record. That is the
/// point of the facility: a dependency that has never heard of frustrate is
/// heard anyway.
///
/// It is **native-only in practice, and not by declaration**. The member exists
/// on web and installs a working logger there; what does not cross is iroh's
/// traffic, because upstream gates `tracing`'s `log` dependency per target and
/// the wasm32 arm does not carry it. A web build hears this crate and not its
/// dependencies. Declaring `native_only` would be the worse lie.
///
/// `sync`, because installing is a registration and not work.
///
/// `debug` and `trace` are firehoses — iroh logs per packet.
#[bridge(sync)]
pub fn install_logging(max_level: String, sink: StreamSink<LogRecord>) -> Result<(), String> {
    let max_level = match max_level.to_ascii_lowercase().as_str() {
        "off" => log::LevelFilter::Off,
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        other => return Err(format!("{other:?} is not a log level")),
    };
    frustrate::logging::install(max_level, sink, |record| LogRecord {
        level: record.level().to_string(),
        target: record.target().to_string(),
        message: record.args().to_string(),
    })
    .map_err(|e| e.to_string())
}

/// Stop forwarding, and close the stream.
///
/// The Rust-side release lever; cancelling the Dart subscription reaches the
/// same end one record later. A caller that is finished with a `Node` still has
/// a logger installed — the two lifetimes are unrelated — so this is how the
/// stream ends deterministically.
#[bridge(sync)]
pub fn uninstall_logging() {
    frustrate::logging::uninstall();
}
