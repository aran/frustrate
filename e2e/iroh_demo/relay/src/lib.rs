//! The one thing this package owns that is code: a check that a relay relays.
//!
//! The server binary is upstream's, aliased as //relay:iroh_relay. What lives
//! here is the relay-protocol exchange used to verify one, shared by the two
//! callers that need it:
//!
//! * **//relay:relay_smoke** — the post-deploy health check, pointed at a URL
//!   you supply. Not hermetic by design: it tests a server someone else started.
//! * **//relay:relay_server_test** — the test at the bottom of this file, which
//!   spawns a server in-process on an ephemeral loopback port and relays
//!   through it. No fixed port, no DNS, no ACME, no packet that leaves the host.
//!
//! Until this file had code, `//relay`'s only test was a `build_test` and
//! *nothing in the module exercised the relay path at all* — the coverage gap
//! the second crate hub cost. Spawning the server in-process closes it without
//! reopening that wound:
//! `iroh_relay::server` is behind the `server` feature this hub already
//! enables, so the test costs zero new packages in either lockfile.

use std::time::Duration;

use iroh_base::{RelayUrl, SecretKey};
use iroh_dns::dns::DnsResolver;
use iroh_relay::{
    client::ClientBuilder,
    protos::relay::{ClientToRelayMsg, RelayToClientMsg},
    tls::{CaTlsConfig, default_provider},
};
use n0_future::{SinkExt, StreamExt};

/// How long to wait for a datagram to come out the far side.
///
/// Bounded on purpose: a relay that accepts the frame and never forwards it
/// must *fail* its caller, not hang it.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Connect two relay clients to `url` and pass one datagram between them.
///
/// This is the whole job a relay has, and it is deliberately a relay-protocol
/// client rather than two `iroh::Endpoint`s: full endpoints would drag `iroh`
/// and its 378-package graph into this package's lockfile to test something
/// that lives one layer below them. What an `Endpoint` on `RelayMode::Custom`
/// does over the wire is exactly this.
pub async fn relay_one_datagram(
    url: &RelayUrl,
    payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    // `insecure_skip_verify` is NOT used: a self-hosted relay must present a
    // certificate the browser would also accept (M5 is served over HTTPS and
    // needs `wss://`), so this check has to fail when the certificate is bad.
    // Against a plain-http relay TLS never engages, but the builder demands a
    // config regardless.
    let tls = CaTlsConfig::default().client_config(default_provider())?;
    let dns = DnsResolver::new();

    let secret_a = SecretKey::generate();
    let secret_b = SecretKey::generate();
    let id_a = secret_a.public();
    let id_b = secret_b.public();

    let client_a = ClientBuilder::new(url.clone(), secret_a, dns.clone())
        .tls_client_config(tls.clone())
        .connect()
        .await?;
    let client_b = ClientBuilder::new(url.clone(), secret_b, dns)
        .tls_client_config(tls)
        .connect()
        .await?;
    println!("connected: a={id_a} b={id_b}");

    let (mut recv_b, _send_b) = client_b.split();
    let (_recv_a, mut send_a) = client_a.split();

    send_a
        .send(ClientToRelayMsg::Datagrams {
            dst_endpoint_id: id_b,
            datagrams: payload.into(),
        })
        .await?;
    println!("sent: a -> b");

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let msg = tokio::time::timeout_at(deadline, recv_b.next())
            .await
            .map_err(|_| format!("no datagram at b within {TIMEOUT:?}"))?
            .ok_or("relay closed b's connection")??;
        match msg {
            RelayToClientMsg::Datagrams {
                remote_endpoint_id,
                datagrams,
            } => {
                if remote_endpoint_id != id_a {
                    return Err(
                        format!("datagram from {remote_endpoint_id}, expected {id_a}").into()
                    );
                }
                if datagrams.contents.as_ref() != payload {
                    return Err(format!("payload mismatch: {:?}", datagrams.contents).into());
                }
                println!("received: b <- a, {} bytes, contents match", payload.len());
                return Ok(());
            }
            // Status/Ping/Restarting are routine server chatter; keep reading.
            other => println!("(ignoring {other:?})"),
        }
    }
}

/// Spawn a relay on an ephemeral loopback port, for tests and for
/// //relay:relay_dev.
///
/// Port 0 rather than a fixed one, and this is the reason the fixture is not
/// `iroh_relay --dev`: that mode hardcodes 3340 *and* a metrics server on 9090,
/// so two concurrent runs collide on both. Here the kernel picks, nothing
/// collides, and [`Server::http_addr`] reports what it picked.
///
/// [`Server::http_addr`]: iroh_relay::server::Server::http_addr
pub async fn spawn_loopback_relay()
-> Result<(iroh_relay::server::Server, RelayUrl), Box<dyn std::error::Error>> {
    use std::net::Ipv4Addr;

    use iroh_relay::server::{RelayConfig, Server, ServerConfig};

    // `ServerConfig` is #[non_exhaustive], so this is default-then-assign
    // rather than a struct literal. The defaults are the ones we want: no QUIC
    // address discovery, and `metrics_addr: None` so no second listener exists
    // to collide with anything.
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, 0)));

    let server = Server::spawn(config).await?;
    let addr = server
        .http_addr()
        .ok_or("relay spawned with no HTTP listener")?;

    // http:// and not ws://: a RelayUrl is exchanged in tickets with the http
    // scheme, and iroh's client maps http -> ws when it dials
    // (iroh-relay/src/client.rs:390-400).
    let url: RelayUrl = format!("http://{addr}").parse()?;
    Ok((server, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relay path, under test, for the first time.
    ///
    /// Hermetic: loopback, an ephemeral port, no DNS server consulted (the URL
    /// carries an IP literal), no certificate, no ACME. It passes with the
    /// machine's network sandboxed off — verified with
    /// `--sandbox_default_allow_network=false`.
    #[tokio::test]
    async fn a_relay_relays() -> Result<(), Box<dyn std::error::Error>> {
        let (server, url) = spawn_loopback_relay().await?;
        println!("relay: url {url}");

        relay_one_datagram(&url, b"tin can server test").await?;

        server.shutdown().await?;
        Ok(())
    }

    /// The negative half: with no relay listening, the check must fail rather
    /// than hang or pass vacuously.
    ///
    /// Without this, a `relay_one_datagram` that silently returned `Ok` on a
    /// dead socket would leave the test above green forever.
    #[tokio::test]
    async fn a_dead_relay_fails_the_check() {
        // Port 1 on loopback: privileged, never bound by this test, and refuses
        // immediately rather than making us wait out TIMEOUT.
        let url: RelayUrl = "http://127.0.0.1:1".parse().unwrap();
        let err = relay_one_datagram(&url, b"unreachable")
            .await
            .expect_err("a relay that does not exist must not pass the check");
        println!("(expected failure: {err})");
    }
}
