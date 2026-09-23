//! `peerbot` — the headless half of the two-peer test.
//!
//! A complete Tin Can peer with no UI: it binds an iroh endpoint, prints its
//! ticket, speaks the same ALPN and the same frame codec as the app, and echoes
//! every message it receives back with a prefix. Everything it observes it
//! prints as one line on stdout, so a driver can assert protocol facts — "this
//! peer connected", "these bytes arrived" — that a screenshot cannot.
//!
//! Bot-versus-app rather than two app instances is deliberate: one UI to drive,
//! deterministic timing, and an independent witness on the wire.
//!
//! # Hermetic by default
//!
//! `--preset minimal` (the default) is `presets::Minimal`: a crypto provider
//! and nothing else — no relays, no DNS, no pkarr. Pairing is the ticket, which
//! carries this host's direct addresses, so two peers on one machine connect
//! over loopback and no packet needs to leave it. `--preset n0` exists for
//! manual cross-network runs and is not what the driver uses.
//!
//! `--relay <url>` replaces whatever relay the preset chose, and nothing else.
//! `--preset minimal --relay http://localhost:3340` is therefore the fully
//! self-hosted peer: one relay, of our own, and no n0 service of any kind in
//! the picture — which is what a local `//relay:iroh_relay --dev` run wants on
//! this side of the conversation.
//!
//! # Shutting down
//!
//! Graceful shutdown is **stdin EOF** (or the line `quit`), not SIGINT: the
//! `signal` feature of tokio is not enabled in this module's crate hub, which
//! resolves one feature-unified graph shared with the app, so it cannot be
//! enabled without a second hub. Closing the child's stdin is what a driver
//! does anyway, and it is the one shutdown signal that survives the parent
//! being killed.
//!
//! # Output
//!
//! One `peerbot: <verb> <args…>` line per event, newline-delimited, in order.
//! Bodies are escaped (`\n`, `\\`) so one message is always one line.
//!
//! ```text
//! peerbot: id 0f3c…            our endpoint id, lowercase hex
//! peerbot: relay http://…      only with --relay: the relay in our own address
//! peerbot: ticket eyJ…         paste this into the app's connectField
//! peerbot: ready               bound, listening, accepting
//! peerbot: connected <peer> <nickname>
//! peerbot: recv <peer> <body>
//! peerbot: sent <peer> <body>
//! peerbot: left <peer> <why>
//! peerbot: error <detail>
//! peerbot: bye
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::{presets, Connection, ReadExactError, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

/// The frame codec, *the same file* the app compiles.
///
/// `src/proto.rs` is a symlink to `../../bridge/src/proto.rs`. Not a copy and
/// not a vendored fork: one inode, so the app and the thing that tests the app
/// cannot disagree about the wire. The bridge package does not `exports_files`
/// it, and this crate is not allowed to edit that package, so a symlink is how
/// a second Bazel package declares it as a source.
mod proto;

/// Must equal `bridge/src/node.rs`'s `ALPN`.
///
/// Copied, because it lives in `node.rs`, which is unlinkable from here: that
/// file needs `frustrate` and iroh and is compiled into a cdylib. `proto.rs` is
/// deliberately dependency-light so it can be shared, and the ALPN is the one
/// wire constant it does not carry — so the single value that *must* match is
/// the single value that is duplicated. `peerbot_test` pins the literal, so a
/// change to `node.rs` that forgets this file leaves a test asserting the old
/// value here — a tripwire, not a fix.
const ALPN: &[u8] = b"frustrate/tin-can/0";

/// How long `--preset minimal` waits for the endpoint to report a direct
/// address before printing a ticket anyway.
///
/// A ticket with no addresses is unusable on `presets::Minimal`, where there is
/// no relay and no discovery to fall back on, so printing one early would hand
/// the driver a ticket that can only fail.
const ADDR_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on the graceful close, for the same reason `node.rs` bounds its:
/// `Endpoint::close` waits for the close to reach every peer.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Everything the command line can say.
struct Args {
    nickname: String,
    /// Dial this ticket once bound. The driver leaves it unset — the *app*
    /// dials, because that is the button a user presses.
    connect: Option<String>,
    /// Prepended to every echoed message, so the app's log distinguishes what
    /// it sent from what came back.
    prefix: String,
    /// `false` is `presets::Minimal` (hermetic); `true` is `presets::N0`.
    n0: bool,
    /// Relay through this URL instead of whatever the preset chose.
    ///
    /// Same semantics as `Node::open`'s `relay`: unset keeps the preset's own
    /// relay configuration exactly (none at all under `minimal`, n0's under
    /// `n0`), and a URL replaces it and changes nothing else. `--preset minimal
    /// --relay <url>` is therefore the fully self-hosted peer — one relay, no
    /// n0 anything — and it is what a local relay run wants on this side.
    relay: Option<String>,
}

impl Args {
    fn parse() -> Result<Args, String> {
        let mut args = Args {
            nickname: "peerbot".to_string(),
            connect: None,
            prefix: "echo: ".to_string(),
            n0: false,
            relay: None,
        };
        let mut argv = std::env::args().skip(1);
        while let Some(flag) = argv.next() {
            let mut value = || {
                argv.next()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--nickname" => args.nickname = value()?,
                "--connect" => args.connect = Some(value()?),
                "--prefix" => args.prefix = value()?,
                "--relay" => args.relay = Some(value()?),
                "--preset" => {
                    args.n0 = match value()?.as_str() {
                        "minimal" => false,
                        "n0" => true,
                        other => return Err(format!("--preset must be minimal or n0, not {other}")),
                    }
                }
                "--help" | "-h" => {
                    println!(
                        "peerbot [--nickname NAME] [--connect TICKET] [--prefix P] \
                         [--preset minimal|n0] [--relay URL]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown flag {other}")),
            }
        }
        Ok(args)
    }
}

/// Everything a connection task needs. Cloned per connection.
#[derive(Clone)]
struct Bot {
    nickname: String,
    prefix: String,
    /// Every live peer's outbound queue. `send`ing to a peer is a poke at this,
    /// never a wait — the same shape `node.rs` uses, for the same reason.
    peers: Arc<Mutex<HashMap<EndpointId, mpsc::UnboundedSender<String>>>>,
}

fn main() -> std::process::ExitCode {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(e) => {
            say(&format!("error {e}"));
            return std::process::ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("peerbot")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            say(&format!("error could not start a runtime: {e}"));
            return std::process::ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            say(&format!("error {e}"));
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let mut builder = if args.n0 {
        Endpoint::builder(presets::N0)
    } else {
        Endpoint::builder(presets::Minimal)
    };
    // Same shape as the app's `builder_for`: the preset first, then one
    // `relay_mode` on top of it, which *replaces* the preset's relay transport
    // rather than adding to it. Parsed before the bind, so a typo is an error
    // line on stdout and exit 1, never an endpoint that quietly never relays.
    let relaying = args.relay.is_some();
    if let Some(text) = args.relay.as_deref() {
        builder = builder.relay_mode(RelayMode::custom([parse_relay_url(text)?]));
    }
    let endpoint = builder
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| format!("could not bind an endpoint: {e}"))?;

    let bot = Bot {
        nickname: args.nickname,
        prefix: args.prefix,
        peers: Arc::new(Mutex::new(HashMap::new())),
    };

    say(&format!("id {}", endpoint.id()));
    let addr = addr_with_paths(&endpoint, relaying).await;
    if relaying {
        // The one line that says whether the relay handshake actually
        // completed. A ticket with no relay url in it, on a run that asked for
        // one, means the peer that pastes it can only reach us by IP.
        match addr.relay_urls().next() {
            Some(url) => say(&format!("relay {url}")),
            None => say(&format!(
                "error the relay was configured but the endpoint has no relay \
                 address after {}s; the ticket below carries IP addresses only",
                ADDR_TIMEOUT.as_secs()
            )),
        }
    }
    say(&format!("ticket {}", ticket_for(&addr)));

    let accepting = tokio::spawn(accept_loop(endpoint.clone(), bot.clone()));

    if let Some(ticket) = args.connect {
        let addr = parse_ticket(&ticket)?;
        tokio::spawn(dial(endpoint.clone(), addr, bot.clone()));
    }

    say("ready");

    // The one place this program waits. Every other loop is a task.
    commands(bot).await;

    accepting.abort();
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, endpoint.close()).await;
    say("bye");
    Ok(())
}

/// Wait until the endpoint knows an address to publish, then hand back the
/// address as it stands. See [`ADDR_TIMEOUT`].
///
/// `relaying` raises the bar: a run that configured a relay wants the relay
/// url *in the ticket*, because that is the only address a peer on another
/// network can use, and it appears seconds after the IP addresses do — only
/// once the relay handshake completes. Waiting for the IP alone would print a
/// perfectly valid ticket that happens to route around the thing under test.
async fn addr_with_paths(endpoint: &Endpoint, relaying: bool) -> EndpointAddr {
    let deadline = std::time::Instant::now() + ADDR_TIMEOUT;
    loop {
        let addr = endpoint.addr();
        let enough = addr.addrs.iter().any(|a| a.is_ip())
            && (!relaying || addr.addrs.iter().any(|a| a.is_relay()));
        if enough || std::time::Instant::now() >= deadline {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Parse a relay URL, and say what is wrong with one that is not.
///
/// `bridge/src/node.rs`'s `parse_relay_url`, copied for the same reason `ALPN`
/// is: that file needs frustrate and is compiled into a cdylib, so nothing
/// here can link it.
fn parse_relay_url(text: &str) -> Result<RelayUrl, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the relay url is empty; omit --relay to use the preset's relays".to_string());
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

/// Read commands from stdin until EOF. Returning ends the process.
///
/// `send <peer|*> <text>` queues a message; `quit` and EOF both shut down.
/// Anything else is reported rather than ignored, because a driver that
/// mistypes a command would otherwise wait for an effect that never comes.
async fn commands(bot: Bot) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            // EOF, or stdin broke. Either way our driver is gone.
            Ok(None) | Err(_) => return,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "quit" {
            return;
        }
        match line.split_once(' ') {
            Some(("send", rest)) => match rest.split_once(' ') {
                Some((who, body)) => send_to(&bot, who, body.to_string()),
                None => say("error send needs a peer and a body"),
            },
            _ => say(&format!("error unknown command {}", escape(line))),
        }
    }
}

/// Queue `body` for one peer, or for every peer when `who` is `*`.
fn send_to(bot: &Bot, who: &str, body: String) {
    let peers = bot.peers.lock().unwrap_or_else(|e| e.into_inner());
    if who == "*" {
        for (id, out) in peers.iter() {
            if out.send(body.clone()).is_ok() {
                say(&format!("sent {id} {}", escape(&body)));
            }
        }
        return;
    }
    let Ok(id) = EndpointId::from_str(who) else {
        return say(&format!("error {who} is not an endpoint id"));
    };
    match peers.get(&id) {
        Some(out) if out.send(body.clone()).is_ok() => {
            say(&format!("sent {id} {}", escape(&body)))
        }
        Some(_) => say(&format!("error {id} is closing")),
        None => say(&format!("error not connected to {id}")),
    }
}

async fn accept_loop(endpoint: Endpoint, bot: Bot) {
    while let Some(incoming) = endpoint.accept().await {
        let bot = bot.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => adopt(conn, bot, false).await,
                Err(e) => say(&format!("error inbound handshake failed: {e}")),
            }
        });
    }
}

async fn dial(endpoint: Endpoint, addr: EndpointAddr, bot: Bot) {
    let peer = addr.id;
    match endpoint.connect(addr, ALPN).await {
        Ok(conn) => adopt(conn, bot, true).await,
        Err(e) => say(&format!("error could not reach {peer}: {e}")),
    }
}

/// Turn a fresh connection into a peer, exactly the way `node.rs` does.
///
/// **Both sides write one nickname frame immediately, then read one.** The
/// dialer opens the stream, the acceptor accepts it. Writing before reading
/// cannot deadlock: one small frame fits in the initial flow-control window.
async fn adopt(conn: Connection, bot: Bot, initiator: bool) {
    let id = conn.remote_id();
    let opened = if initiator {
        conn.open_bi().await
    } else {
        conn.accept_bi().await
    };
    let (mut send, mut recv) = match opened {
        Ok(pair) => pair,
        Err(e) => return say(&format!("error {id} stream failed: {e}")),
    };

    let hello = match proto::encode(&bot.nickname) {
        Ok(frame) => frame,
        Err(e) => return say(&format!("error our own nickname is unsendable: {e}")),
    };
    if let Err(e) = send.write_all(&hello).await {
        return say(&format!("error {id} hello failed: {e}"));
    }
    let nickname = match read_frame(&mut recv).await {
        Ok(Some(nickname)) => nickname,
        Ok(None) => return say(&format!("error {id} left before saying hello")),
        Err(e) => return say(&format!("error {id} {e}")),
    };

    let (out, queue) = mpsc::unbounded_channel();
    bot.peers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, out.clone());
    say(&format!("connected {id} {}", escape(&nickname)));

    tokio::spawn(write_loop(send, queue, id));
    read_loop(conn, recv, id, bot, out).await;
}

/// Drain the queue onto the wire.
async fn write_loop(
    mut send: SendStream,
    mut queue: mpsc::UnboundedReceiver<String>,
    id: EndpointId,
) {
    while let Some(body) = queue.recv().await {
        let frame = match proto::encode(&body) {
            Ok(frame) => frame,
            Err(e) => {
                say(&format!("error {id} unsendable message: {e}"));
                continue;
            }
        };
        if let Err(e) = send.write_all(&frame).await {
            return say(&format!("error {id} write failed: {e}"));
        }
    }
    let _ = send.finish();
}

/// Report every inbound frame and echo it back with the prefix.
async fn read_loop(
    conn: Connection,
    mut recv: RecvStream,
    id: EndpointId,
    bot: Bot,
    out: mpsc::UnboundedSender<String>,
) {
    let why = loop {
        match read_frame(&mut recv).await {
            Ok(Some(body)) => {
                say(&format!("recv {id} {}", escape(&body)));
                // Do not echo an echo. Two peerbots pointed at each other
                // otherwise amplify one message into an unbounded ping-pong —
                // observed, at about 30 MB of log in three seconds — and the
                // app under test never sends a body carrying this prefix, so
                // nothing a driver asserts is affected.
                if !bot.prefix.is_empty() && body.starts_with(&bot.prefix) {
                    continue;
                }
                let echo = format!("{}{body}", bot.prefix);
                if out.send(echo.clone()).is_ok() {
                    say(&format!("sent {id} {}", escape(&echo)));
                }
            }
            Ok(None) => break "the peer closed the connection".to_string(),
            Err(e) => break e,
        }
    };
    bot.peers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    conn.close(0u32.into(), b"bye");
    say(&format!("left {id} {}", escape(&why)));
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

fn ticket_for(addr: &EndpointAddr) -> String {
    EndpointTicket::new(addr.clone()).to_string()
}

/// Accept a ticket, or a bare endpoint id — the same two forms `node.rs` takes.
fn parse_ticket(text: &str) -> Result<EndpointAddr, String> {
    let text = text.trim();
    if let Ok(ticket) = EndpointTicket::from_str(text) {
        return Ok(ticket.endpoint_addr().clone());
    }
    EndpointId::from_str(text)
        .map(EndpointAddr::new)
        .map_err(|e| format!("not a ticket and not an endpoint id: {e}"))
}

/// One event, one line, flushed.
///
/// Rust's stdout is a `LineWriter`, so `println!` already flushes on the
/// newline even through a pipe — the explicit flush is belt and braces for a
/// driver that would otherwise wait forever on a line that is sitting in a
/// buffer.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "peerbot: {line}");
    let _ = out.flush();
}

/// Make a body safe to put on one line.
fn escape(body: &str) -> String {
    body.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_keeps_a_message_on_one_line() {
        assert_eq!(escape("a\nb"), "a\\nb");
        assert_eq!(escape("a\\nb"), "a\\\\nb");
        assert!(!escape("one\ntwo\r\nthree").contains('\n'));
    }

    /// The one constant this crate copies rather than shares. If the app's ALPN
    /// changes, the two peers stop negotiating and every driven test fails with
    /// a handshake error that names nothing — so pin the literal here too.
    #[test]
    fn the_alpn_is_the_one_in_node_rs() {
        assert_eq!(ALPN, b"frustrate/tin-can/0");
    }

    #[test]
    fn a_bare_endpoint_id_is_a_ticket() {
        let id = iroh::SecretKey::generate().public();
        assert_eq!(parse_ticket(&format!(" {id} ")).unwrap().id, id);
        assert!(parse_ticket("hello").is_err());
    }

    /// The same three cases `bridge/src/node.rs` pins, because this is a copy
    /// of that function and a copy that drifts is worse than no copy.
    #[test]
    fn a_relay_url_needs_a_scheme() {
        assert!(parse_relay_url("http://localhost:3340").is_ok());
        assert!(parse_relay_url("https://relay.example.com").is_ok());
        assert!(parse_relay_url("localhost:3340")
            .unwrap_err()
            .contains("names no host"));
        assert!(parse_relay_url("  ").unwrap_err().contains("empty"));
    }

    /// The ticket peerbot prints is one the app can parse — the whole pairing
    /// contract, in one assertion.
    #[test]
    fn the_printed_ticket_round_trips() {
        let addr = EndpointAddr::new(iroh::SecretKey::generate().public());
        let parsed = parse_ticket(&ticket_for(&addr)).expect("our own ticket parses");
        assert_eq!(parsed.id, addr.id);
    }
}
