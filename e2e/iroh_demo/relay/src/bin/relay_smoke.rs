//! Proves a relay actually relays.
//!
//! Connects two relay clients to the server under test and has one send a
//! datagram addressed to the other's `EndpointId`. If the bytes come out the
//! far side, the server is doing the one job a relay has. Exits 0 on success,
//! 1 with a diagnostic on failure.
//!
//!     bazel run //relay:relay_smoke                       # http://localhost:3340
//!     bazel run //relay:relay_smoke -- https://relay.example.com
//!
//! This is the post-deploy health check. Nothing about it assumes localhost —
//! point it at the real hostname and it exercises the TLS path, the
//! certificate, and the access control in one shot. That is also why it stays
//! a binary and did not become the new //relay:relay_server_test: the test
//! spawns its own server and proves the *code* relays, while this proves a
//! *deployment* does, and only a human or the deploy script knows which one to
//! ask about.
//!
//! The exchange itself is shared with that test — see
//! [`iroh_relay_server::relay_one_datagram`].

use std::process::ExitCode;

use iroh_base::RelayUrl;
use iroh_relay_server::relay_one_datagram;

const DEFAULT_URL: &str = "http://localhost:3340";
const PAYLOAD: &[u8] = b"tin can smoke test";

#[tokio::main]
async fn main() -> ExitCode {
    let url: String = std::env::args().nth(1).unwrap_or(DEFAULT_URL.to_string());
    match run(&url).await {
        Ok(()) => {
            println!("OK: {url} relayed {} bytes between two clients", PAYLOAD.len());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("FAIL: {url}: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url: RelayUrl = url.parse()?;
    relay_one_datagram(&url, PAYLOAD).await
}
