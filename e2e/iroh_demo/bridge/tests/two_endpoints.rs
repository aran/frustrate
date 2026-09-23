//! The hermetic two-endpoint test: two `Node`s in one process exchange a
//! message with no external network at all.
//!
//! "No external network" is literal. Both endpoints bind `127.0.0.1:0` with
//! every other IP transport cleared, on `presets::Minimal` — which configures a
//! crypto provider and *nothing else*: no relays, no DNS, no pkarr. Pairing is
//! the ticket, which carries the loopback address, so no lookup service is
//! needed either. Nothing here contacts a relay server, not even an in-process
//! one; there is no server to be hermetic about.
//!
//! Every test in this file is a plain `#[test]`, not a `#[tokio::test]`. That
//! is the point: `Node`'s whole surface is synchronous, because Dart calls it
//! from an actor thread, and this file drives it exactly as the actor does —
//! call a sync method, then wait for an event. If any method here started
//! blocking on the network, these tests would be the first thing to notice.
//!
//! # Why this file is `mod`-ed into `node.rs` instead of linking the crate
//!
//! It cannot link the crate. The bridge is a `rust_shared_library`, i.e. a
//! cdylib, and rustc cannot `extern crate` a cdylib — so no `rust_test` can
//! take it as a `deps` entry. `cargo test` is not an option either: the crate's
//! `frustrate` dependency is a Bazel label, not a cargo one, and
//! `frustrate_generated.rs` is a build artifact, so `cargo check` here fails
//! before it starts.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use super::*;

/// Generous: the whole exchange is loopback, but CI machines stall.
const DEADLINE: Duration = Duration::from_secs(20);

/// A node plus the channel its events arrive on.
struct Probe {
    node: Node,
    events: Receiver<PeerEvent>,
    /// Events pulled off the channel that no `wait` has claimed yet.
    ///
    /// Not an optimisation: `Connected`, `Path` and `Message` are produced by
    /// three independent tasks, so their relative order is a scheduling detail.
    /// Without this buffer, waiting for `Message` would silently discard the
    /// `Path` that arrived first, and the test would assert a race.
    pending: RefCell<Vec<PeerEvent>>,
}

impl Probe {
    fn open(nickname: &str) -> Probe {
        let (tx, rx): (Sender<PeerEvent>, Receiver<PeerEvent>) = mpsc::channel();
        let node = Node::open_local(nickname, tx).expect("bind on loopback");
        Probe {
            node,
            events: rx,
            pending: RefCell::new(Vec::new()),
        }
    }

    /// Claim the first event `pick` accepts, from the buffer or from the
    /// channel, or fail naming everything that was seen instead.
    fn wait<T>(&self, what: &str, mut pick: impl FnMut(&PeerEvent) -> Option<T>) -> T {
        let mut pending = self.pending.borrow_mut();
        if let Some(index) = pending.iter().position(|event| pick(event).is_some()) {
            let found = pick(&pending[index]).expect("just matched");
            pending.remove(index);
            return found;
        }

        let deadline = std::time::Instant::now() + DEADLINE;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.events.recv_timeout(left) {
                Ok(event) => {
                    if let Some(found) = pick(&event) {
                        return found;
                    }
                    pending.push(event);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for {what}; saw {:?}", described(&pending))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "the node dropped its sink while waiting for {what}; saw {:?}",
                        described(&pending)
                    )
                }
            }
        }
    }

    /// The ticket this node announced at bind.
    fn listening_ticket(&self) -> String {
        self.wait("Listening", |event| match event {
            PeerEvent::Listening { ticket } => Some(ticket.clone()),
            _ => None,
        })
    }

    /// The peer id and nickname of the next peer to connect.
    fn connected(&self) -> (String, String) {
        self.wait("Connected", |event| match event {
            PeerEvent::Connected { peer, nickname } => Some((peer.clone(), nickname.clone())),
            _ => None,
        })
    }
}

fn described(events: &[PeerEvent]) -> Vec<String> {
    events.iter().map(describe).collect()
}

/// `PeerEvent` has no `Debug` — it is a `#[bridge]` enum, and codegen derives
/// nothing — so failure messages need this by hand.
fn describe(event: &PeerEvent) -> String {
    match event {
        PeerEvent::Listening { ticket } => format!("Listening({} chars)", ticket.len()),
        PeerEvent::Online => "Online".to_string(),
        PeerEvent::Dialing { peer } => format!("Dialing({})", short(peer)),
        PeerEvent::Connected { peer, nickname } => {
            format!("Connected({}, {nickname})", short(peer))
        }
        PeerEvent::Path { peer, direct } => format!("Path({}, direct={direct})", short(peer)),
        PeerEvent::Message { peer, body, .. } => format!("Message({}, {body:?})", short(peer)),
        PeerEvent::Left { peer, why } => format!("Left({}, {why})", short(peer)),
        PeerEvent::Failed { peer, detail, .. } => {
            let who = peer.as_deref().map(short).unwrap_or_else(|| "-".into());
            format!("Failed({who}, {detail})")
        }
    }
}

fn short(peer: &str) -> String {
    peer.chars().take(8).collect()
}

/// The acceptance criterion: two endpoints connect and exchange a message,
/// in both directions, with no external network.
#[test]
fn two_endpoints_exchange_a_message() {
    let alice = Probe::open("alice");
    let bob = Probe::open("bob");

    let alice_ticket = alice.listening_ticket();
    // A ticket is worthless if it carries no way to reach us. On loopback that
    // is the only addressing there is, since nothing else is configured.
    assert!(
        parse_ticket(&alice_ticket)
            .expect("our own ticket parses")
            .addrs
            .iter()
            .any(|addr| addr.is_ip()),
        "the bind-time ticket must carry a direct address"
    );
    let _ = bob.listening_ticket();

    // Fire and forget. The outcome is an event, which is the whole shape of
    // this API and the reason the actor never wedges.
    bob.node.connect(alice_ticket);

    let (alice_id, alice_nick) = bob.connected();
    let (bob_id, bob_nick) = alice.connected();
    assert_eq!(alice_nick, "alice");
    assert_eq!(bob_nick, "bob");

    bob.node
        .send(&alice_id, "hello from bob".to_string())
        .expect("bob knows alice");
    let got = alice.wait("Message", |event| match event {
        PeerEvent::Message { peer, body, .. } if peer == &bob_id => Some(body.clone()),
        _ => None,
    });
    assert_eq!(got, "hello from bob");

    alice
        .node
        .send(&bob_id, "hello back".to_string())
        .expect("alice knows bob");
    let got = bob.wait("Message", |event| match event {
        PeerEvent::Message { peer, body, .. } if peer == &alice_id => Some(body.clone()),
        _ => None,
    });
    assert_eq!(got, "hello back");

    // Loopback is a direct path, so the badge must say so. This is the M1
    // claim: `Path { direct: true }`, with relays asserted nowhere.
    let direct = bob.wait("Path", |event| match event {
        PeerEvent::Path { peer, direct } if peer == &alice_id => Some(*direct),
        _ => None,
    });
    assert!(direct, "a loopback path is direct");
}

/// Disconnecting is reported to both sides, and by exactly one producer.
#[test]
fn disconnect_is_reported_to_both_sides() {
    let alice = Probe::open("alice");
    let bob = Probe::open("bob");
    let ticket = alice.listening_ticket();
    let _ = bob.listening_ticket();

    bob.node.connect(ticket);
    let (alice_id, _) = bob.connected();
    let (bob_id, _) = alice.connected();

    bob.node.disconnect(&alice_id);

    let why = bob.wait("Left (local)", |event| match event {
        PeerEvent::Left { peer, why } if peer == &alice_id => Some(why.clone()),
        _ => None,
    });
    assert!(!why.is_empty(), "Left always names a reason");

    alice.wait("Left (remote)", |event| match event {
        PeerEvent::Left { peer, .. } if peer == &bob_id => Some(()),
        _ => None,
    });

    // Idempotent and silent: the UI asking twice is not an error.
    bob.node.disconnect(&alice_id);
    // And a message to a peer that left is a synchronous, local rejection.
    let err = bob
        .node
        .send(&alice_id, "anyone there".to_string())
        .expect_err("the peer is gone");
    assert!(err.contains("not connected"), "unhelpful error: {err}");
}

/// A ticket the user mistyped fails immediately, by name, without a dial.
#[test]
fn a_bad_ticket_fails_before_the_network() {
    let alice = Probe::open("alice");
    let _ = alice.listening_ticket();

    alice.node.connect("this is not a ticket".to_string());

    let detail = alice.wait("Failed(BadTicket)", |event| match event {
        PeerEvent::Failed {
            peer: None,
            kind: FailureKind::BadTicket,
            detail,
        } => Some(detail.clone()),
        _ => None,
    });
    assert!(detail.contains("not a ticket"), "unhelpful: {detail}");
}

/// Sending to a peer we never connected to is a `Result`, not an event: it is a
/// synchronous, local fact and the caller is already awaiting an answer.
#[test]
fn sending_to_an_unknown_peer_is_a_synchronous_error() {
    let alice = Probe::open("alice");
    let _ = alice.listening_ticket();

    let stranger = iroh::SecretKey::generate().public().to_string();
    let err = alice
        .node
        .send(&stranger, "hello?".to_string())
        .expect_err("nobody by that name");
    assert!(err.contains("not connected"), "unhelpful error: {err}");

    let err = alice
        .node
        .send("not-an-endpoint-id", "hello?".to_string())
        .expect_err("not even a name");
    assert!(err.contains("endpoint id"), "unhelpful error: {err}");
}

/// Dropping a `Node` returns promptly. The `Drop` path is two bounded waits
/// (2s for the close, 500ms for the runtime); if either were unbounded, Dart's
/// `dispose()` would hang behind it and the actor thread would outlive the app.
#[test]
fn dropping_a_node_is_bounded() {
    let alice = Probe::open("alice");
    let bob = Probe::open("bob");
    let ticket = alice.listening_ticket();
    let _ = bob.listening_ticket();
    bob.node.connect(ticket);
    let _ = bob.connected();
    let _ = alice.connected();

    let start = std::time::Instant::now();
    drop(bob);
    drop(alice);
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(6),
        "disposal took {took:?}; the bounds are 2s + 500ms per node"
    );
}
