//! The web implementation: a real iroh peer, and a second-class one.
//!
//! `api.rs` aliases this module as `imp` when `target_family = "wasm"`, so the
//! bridge surface is identical to `node.rs`'s. That was once forced, before
//! `#[bridge(native_only)]` could omit a member from the web surface, but it
//! was never a cost here and is not one now: every member does the same job it
//! does natively, so this crate wants one surface rather than two. What the
//! signature cannot say by itself is that two of those members mean *less*
//! here; `api.rs`'s rustdoc says it, and a Dart caller reads that on both
//! platforms.
//!
//! # What is genuinely different, and why
//!
//! **Relay-only, always. There is no hole punching in a browser, ever.** A page
//! cannot open a UDP socket; there is no API and no permission that grants one.
//! So every byte this peer sends or receives crosses a relay over a WebSocket,
//! [`PeerEvent::Path`] reports `direct: false` for the life of every connection,
//! and there is no configuration that changes that. iroh knows this: under its
//! own `wasm_browser` cfg, `Endpoint::watch_addr` returns an address built from
//! the home relay alone, "as there are no APIs for directly using sockets in
//! browsers". The app is required to *show* that degradation rather than hide
//! it, so the path badge reads `relay` and means it.
//!
//! **Nothing here may block, so the constructor cannot bind.** A wasm instance
//! cannot synchronously await a JS promise — the whole transport is JS promises
//! — so `Node::open` returns a `Node` whose endpoint does not exist yet and
//! whose ticket is the empty string. This is exactly the case `api.rs` designed
//! for: the ticket's real delivery vehicle is [`PeerEvent::Listening`], and
//! [`Node::ticket`] serves whatever the last `Listening` carried. Note what this
//! means for the surface's honesty: on web `open()` can no longer report a bind
//! failure through its `Result`, because it has already returned by the time the
//! bind fails. Bind failures arrive as [`PeerEvent::Failed`] instead.
//!
//! **There is no runtime to own.** `node.rs`'s central fact — the actor owns a
//! multi-threaded `tokio::Runtime` — has no analogue. A browser wasm instance
//! has one thread and its executor is the JS event loop, so every task here is
//! `wasm_bindgen_futures::spawn_local`. That is *more* faithful to the actor
//! rule than the native version, not less: nothing can block, so nothing can
//! wedge the instance.
//!
//! The one rule from `node.rs` still holds and is now structural: **this file
//! contains no `block_on` and no `.await` outside a spawned task.** Actor
//! methods parse, poke a channel, or spawn.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use frustrate::StreamSink;
use iroh::endpoint::{presets, Connection, ReadExactError, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::{mpsc, watch};
use wasm_bindgen_futures::spawn_local;

use crate::api::{FailureKind, PeerEvent, Preset};

/// The frame codec, shared verbatim with `node.rs` and `peerbot`.
///
/// `#[path]` resolves relative to `src/`, so this is `src/proto.rs` — the same
/// file `node.rs` picks up the same way. The two implementation modules are
/// never compiled together (`lib.rs` cfgs them apart), so there is exactly one
/// `proto` in any given build.
#[path = "proto.rs"]
pub mod proto;

/// The application-layer protocol name.
///
/// **Must equal `node.rs`'s `ALPN` byte for byte**, or a browser peer and a
/// native peer fail at ALPN negotiation rather than at anything legible. It is
/// duplicated rather than shared because the two modules are mutually exclusive
/// and neither can name the other; `proto.rs` cannot hold it either, since that
/// file is deliberately free of iroh and is linked by `peerbot` too. This is the
/// same split-contract problem the ALPN has in `node.rs`, one level up: the
/// one constant whose mismatch is unrecoverable is the one that cannot live in
/// the shared file.
pub const ALPN: &[u8] = b"frustrate/tin-can/0";

/// One live peer: the connection, and the queue the writer task drains.
///
/// Identical in shape to `node.rs`'s, minus the `Send` bounds nothing on this
/// platform can satisfy or needs.
struct Peer {
    conn: Connection,
    out: mpsc::UnboundedSender<String>,
}

/// `Rc<RefCell<..>>` rather than `Arc<Mutex<..>>`: there is one thread, and
/// pretending otherwise would cost an atomic per access and buy nothing. The
/// actor's object is a plain `Box<T>` needing neither `Send` nor `Sync`,
/// which is what makes this legal.
type Peers = Rc<RefCell<HashMap<EndpointId, Peer>>>;

pub struct Node {
    /// Published once the bind completes. Every operation that needs an
    /// endpoint waits on this rather than failing early, so a `connect()` that
    /// arrives during the bind is queued by construction instead of racing it.
    endpoint: watch::Receiver<Option<Endpoint>>,
    /// The last ticket we published, which is what [`Node::ticket`] answers
    /// with. Empty until the first [`PeerEvent::Listening`].
    ticket: Rc<RefCell<String>>,
    nickname: String,
    peers: Peers,
    events: StreamSink<PeerEvent>,
    /// Whether an address lookup service is configured. See [`resolvable`].
    discovery: bool,
}

impl Node {
    /// Start binding an endpoint, and return immediately.
    ///
    /// A custom `relay` matters more here than anywhere else: a browser peer
    /// has no UDP socket and therefore no direct path, ever, so **every byte it
    /// sends crosses this relay**. Pointing a browser at a relay is not a
    /// preference about routing, it is a choice of who carries the whole
    /// conversation.
    ///
    /// This constructor used to be incapable of failing — the bind is
    /// asynchronous, so `Result` was kept only because the bridged signature is
    /// shared. The relay parameter gives it one honest use back:
    /// parsing a URL is synchronous and local, so a bad relay URL is rejected
    /// here, on web exactly as on native, rather than becoming a silent
    /// never-online.
    pub fn open(
        nickname: String,
        preset: Preset,
        relay: Option<String>,
        events: StreamSink<PeerEvent>,
    ) -> Result<Self, String> {
        let relay = match relay.as_deref() {
            Some(text) => Some(parse_relay_url(text)?),
            None => None,
        };
        // A browser cannot send UDP, so every byte a web peer moves goes through
        // a relay. `Minimal` with no relay is therefore not a slow
        // configuration, it is a non-functioning one — an endpoint that binds,
        // reports `Listening`, and can never reach or be reached by anyone.
        // Native has no equivalent failure: there, `Minimal` with no relay is
        // the *good* hermetic case, because two peers on one host or one LAN
        // connect directly. Rejecting it here, synchronously, is the difference
        // between a build error and a demo that looks fine and does nothing.
        if matches!(preset, Preset::Minimal) && relay.is_none() {
            return Err("a browser peer has no direct transport, so Minimal \
                        without a relay can never connect. Pass a relay url, \
                        or build with PRESET=n0."
                .to_string());
        }
        let (tx, endpoint) = watch::channel(None);
        let ticket = Rc::new(RefCell::new(String::new()));
        let peers: Peers = Rc::new(RefCell::new(HashMap::new()));

        spawn_local(bind(
            tx,
            ticket.clone(),
            peers.clone(),
            events.clone(),
            nickname.clone(),
            preset,
            relay,
        ));

        Ok(Node {
            endpoint,
            ticket,
            nickname,
            peers,
            events,
            discovery: matches!(preset, Preset::N0),
        })
    }

    /// The last ticket [`PeerEvent::Listening`] carried, or `""` before the
    /// bind completes.
    ///
    /// The empty string is not an error case to be tidied away — it is the
    /// honest answer to "what is my ticket" asked before there is one, and it
    /// is why `api.rs` emits the ticket as an event as well as exposing it as a
    /// getter. A UI that renders this without waiting for `Listening` shows an
    /// empty box, which is the correct depiction of the state it is in.
    pub fn ticket(&self) -> &str {
        // SAFETY-ish: the returned reference must not outlive the borrow, and
        // it cannot — `api.rs` calls `.to_string()` on it immediately. Handing
        // out a `Ref` would change the shared inherent signature.
        //
        // This is the one place the web transport is uncomfortable in the
        // native signature: `-> &str` presumes a stored `String` that is never
        // replaced, which is exactly what a late-arriving ticket has to do.
        unsafe { &*self.ticket.as_ptr() }
    }

    /// Dial the peer named by `ticket`. Returns immediately.
    ///
    /// Parsing is local and instant, so a bad ticket is reported synchronously
    /// (as an event, the signature having nowhere else to put it). The dial
    /// waits for the endpoint if the bind is still in flight.
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
        spawn_local(dial(
            self.endpoint.clone(),
            addr,
            self.peers.clone(),
            self.events.clone(),
            self.nickname.clone(),
        ));
    }

    /// Queue a message for a connected peer. Byte-for-byte `node.rs`'s body.
    pub fn send(&self, peer: &str, body: String) -> Result<(), String> {
        let id = parse_peer(peer)?;
        let peers = self.peers.borrow();
        let entry = peers.get(&id).ok_or("not connected to that peer")?;
        entry
            .out
            .send(body)
            .map_err(|_| "that peer's connection is closing".to_string())
    }

    /// Close one connection. Idempotent, and silent for an unknown peer.
    ///
    /// As in `node.rs`, this deliberately does not emit [`PeerEvent::Left`] —
    /// the reader task sees the close and emits exactly one, so that event has
    /// a single producer whoever initiated the disconnect.
    pub fn disconnect(&self, peer: &str) {
        let Ok(id) = parse_peer(peer) else { return };
        let gone = self.peers.borrow_mut().remove(&id);
        if let Some(peer) = gone {
            peer.conn.close(0u32.into(), b"disconnected");
        }
    }
}

impl Drop for Node {
    /// Best-effort, and that is a real difference from native.
    ///
    /// `node.rs`'s `Drop` blocks on a bounded `endpoint.close()` so that
    /// `dispose()` means "the sockets are shut". Here nothing may block, so the
    /// close is spawned and `dispose()` returns before it completes. If the page
    /// is navigating away the task never runs at all and peers learn we are gone
    /// by timing out — which they must handle regardless, and which is the same
    /// outcome native's 2-second timeout produces against a wedged relay.
    fn drop(&mut self) {
        let Some(endpoint) = self.endpoint.borrow().clone() else {
            return;
        };
        spawn_local(async move {
            endpoint.close().await;
        });
    }
}

/// Bind the endpoint, publish it, and start the two long-lived loops.
///
/// This is `node.rs`'s `start()` with the blocking turned inside out: there the
/// bind is `block_on` inside the constructor, here it is the head of a spawned
/// task and everything downstream of it waits on `tx`.
async fn bind(
    tx: watch::Sender<Option<Endpoint>>,
    ticket: Rc<RefCell<String>>,
    peers: Peers,
    events: StreamSink<PeerEvent>,
    nickname: String,
    preset: Preset,
    relay: Option<RelayUrl>,
) {
    // `presets::N0` is browser-safe: its own docs note that the DNS address
    // lookup is added "outside browsers", and pkarr publish/resolve are plain
    // HTTPS, which a page can do. Relays are reached over `wss://`.
    //
    // The preset/relay matrix is `node.rs`'s `builder_for` and the reasoning is
    // recorded there. Two browser-only caveats on top of it: a page served over
    // HTTPS may only open `wss://`, so an `http://` relay that works natively is
    // blocked by mixed-content rules unless the page itself is on
    // `http://localhost`; and the relay's URL must satisfy the page's
    // `connect-src`, which is why the browser test proxies its relay through
    // its own origin rather than widening the CSP.
    let mut builder = match preset {
        Preset::Minimal => Endpoint::builder(presets::Minimal),
        Preset::N0 => Endpoint::builder(presets::N0),
    };
    if let Some(url) = relay {
        builder = builder.relay_mode(RelayMode::custom([url]));
    }
    let endpoint = match builder.alpns(vec![ALPN.to_vec()]).bind().await {
        Ok(endpoint) => endpoint,
        Err(e) => {
            // The failure `open()` would have returned natively. It has to be
            // an event here: `open()` returned successfully some time ago.
            events.add(PeerEvent::Failed {
                peer: None,
                kind: FailureKind::Internal,
                detail: format!("could not bind an endpoint: {e}"),
            });
            return;
        }
    };

    publish_ticket(&endpoint, &ticket, &events);
    let _ = tx.send(Some(endpoint.clone()));

    spawn_local(accept_loop(
        endpoint.clone(),
        peers,
        events.clone(),
        nickname,
    ));
    spawn_local(online_watch(endpoint, ticket, events));
}

/// Render the endpoint's current address as a ticket, store it, and announce it.
///
/// Called twice: once at bind, when a browser endpoint's address is bare
/// identity (there are no direct addresses and no home relay yet), and once
/// when the relay handshake completes and the address finally carries something
/// another peer can dial. The second one is the ticket that actually works.
fn publish_ticket(endpoint: &Endpoint, ticket: &Rc<RefCell<String>>, events: &StreamSink<PeerEvent>) {
    let text = EndpointTicket::new(endpoint.addr()).to_string();
    *ticket.borrow_mut() = text.clone();
    events.add(PeerEvent::Listening { ticket: text });
}

/// Wait for relay reachability, once, then re-announce the ticket.
///
/// This matters far more on web than natively. A native endpoint's bind-time
/// ticket already carries direct addresses, so it is dialable immediately; a
/// browser endpoint has none, so until the home relay is known its ticket names
/// an identity and no way to reach it. `Online` is the moment the ticket becomes
/// real, and the second `Listening` is how the app learns.
async fn online_watch(endpoint: Endpoint, ticket: Rc<RefCell<String>>, events: StreamSink<PeerEvent>) {
    endpoint.online().await;
    if !events.add(PeerEvent::Online) {
        return;
    }
    publish_ticket(&endpoint, &ticket, &events);
}

/// Block until the endpoint exists, then hand it over.
///
/// Returns `None` only if the `Node` was dropped before the bind finished, in
/// which case the caller has nothing to report to and nobody to report it to.
async fn endpoint_ready(mut rx: watch::Receiver<Option<Endpoint>>) -> Option<Endpoint> {
    loop {
        if let Some(endpoint) = rx.borrow_and_update().clone() {
            return Some(endpoint);
        }
        rx.changed().await.ok()?;
    }
}

/// Take every inbound connection.
async fn accept_loop(endpoint: Endpoint, peers: Peers, events: StreamSink<PeerEvent>, me: String) {
    while let Some(incoming) = endpoint.accept().await {
        let (peers, events, me) = (peers.clone(), events.clone(), me.clone());
        spawn_local(async move {
            match incoming.await {
                Ok(conn) => adopt(conn, peers, events, me, false).await,
                Err(e) => {
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

/// Dial one peer and hand the result to `adopt`.
async fn dial(
    rx: watch::Receiver<Option<Endpoint>>,
    addr: EndpointAddr,
    peers: Peers,
    events: StreamSink<PeerEvent>,
    me: String,
) {
    let Some(endpoint) = endpoint_ready(rx).await else {
        return;
    };
    let peer = addr.id.to_string();
    match endpoint.connect(addr, ALPN).await {
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
/// each way, then a reader and a writer.
///
/// `node.rs` also spawns a `path_loop` here. This does not, and the omission is
/// the honest one: in a browser the answer is a constant. Every path is a relay
/// path, forever, so the badge is emitted once with `direct: false` rather than
/// watched for a transition that cannot occur.
async fn adopt(conn: Connection, peers: Peers, events: StreamSink<PeerEvent>, me: String, initiator: bool) {
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
    peers.borrow_mut().insert(
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

    // Not a watcher: a browser has no direct path to transition to.
    if !events.add(PeerEvent::Path {
        peer: peer.clone(),
        direct: false,
    }) {
        return;
    }

    spawn_local(write_loop(send, queue, events.clone(), peer.clone()));
    read_loop(conn, recv, id, peers, events, peer).await;
}

/// Report a failure attributable to one peer.
fn fail(events: &StreamSink<PeerEvent>, peer: &str, detail: String) {
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
    recv.read_exact(&mut body).await.map_err(|e| e.to_string())?;
    proto::decode_body(len, &body)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Drain the queue onto the wire.
async fn write_loop(
    mut send: SendStream,
    mut queue: mpsc::UnboundedReceiver<String>,
    events: StreamSink<PeerEvent>,
    peer: String,
) {
    while let Some(body) = queue.recv().await {
        let frame = match proto::encode(&body) {
            Ok(frame) => frame,
            Err(e) => {
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
    events: StreamSink<PeerEvent>,
    peer: String,
) {
    let why = loop {
        match read_frame(&mut recv).await {
            Ok(Some(body)) => {
                let event = PeerEvent::Message {
                    peer: peer.clone(),
                    body,
                    // Not SystemTime::now(): see `now()` below.
                    at: now(),
                };
                if !events.add(event) {
                    break None;
                }
            }
            Ok(None) => break Some("the peer closed the connection".to_string()),
            Err(e) => break Some(e),
        }
    };

    peers.borrow_mut().remove(&id);
    conn.close(0u32.into(), b"bye");
    if let Some(why) = why {
        events.add(PeerEvent::Left { peer, why });
    }
}

/// Accept a ticket, or a bare endpoint id. `node.rs`'s body, verbatim.
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
/// `node.rs`'s body, verbatim — see there for why it fires on the address
/// rather than on which parse branch produced it.
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

/// Parse the `peer` string the UI got out of an event back into an identity.
fn parse_peer(peer: &str) -> Result<EndpointId, String> {
    EndpointId::from_str(peer.trim()).map_err(|e| format!("not an endpoint id: {e}"))
}

/// Parse a relay URL, and say what is wrong with one that is not.
/// `node.rs`'s body, verbatim, and duplicated for the same reason everything
/// else in this file is: the two modules are mutually exclusive and neither
/// can name the other. Kept honest by the wasm build compiling this file.
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

/// The wall clock, from JS.
///
/// **`SystemTime::now()` cannot be called here.** On
/// `wasm32-unknown-unknown` std has no clock, and its `SystemTime::now()` is
/// the `unsupported` stub, which panics — under `panic = "abort"` that is a
/// wasm trap, surfacing in the browser as a bare `Uncaught (in promise)
/// unreachable` with no file, no line and no mention of time.
///
/// This cost a real debugging session, and the shape of it is the finding:
/// `api.rs` declares `PeerEvent::Message { at: SystemTime }` because
/// `SystemTime` is a type frustrate bridges (it becomes a Dart `DateTime`).
/// Nothing anywhere says that a bridged type frustrate supports may be
/// unconstructible on a platform frustrate supports. The native code that
/// fills it in is the only code that could possibly be written, and it aborts:
/// on wasm32 `SystemTime::now()` is std's `unsupported` stub, which panics, and
/// under `panic = "abort"` that is a trap the browser reports as, in its
/// entirety, `unreachable`.
///
/// `Date.now()` is milliseconds since the Unix epoch as an `f64`, which is
/// exactly `UNIX_EPOCH + Duration`. Sub-millisecond precision does not exist
/// here and does not matter: this timestamp is stamped on arrival, for display.
fn now() -> SystemTime {
    let millis = js_sys::Date::now();
    if millis <= 0.0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::from_millis(millis as u64)
}
