//! A relay a test harness can start, address, and stop.
//!
//!     bazel run //relay:relay_dev
//!
//! Prints one line naming the URL it bound, then `relay: ready`, then relays
//! until stdin reaches EOF. That contract is copied deliberately from
//! //peerbot:peerbot — the browser harness spawns both the same way, waits for
//! the same shape of readiness line, and shuts both down by closing stdin. A
//! fixture that needed its own supervision idiom would be a second thing to get
//! wrong.
//!
//! # Why this exists when `iroh_relay --dev` already does
//!
//! `--dev` hardcodes port 3340 **and** a metrics listener on 9090, offers no
//! port-0 option, and prints no line a harness can parse for readiness. Three
//! ways for two concurrent runs to collide, where an ephemeral port has none.
//! This binary is ~40 lines against upstream's 959-line CLI precisely because
//! it does one thing: bind loopback, say where, wait.
//!
//! It is **not** a deployment artifact. Plain HTTP, loopback only, no
//! certificate, no access control. `relay.toml` and //relay:iroh_relay are the
//! shape a real host runs; this is the shape a test runs.

use std::{io::Read, process::ExitCode};

use iroh_relay_server::spawn_loopback_relay;

#[tokio::main]
async fn main() -> ExitCode {
    let (server, url) = match spawn_loopback_relay().await {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("relay: error {err}");
            return ExitCode::FAILURE;
        }
    };

    // The harness parses this line for the URL to hand both peers. Keep the
    // `relay: <verb> <value>` shape — peerbot's stdout reads the same way.
    println!("relay: url {url}");
    println!("relay: ready");

    // Block a worker thread on stdin rather than adding tokio's `io-std`
    // feature: this hub's tokio is `rt-multi-thread`/`macros`/`time`, and one
    // parked blocking thread costs less than widening a 358-package graph.
    let _ = tokio::task::spawn_blocking(|| {
        let mut sink = String::new();
        let _ = std::io::stdin().read_to_string(&mut sink);
    })
    .await;

    println!("relay: bye");
    match server.shutdown().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("relay: error on shutdown {err}");
            ExitCode::FAILURE
        }
    }
}
