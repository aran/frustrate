//! The iroh implementation: one endpoint, one ALPN, one bidirectional stream
//! per peer, length-prefixed UTF-8 frames.
//!
//! # The one rule
//!
//! `api.rs` declares `Node` as `#[bridge(actor)]`, and an actor
//! processes one message at a time. **A method that waits wedges the
//! instance**, and dialing a peer takes seconds. So every method below either
//! inspects state or pokes a channel; anything that can take longer than a
//! millisecond runs as a detached tokio task and reports through the event
//! stream. That is why [`Node::connect`] returns `()` rather than a
//! connection — the outcome arrives later as [`PeerEvent::Connected`] or
//! [`PeerEvent::Failed`].
//!
//! Mechanically the rule is: **this file contains exactly two `block_on`
//! calls**, one in [`Node::open`] for the bind and one in `Drop` for the close,
//! and both are bounded. The review check, which must print exactly two lines:
//!
//! ```text
//! grep -n block_on src/node.rs | grep -v '//'
//! ```
//!
//! # The runtime
//!
//! Multi-threaded, `enable_all()`, owned outright by the actor. None of that is
//! taste:
//!
//! - **Multi-threaded**, because on a current-thread runtime iroh's background
//!   tasks (magicsock, the relay actor, the pkarr publisher) advance only while
//!   control is inside `block_on` — so relay keepalives would stall in the gaps
//!   between Dart calls, which is most of the time.
//! - **`enable_all()`**, because without the IO driver `netwatch` panics at
//!   `udp.rs:799` ("A Tokio 1.x context was found, but IO is disabled") and with
//!   no reactor at all `portmapper` panics at `lib.rs:175`. Both reproduced.
//!
//! Owning a runtime is legal here only because an actor's object is a plain
//! `Box<T>` on a dedicated OS thread: it needs neither `Send` nor `Sync`, and
//! blocking it blocks nothing but itself.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use frustrate::StreamSink;
use iroh::endpoint::{presets, Builder, Connection, PathEvent, ReadExactError, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::api::{FailureKind, PeerEvent, Preset};

/// The frame codec, shared verbatim with `peerbot` (C5).
///
/// Declared here rather than in `lib.rs` because `lib.rs` is C2's file and this
/// module is the only consumer. `#[path]` resolves relative to `src/`, so this
/// is `src/proto.rs` — the same file a second crate can pull in with its own
/// `#[path]`, which is the whole point of keeping it dependency-light.
#[path = "proto.rs"]
pub mod proto;

/// The hermetic two-endpoint test (C3's acceptance criterion 1).
///
/// It lives in `bridge/tests/` but is compiled *into* this crate rather than
/// linked against it. That used to be forced — a cdylib cannot be linked by an
/// integration test — but `frustrate_bridge_library` now declares an rlib
/// beside the cdylib. What keeps it here is this file's own
/// shape: `use super::*` plus the `#[cfg(test)]` event seam below, neither of
/// which an external test can reach. Moving it means rewriting it against the
/// public surface, or onto `frustrate::testing::stream()`, which exists now.
#[cfg(test)]
#[path = "../tests/two_endpoints.rs"]
mod two_endpoints;

/// The application-layer protocol name. Version it, because a peer speaking a
/// different framing must fail at ALPN negotiation rather than at `read_exact`.
pub const ALPN: &[u8] = b"frustrate/tin-can/0";

/// How long `Drop` will wait for a graceful close before abandoning it.
///
/// One generous relay round trip. Not decoration: `endpoint.close()` waits for
/// the close to reach the relay, so a wedged relay makes it unbounded — which
/// would hang the Dart `dispose()` awaiting it and leave the actor thread alive
/// for the life of the process. Past this, peers learn we are gone by timing
/// out, which they must handle regardless.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `Runtime::shutdown_timeout` waits for detached tasks.
///
/// Plain `Runtime::drop` blocks until every task finishes, and ours are loops.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// Where events go.
///
/// This exists because `StreamSink` cannot be built outside a running Dart
/// isolate: `frustrate::stream::sink` needs a stream id that the generated glue
/// allocated, and `post::deliver` *panics* ("frustrate_init_dl was not called")
/// for an id no isolate owns. So a bridge crate cannot test any code that emits
/// events without a seam of its own. This is that seam. `frustrate::testing`
/// ships the general form of it now.
#[derive(Clone)]
pub(crate) enum Events {
    /// The real thing: the sink Dart handed us at construction.
    Dart(StreamSink<PeerEvent>),
    /// A plain channel, for tests. `std::sync::mpsc` rather than tokio's,
    /// because the test that reads it is a *synchronous* `#[test]` — exactly
    /// like the actor thread it stands in for — and wants `recv_timeout`, so
    /// that a hang fails the test instead of hanging it.
    #[cfg(test)]
    Probe(std::sync::mpsc::Sender<PeerEvent>),
}

impl Events {
    /// Post one event. `false` means the consumer is gone and the producer
    /// should stop — the cooperative-cancel contract `StreamSink::add` defines,
    /// which every loop in this file honours.
    fn add(&self, event: PeerEvent) -> bool {
        match self {
            Events::Dart(sink) => sink.add(event),
            #[cfg(test)]
            Events::Probe(tx) => tx.send(event).is_ok(),
        }
    }
}

/// One live peer: the connection, and the queue the writer task drains.
struct Peer {
    conn: Connection,
    out: mpsc::UnboundedSender<String>,
}

/// Shared because three tasks touch it: `adopt` inserts, the reader removes,
/// and [`Node::send`] reads — the last of those on the actor thread.
type Peers = Arc<Mutex<HashMap<EndpointId, Peer>>>;

/// A `Mutex` guard that survives a panic in another task.
///
/// Poisoning would turn one panicking reader task into a permanently dead
/// `send()`, which is a worse failure than the inconsistency it guards against
/// — the map is a `HashMap` of handles, not an invariant.
fn lock(peers: &Peers) -> std::sync::MutexGuard<'_, HashMap<EndpointId, Peer>> {
    peers.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Node {
    /// `Option` only so `Drop` can take it: `Runtime::shutdown_timeout`
    /// consumes `self`.
    rt: Option<Runtime>,
    /// A handle to the same runtime, so spawning does not need `rt`.
    handle: Handle,
    endpoint: Endpoint,
    /// Computed once, at bind. It cannot be recomputed later because
    /// `ticket()` returns `&str` — see the note on [`Node::ticket`].
    ticket: String,
    nickname: String,
    peers: Peers,
    events: Events,
    /// Whether an address lookup service is configured, i.e. whether a ticket
    /// carrying only an identity can be resolved. False under
    /// [`Preset::Minimal`]; see [`Node::connect`].
    discovery: bool,
}

impl Node {
    /// Bind an endpoint and start listening.
    ///
    /// `preset` chooses which third-party services exist at all and `relay`
    /// chooses who carries the bytes; they are independent. See [`builder_for`]
    /// for the full matrix. A URL that is not a relay URL is rejected here,
    /// before the bind, so a typo is an attributable error rather than a
    /// connection that silently never happens.
    ///
    /// Blocks on the bind, which is sockets only, and only blocks this actor's
    /// own executor. Relay reachability is *not* waited on; it arrives later as
    /// [`PeerEvent::Online`].
    pub fn open(
        nickname: String,
        preset: Preset,
        relay: Option<String>,
        events: StreamSink<PeerEvent>,
    ) -> Result<Self, String> {
        Self::start(
            nickname,
            Events::Dart(events),
            builder_for(preset, relay.as_deref())?,
            has_discovery(preset),
        )
    }

    /// The shared body of every constructor. `builder` is the only thing that
    /// differs between the app ([`builder_for`]) and the hermetic test
    /// (`presets::Minimal`, bound to loopback).
    ///
    /// `discovery` travels alongside because the endpoint cannot be asked: iroh
    /// exposes no "do I have an address lookup service" predicate, so the one
    /// place that knows is the caller that chose the preset.
    fn start(
        nickname: String,
        events: Events,
        builder: Builder,
        discovery: bool,
    ) -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("iroh-demo")
            .build()
            .map_err(|e| format!("could not start the network runtime: {e}"))?;

        // block_on 1 of 2. Bounded by the bind itself, which does not wait on
        // any remote party.
        let endpoint = rt
            .block_on(builder.alpns(vec![ALPN.to_vec()]).bind())
            .map_err(|e| format!("could not bind an endpoint: {e}"))?;

        let ticket = ticket_for(&endpoint.addr());
        let peers: Peers = Arc::new(Mutex::new(HashMap::new()));
        let handle = rt.handle().clone();

        handle.spawn(accept_loop(
            endpoint.clone(),
            peers.clone(),
            events.clone(),
            nickname.clone(),
        ));
        handle.spawn(online_watch(endpoint.clone(), events.clone()));

        events.add(PeerEvent::Listening {
            ticket: ticket.clone(),
        });

        Ok(Node {
            rt: Some(rt),
            handle,
            endpoint,
            ticket,
            nickname,
            peers,
            events,
            discovery,
        })
    }

    /// The ticket as it stood at bind: our identity plus the direct addresses
    /// the sockets already had.
    ///
    /// It does **not** carry the home relay URL, because that is only known
    /// once the endpoint has reached a relay, seconds later — and this returning
    /// `&str` means the stored string can never be replaced. That is fine on
    /// `presets::N0`, where a peer holding only the identity resolves us through
    /// pkarr; the refreshed, relay-carrying ticket is emitted as a second
    /// [`PeerEvent::Listening`] once we are online, and that is the one a peer
    /// on another network needs — most of all when [`builder_for`] pointed us
    /// at a self-hosted relay that no third party can look up for us.
    pub fn ticket(&self) -> &str {
        &self.ticket
    }

    /// Dial the peer named by `ticket`. Returns immediately.
    ///
    /// Parsing is local and instant, so a bad ticket is reported synchronously
    /// (as an event, since the signature has nowhere else to put it). The dial
    /// itself is detached: it takes as long as the network takes, and this
    /// thread is the actor.
    pub fn connect(&self, ticket: String) {
        let addr = match parse_ticket(&ticket).and_then(|addr| resolvable(addr, self.discovery)) {
            Ok(addr) => addr,
            Err(detail) => {
                self.events.add(PeerEvent::Failed {
                    peer: None,
                    kind: FailureKind::BadTicket,
                    detail,
                });
                return;
            }
        };

        self.events.add(PeerEvent::Dialing {
            peer: addr.id.to_string(),
        });
        self.handle.spawn(dial(
            self.endpoint.clone(),
            addr,
            self.peers.clone(),
            self.events.clone(),
            self.nickname.clone(),
        ));
    }

    /// Queue a message for a connected peer.
    ///
    /// No `block_on`, no `await`: the send is a poke at an unbounded channel
    /// that the peer's writer task drains. `Err` is a synchronous, local fact
    /// only — the peer is unknown, or its writer has already gone. A *write*
    /// failure is asynchronous and arrives as [`PeerEvent::Failed`] or
    /// [`PeerEvent::Left`].
    pub fn send(&self, peer: &str, body: String) -> Result<(), String> {
        let id = parse_peer(peer)?;
        let peers = lock(&self.peers);
        let entry = peers.get(&id).ok_or("not connected to that peer")?;
        entry
            .out
            .send(body)
            .map_err(|_| "that peer's connection is closing".to_string())
    }

    /// Close one connection. Idempotent, and silent for an unknown peer.
    ///
    /// Deliberately does *not* emit [`PeerEvent::Left`]: the reader task will
    /// see the close and emit exactly one, so there is a single producer for
    /// that event whether the disconnect was ours or theirs.
    pub fn disconnect(&self, peer: &str) {
        let Ok(id) = parse_peer(peer) else { return };
        let gone = lock(&self.peers).remove(&id);
        if let Some(peer) = gone {
            // Sync, returns (). The reader task wakes with a
            // `LocallyClosed` and does the reporting.
            peer.conn.close(0u32.into(), b"disconnected");
        }
    }

    /// Bind on `presets::Minimal` over loopback only: no relays, no DNS, no
    /// pkarr, no packet that leaves the host. The hermetic test's constructor.
    #[cfg(test)]
    pub(crate) fn open_local(
        nickname: &str,
        events: std::sync::mpsc::Sender<PeerEvent>,
    ) -> Result<Self, String> {
        let builder = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .map_err(|e| e.to_string())?;
        Self::start(nickname.to_string(), Events::Probe(events), builder, false)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let Some(rt) = self.rt.take() else { return };
        // block_on 2 of 2, and the reason for the timeout: `close()` waits for
        // the graceful close to reach the relay, so an unbounded wait here
        // would hang Dart's `dispose()` and strand this thread forever.
        //
        // The timeout is constructed *inside* the async block on purpose.
        // `tokio::time::timeout(d, f)` builds its `Sleep` eagerly, at the call
        // site, and `Sleep::new_timeout` needs a runtime context — so the
        // obvious `rt.block_on(timeout(d, ep.close()))` panics with "there is
        // no reactor running", inside `Drop`, which Rust turns into a
        // process abort.
        let _ = rt.block_on(async {
            tokio::time::timeout(CLOSE_TIMEOUT, self.endpoint.close()).await
        });
        // Every task in this file is a loop, so a plain `Runtime::drop` — which
        // waits for all of them — would never return either.
        rt.shutdown_timeout(SHUTDOWN_TIMEOUT);
    }
}

/// The builder the app binds from: a preset, then the relay swapped out if one
/// was named.
///
/// # Two knobs, and they are genuinely independent
///
/// `relay_mode` is a setter that *replaces* the relay transport rather than
/// appending one, and `N0::apply` sets the relay last, so applying an override
/// afterwards changes exactly that and nothing else. The preset therefore
/// decides what *else* exists, and the four combinations mean four different
/// things:
///
/// | | `relay = None` | `relay = Some(url)` |
/// |---|---|---|
/// | `Minimal` | nothing at all | only that relay |
/// | `N0` | n0's relays + pkarr + DNS | that relay, plus n0's pkarr + DNS |
///
/// The bottom-right cell is the one worth stating out loud, because it reads
/// like self-hosting and is not: a custom relay under `N0` puts n0's relays out
/// of the *data path*, which is M3's claim, while the endpoint keeps publishing
/// its address record to n0's pkarr relay over HTTPS. "Touches nothing of n0's"
/// is the left column, `Minimal`.
///
/// # Why this used to be one knob
///
/// It was `N0` unconditionally, and the argument was that changing one variable
/// keeps a misbehaving self-hosted relay attributable — that if discovery
/// vanished at the same time, you would not know which broke you. That reasoning
/// was sound and the conclusion was still wrong: it optimised for diagnosing a
/// relay at the cost of making "no third party at all" unreachable from a test,
/// which is what made it wrong. Two orthogonal knobs keep the attribution *and*
/// the configuration — you can still change one at a time; you simply are no
/// longer forced to.
///
/// The cost `Minimal` really does carry is that [`parse_ticket`]'s second form
/// stops working: a bare `EndpointId` resolves only through pkarr. That is not
/// silent — see [`resolvable`].
///
/// A third shape, `presets::Empty` plus a relay, is what `relay/README.md`
/// describes and it does not work: `Empty` sets no `crypto_provider`, which is a mandatory builder
/// option, so `bind()` always fails. Its own rustdoc says so.
fn builder_for(preset: Preset, relay: Option<&str>) -> Result<Builder, String> {
    let builder = match preset {
        Preset::Minimal => Endpoint::builder(presets::Minimal),
        Preset::N0 => Endpoint::builder(presets::N0),
    };
    let Some(relay) = relay else {
        return Ok(builder);
    };
    // `RelayMode::custom` takes an `IntoIterator<Item = RelayUrl>` and builds
    // the `RelayMap` itself. It is also the way around the `RelayMap::from`
    // ambiguity `relay/README.md` warns about: nothing here has to name
    // `RelayMap` at all.
    Ok(builder.relay_mode(RelayMode::custom([parse_relay_url(relay)?])))
}

/// Parse a relay URL, and say what is wrong with one that is not.
///
/// The host check is not belt and braces. `RelayUrl`'s `FromStr` is
/// `Url::from_str`, so `localhost:3340` — the obvious thing to type — parses
/// *successfully*, as a URL whose scheme is `localhost` and whose path is
/// `3340`. It has no host, it can never connect, and without this check the
/// only symptom is an endpoint that never comes online.
fn parse_relay_url(text: &str) -> Result<RelayUrl, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the relay url is empty; leave it unset to use n0's relays".to_string());
    }
    let url: RelayUrl = text
        .parse()
        .map_err(|e| format!("\"{text}\" is not a relay url: {e}"))?;
    if url.host_str().is_none() {
        return Err(format!(
            "\"{text}\" is not a relay url: it names no host. A relay url needs a \
             scheme — try https://{text}, or http://{text} for a --dev relay"
        ));
    }
    Ok(url)
}

/// Render an address as the string a peer pastes into the other app.
fn ticket_for(addr: &EndpointAddr) -> String {
    EndpointTicket::new(addr.clone()).to_string()
}

/// Accept a ticket, or a bare endpoint id.
///
/// The bare id is not indulgence: it is what `presets::N0`'s pkarr lookup
/// resolves, it is what the research brief verified connects across networks,
/// and it is what a user pastes when they copied only half the line.
pub(crate) fn parse_ticket(text: &str) -> Result<EndpointAddr, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("no ticket was given".to_string());
    }
    if let Ok(ticket) = EndpointTicket::from_str(text) {
        return Ok(ticket.endpoint_addr().clone());
    }
    match EndpointId::from_str(text) {
        Ok(id) => Ok(EndpointAddr::new(id)),
        Err(e) => Err(format!("not a ticket and not an endpoint id: {e}")),
    }
}

/// Reject an address this endpoint has no way to turn into a route.
///
/// A bare `EndpointId` parses into an `EndpointAddr` with no addresses and no
/// relay, which is dialable *only* through an address lookup service. Under
/// [`Preset::Minimal`] there is none, and iroh's failure for that case is a dial
/// that finds nowhere to send a packet and eventually times out — the right
/// outcome with the diagnosis removed, arriving as `Unreachable` minutes later
/// instead of `BadTicket` immediately.
///
/// So the check is local, synchronous, and names both fixes. It fires on the
/// *address*, not on which parse branch produced it: a full ticket that happens
/// to carry nothing is equally undialable, and would otherwise slip through.
fn resolvable(addr: EndpointAddr, discovery: bool) -> Result<EndpointAddr, String> {
    if discovery || !addr.addrs.is_empty() {
        return Ok(addr);
    }
    Err(format!(
        "this ticket carries only an identity ({}), and this build has no \
         address lookup to resolve it with. Paste the peer's full ticket, or \
         build with PRESET=n0 to use n0's pkarr.",
        addr.id
    ))
}

/// Whether a preset configures an address lookup service.
fn has_discovery(preset: Preset) -> bool {
    match preset {
        Preset::Minimal => false,
        Preset::N0 => true,
    }
}

/// Parse the `peer` string the UI got out of an event back into an identity.
fn parse_peer(peer: &str) -> Result<EndpointId, String> {
    EndpointId::from_str(peer.trim()).map_err(|e| format!("not an endpoint id: {e}"))
}

/// Take every inbound connection. Each one is adopted in its own task, so a
/// slow handshake never stalls the next arrival.
async fn accept_loop(ep: Endpoint, peers: Peers, events: Events, me: String) {
    while let Some(incoming) = ep.accept().await {
        let (peers, events, me) = (peers.clone(), events.clone(), me.clone());
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => adopt(conn, peers, events, me, false).await,
                Err(e) => {
                    // No peer to name: the handshake failed before we learned
                    // who it was.
                    events.add(PeerEvent::Failed {
                        peer: None,
                        kind: FailureKind::Transport,
                        detail: e.to_string(),
                    });
                }
            }
        });
    }
}

/// Wait for relay reachability, once.
///
/// `Endpoint::online()` is a one-shot: it answers "are we up yet" and never
/// reports going down again. That is exactly why `PeerEvent` has no `Offline`
/// variant — nothing verified produces one. An outage shows up per-connection
/// as [`PeerEvent::Left`].
///
/// On an endpoint with no relays configured (the hermetic test) this never
/// resolves. That is correct and costs one parked task.
async fn online_watch(ep: Endpoint, events: Events) {
    ep.online().await;
    if !events.add(PeerEvent::Online) {
        return;
    }
    // Re-announce the ticket now that it can carry the home relay URL. The
    // bind-time one has direct addresses only; this is the one that works from
    // another network without pkarr, which is what M3's own relay will need.
    events.add(PeerEvent::Listening {
        ticket: ticket_for(&ep.addr()),
    });
}

/// Dial one peer and hand the result to `adopt`.
async fn dial(ep: Endpoint, addr: EndpointAddr, peers: Peers, events: Events, me: String) {
    let peer = addr.id.to_string();
    match ep.connect(addr, ALPN).await {
        Ok(conn) => adopt(conn, peers, events, me, true).await,
        Err(e) => {
            events.add(PeerEvent::Failed {
                peer: Some(peer),
                kind: FailureKind::Unreachable,
                detail: e.to_string(),
            });
        }
    }
}

/// Turn a fresh connection into a peer: one bidirectional stream, a nickname
/// each way, then a reader, a writer and a path watcher.
///
/// `initiator` decides who opens the stream. Both sides write their nickname
/// before reading the other's, which cannot deadlock: one small frame fits in
/// the initial flow-control window.
async fn adopt(conn: Connection, peers: Peers, events: Events, me: String, initiator: bool) {
    let id = conn.remote_id();
    let peer = id.to_string();

    let opened = if initiator {
        conn.open_bi().await
    } else {
        conn.accept_bi().await
    };
    let (mut send, mut recv) = match opened {
        Ok(pair) => pair,
        Err(e) => return fail(&events, &peer, e.to_string()),
    };

    let hello = match proto::encode(&me) {
        Ok(frame) => frame,
        Err(e) => return fail(&events, &peer, format!("our own nickname is unsendable: {e}")),
    };
    if let Err(e) = send.write_all(&hello).await {
        return fail(&events, &peer, e.to_string());
    }
    let nickname = match read_frame(&mut recv).await {
        Ok(Some(nickname)) => nickname,
        Ok(None) => return fail(&events, &peer, "the peer left before saying hello".to_string()),
        Err(e) => return fail(&events, &peer, e),
    };

    let (out, queue) = mpsc::unbounded_channel();
    lock(&peers).insert(
        id,
        Peer {
            conn: conn.clone(),
            out,
        },
    );

    if !events.add(PeerEvent::Connected {
        peer: peer.clone(),
        nickname,
    }) {
        return;
    }

    tokio::spawn(write_loop(send, queue, events.clone(), peer.clone()));
    tokio::spawn(path_loop(conn.clone(), events.clone(), peer.clone()));
    read_loop(conn, recv, id, peers, events, peer).await;
}

/// Report a failure attributable to one peer.
fn fail(events: &Events, peer: &str, detail: String) {
    events.add(PeerEvent::Failed {
        peer: Some(peer.to_string()),
        kind: FailureKind::Transport,
        detail,
    });
}

/// Read one frame. `Ok(None)` is a clean end of stream.
async fn read_frame(recv: &mut RecvStream) -> Result<Option<String>, String> {
    let mut header = [0u8; proto::HEADER_LEN];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    let len = proto::body_len(header).map_err(|e| e.to_string())?;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .map_err(|e| e.to_string())?;
    proto::decode_body(len, &body)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Drain the queue onto the wire. Ends when the last `Peer` holding the sender
/// is dropped, which is what makes removing a peer from the map close its
/// stream.
async fn write_loop(
    mut send: SendStream,
    mut queue: mpsc::UnboundedReceiver<String>,
    events: Events,
    peer: String,
) {
    while let Some(body) = queue.recv().await {
        let frame = match proto::encode(&body) {
            Ok(frame) => frame,
            Err(e) => {
                // One unsendable message is not a broken connection: report it
                // and keep the peer.
                if !events.add(PeerEvent::Failed {
                    peer: Some(peer.clone()),
                    kind: FailureKind::Transport,
                    detail: e.to_string(),
                }) {
                    return;
                }
                continue;
            }
        };
        if let Err(e) = send.write_all(&frame).await {
            fail(&events, &peer, e.to_string());
            return;
        }
    }
    let _ = send.finish();
}

/// Turn inbound frames into [`PeerEvent::Message`], and the end of the stream —
/// however it ends — into exactly one [`PeerEvent::Left`].
async fn read_loop(
    conn: Connection,
    mut recv: RecvStream,
    id: EndpointId,
    peers: Peers,
    events: Events,
    peer: String,
) {
    let why = loop {
        match read_frame(&mut recv).await {
            Ok(Some(body)) => {
                let event = PeerEvent::Message {
                    peer: peer.clone(),
                    body,
                    // The *receiver's* clock. Peers do not share one, and a
                    // sender-stamped time would be unverifiable.
                    at: SystemTime::now(),
                };
                if !events.add(event) {
                    // Dart cancelled. Stop quietly — there is nobody to tell.
                    break None;
                }
            }
            Ok(None) => break Some("the peer closed the connection".to_string()),
            Err(e) => break Some(e),
        }
    };

    lock(&peers).remove(&id);
    conn.close(0u32.into(), b"bye");
    if let Some(why) = why {
        events.add(PeerEvent::Left { peer, why });
    }
}

/// Watch which path carries application data, and say so.
///
/// This is the path badge: `relay` until hole punching succeeds, `direct`
/// after. It settles the design's third unverified assumption — 1.0 does have
/// an equivalent of 0.x's `conn_type_stream`, namely
/// `Connection::path_events()`.
async fn path_loop(conn: Connection, events: Events, peer: String) {
    let mut stream = conn.path_events();
    // The selection that already happened before we subscribed. Subscribing
    // first means nothing between these two lines is missed.
    let selected = conn
        .paths()
        .iter()
        .find(|path| path.is_selected())
        .map(|path| path.is_ip());
    if let Some(direct) = selected {
        if !events.add(PeerEvent::Path {
            peer: peer.clone(),
            direct,
        }) {
            return;
        }
    }
    while let Some(event) = stream.next().await {
        if let PathEvent::Selected { remote_addr, .. } = event {
            if !events.add(PeerEvent::Path {
                peer: peer.clone(),
                direct: remote_addr.is_ip(),
            }) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_round_trips_through_parse() {
        let addr = EndpointAddr::new(iroh::SecretKey::generate().public());
        let parsed = parse_ticket(&ticket_for(&addr)).expect("our own ticket parses");
        assert_eq!(parsed.id, addr.id);
    }

    #[test]
    fn a_bare_endpoint_id_is_a_ticket() {
        let id = iroh::SecretKey::generate().public();
        let parsed = parse_ticket(&format!("  {id}  ")).expect("a bare id is accepted");
        assert_eq!(parsed.id, id);
        assert!(parsed.addrs.is_empty());
    }

    #[test]
    fn nonsense_is_rejected_by_name() {
        assert!(parse_ticket("").unwrap_err().contains("no ticket"));
        assert!(parse_ticket("hello").unwrap_err().contains("not a ticket"));
    }

    #[test]
    fn a_relay_url_is_accepted_in_both_schemes() {
        for text in ["http://localhost:3340", "https://relay.example.com", "  http://127.0.0.1:3340  "] {
            assert!(parse_relay_url(text).is_ok(), "{text} should parse");
        }
        for preset in [Preset::Minimal, Preset::N0] {
            assert!(builder_for(preset, Some("http://localhost:3340")).is_ok());
            assert!(builder_for(preset, None).is_ok());
        }
    }

    /// The four cells of `builder_for`'s matrix are four different things, and
    /// the one that matters is that `Minimal` has no address lookup — which is
    /// what makes a bare-id ticket undialable rather than slow.
    #[test]
    fn only_n0_can_resolve_a_bare_id() {
        let bare = EndpointAddr::new(iroh::SecretKey::generate().public());
        assert!(bare.addrs.is_empty(), "a bare id carries no addresses");

        let err = resolvable(bare.clone(), has_discovery(Preset::Minimal)).unwrap_err();
        assert!(err.contains("only an identity"), "{err}");
        assert!(err.contains("PRESET=n0"), "{err}");

        assert!(resolvable(bare, has_discovery(Preset::N0)).is_ok());
    }

    /// The rejection keys off the address, not off which parse branch made it:
    /// a ticket that carries somewhere to send a packet is dialable under any
    /// preset.
    #[test]
    fn an_addressed_ticket_needs_no_discovery() {
        let addr = EndpointAddr::new(iroh::SecretKey::generate().public())
            .with_relay_url("http://127.0.0.1:3340".parse().expect("a relay url"));
        assert!(resolvable(addr, has_discovery(Preset::Minimal)).is_ok());
    }

    /// The whole reason `parse_relay_url` checks the host: this string parses
    /// as a URL and would otherwise reach `bind()`, where the only symptom is
    /// an endpoint that never comes online.
    #[test]
    fn a_url_with_no_host_is_rejected_before_the_bind() {
        let err = parse_relay_url("localhost:3340").unwrap_err();
        assert!(err.contains("names no host"), "{err}");
        assert!(err.contains("http://localhost:3340"), "{err}");
    }

    #[test]
    fn a_bad_relay_url_fails_the_constructor_rather_than_panicking() {
        assert!(parse_relay_url("").unwrap_err().contains("empty"));
        assert!(builder_for(Preset::N0, Some("not a url at all")).is_err());
        assert!(builder_for(Preset::N0, Some("")).is_err());
    }
}
