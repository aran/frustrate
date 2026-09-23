# The Tin Can relay

Everything under "What a human must do" costs money or opens a port to the
internet. Read each script before running it.

## What this is

One `iroh-relay` binary with `features = ["server"]`, its production config, its
systemd unit, the GCP automation to put it on a VM, and — the only code this
package really owns — a check that a relay relays, in three shapes: a test that
spawns its own server, a fixture a harness can start, and a health check for a
deployment. There is no Flutter, Dart, or frustrate machinery in it.

**The demo does not need it.** Dialing by ticket over n0's public relays
connects across networks with zero configuration. Self-hosting buys owning the
stack, and it becomes *non-optional* for a browser peer: a page served over
HTTPS can only talk to a `wss://` relay with a publicly trusted certificate, and
`insecure_skip_verify` is native-only.

## Targets

| Target | What it is |
| --- | --- |
| `//relay:iroh_relay` | The relay server. An alias for upstream's own `[[bin]] iroh-relay`, surfaced by `crate.annotation(gen_binaries = [...])` in `//:MODULE.bazel`. |
| `//relay:relay_support` | The relay-protocol exchange — two clients, one datagram, checked at the far end — plus the loopback spawn helper. Shared, so a deployment and the code are verified the same way. |
| `//relay:relay_server_test` | **The relay path's only automated coverage.** Spawns a server in-process on an ephemeral loopback port and relays through it. A real `bazel test` target: nothing external listening, no fixed port, passes with the network sandboxed off. |
| `//relay:relay_dev` | A relay a test harness starts. Loopback, ephemeral port, `relay: url …` / `relay: ready` on stdout, shutdown on stdin EOF — deliberately peerbot's contract. |
| `//relay:relay_smoke` | The same exchange against a URL you supply: the post-deploy health check. Exercises TLS, the certificate and the access control, which a loopback test by construction cannot. |
| `//relay:relay_build_test` | Builds the server and both binaries. Also the guard for `supported_platform_triples` — a missing triple resolves to `//:incompatible` and would otherwise pass as a silent skip. |
| `//relay:linux_x86_64` | The deployment platform. |

`relay_server_test` is the relay path's coverage, and it costs no package in
either lockfile: `iroh_relay::server` sits behind the `server` feature that
`@relay_crates` already enables.

The server is an alias rather than a `rust_binary` of our own on purpose:
upstream's `main.rs` is 959 lines of CLI and TOML-config handling, and a
vendored copy would drift from the schema this README documents. What this
package owns is the configuration and the deployment.

`iroh-relay` is depended on **directly**, not through the `iroh` facade — the
server lives behind that crate's `server` feature and the facade does not
re-export it.

## Running it locally

```
bazel test //relay:relay_server_test      # spawns its own relay; needs nothing
bazel run //relay:relay_dev               # a relay on an ephemeral loopback port
bazel run //relay:iroh_relay -- --dev     # plain HTTP on http://localhost:3340
bazel run //relay:relay_smoke             # defaults to that URL
```

**Prefer `relay_dev` to `iroh_relay --dev` for anything automated.** `--dev`
hardcodes port 3340 *and* a metrics listener on 9090, offers no port-0 option,
and prints no line a harness can parse for readiness — three ways for two
concurrent runs to collide. `relay_dev` binds an ephemeral port and announces
it:

```
$ bazel run //relay:relay_dev
relay: url http://127.0.0.1:61505/
relay: ready
```

`iroh_relay --dev` remains the right thing for a human who wants the *real*
server with its real CLI, and for validating `relay.toml` below.

`--dev` ignores every TLS field in a config file, so it is the only mode that
needs no certificate. It is also, usefully, an offline way to check that
`relay.toml` parses:

```
bazel run //relay:iroh_relay -- --dev --config-path=$PWD/relay/relay.toml
```

which loads the whole file and then refuses, because `--dev` is incompatible
with `cert_mode = "LetsEncrypt"`. That refusal *is* the validation: it happens
after parsing and before any network call.

Point the app at a local relay with:

```
bazel run //tools:two_peers -- --relay http://localhost:3340
```

which passes it to the app as `--dart-define=RELAY_URL=…` and to `peerbot` as
`--relay`. Both reach `Endpoint::builder` as:

```rust
let url: RelayUrl = "http://localhost:3340".parse()?;
Endpoint::builder(presets::N0).relay_mode(RelayMode::custom([url]))
```

Three things that are easy to get wrong here:

- **`presets::Empty` does not work.** It sets no `crypto_provider`, which is a
  mandatory builder option, so `bind()` always fails — its own rustdoc says so.
  The two shapes that do work are `presets::N0` plus a `relay_mode` (keeps
  pkarr/DNS discovery, replaces only the relay — what `bridge/src/node.rs`'s
  `builder_for` uses, and why) and `presets::Minimal` plus a `relay_mode` (no
  discovery at all — what `peerbot --preset minimal --relay …` uses).
  `Builder::relay_mode` *replaces* the relay transport rather than appending
  one, which is what makes "preset, then override" a one-line change.
- **No turbofish is needed.** `RelayMode::custom` takes an
  `IntoIterator<Item = RelayUrl>` and builds the `RelayMap` itself, so nothing
  has to name `RelayMap` and the `From<RelayUrl>` ambiguity never arises.
- **`http://` is fine.** `RelayUrl`'s `FromStr` is `Url::from_str` and imposes
  no scheme; `iroh-relay`'s client maps scheme `http` to `ws` and everything
  else to `wss` (`client.rs:272`), so a `--dev` relay is reachable by a real
  `iroh::Endpoint` with no TLS anywhere. **A browser is the exception** — a page
  served over HTTPS may only open `wss://`, so a browser peer needs a real
  certificate and `--dev` is a native-only convenience.

What `RelayUrl` will *not* catch is the obvious typo: `localhost:3340` parses
successfully, as a URL whose scheme is `localhost` and whose path is `3340`. It
has no host and can never connect. Both `node.rs` and `peerbot` reject a
host-less URL themselves, before the bind, for that reason.

## Ports

From `iroh-relay/src/defaults.rs`, which is authoritative. The crate README's
`7824` for QUIC address discovery is a typo.

| Port | Protocol | Purpose | Public? |
| --- | --- | --- | --- |
| 80 | tcp | Captive-portal probe, and the ACME HTTP-01 challenge | yes |
| 443 | tcp | The relay itself, `wss://` | yes |
| 7842 | udp | QUIC address discovery — what makes hole punching work | yes |
| 9090 | tcp | Metrics | **no** — bound to loopback by `relay.toml` |
| 3340 | tcp | `--dev` only | n/a |

## Building for the VM

```
bazel build //relay:iroh_relay --platforms=//relay:linux_x86_64
```

This works from macOS — it produces an `ELF 64-bit LSB pie executable, x86-64`
in ~30s. It needs a Linux **C** toolchain, and the reason is easy to
misdiagnose, so it is worth stating why.

Until `//:MODULE.bazel` registered a Linux `cc_toolchain` (`@llvm`,
zero-sysroot — see the comment there), it failed with:

```
ERROR: .../rules_rust+/ffi/cc/allocator_library/BUILD.bazel:21:11: While resolving
toolchains for target @@rules_rust+//ffi/cc/allocator_library:cc_allocator_library:
No matching toolchains found for types:
  @@bazel_tools//tools/cpp:toolchain_type
```

The Rust half is entirely fine. `--toolchain_resolution_debug` confirms the
cross toolchain resolves:

```
Selected execution platform @@platforms//host:host, type @@rules_rust+//rust:toolchain_type
  -> toolchain @@rules_rust++rust+rust_macos_aarch64__x86_64-unknown-linux-gnu__stable_tools//:rust_toolchain
```

and no crate resolved to `//:incompatible`, so `supported_platform_triples` is
right. What was missing is a **C** toolchain for Linux — and the first thing to
need one is not `ring`, it is rules_rust's own allocator shim, which every
`rust_binary` links. Supplying it means registering a Linux `cc_toolchain`,
which no module in these three repos did.

`deploy/deploy_relay.dart` builds **on the VM**, natively. Since the
cross-compile works, the shorter path is to build `//relay:iroh_relay
--platforms=//relay:linux_x86_64` on the developer's machine and upload one
binary — which also retires the awkwardness where Bazel's whole-graph module
resolution forces the whole frustrate worktree to be uploaded in order to build
a relay that uses none of it.

Also note `@rules_rust//rust/platform:x86_64-unknown-linux-gnu` is **not** a
platform — it is a constraint bundle, and passing it to `--platforms` fails
with "does not provide PlatformInfo". Hence `//relay:linux_x86_64`.

---

## What a human must do

Read every script before running it. Each `gcloud` call below either costs
money or opens a port to the internet.

### Prerequisites

- `gcloud` authenticated, with a default project and zone:
  `gcloud config set project <id> && gcloud config set compute/zone <zone>`
- A domain you control, with the ability to add an A record.
- A mailbox for Let's Encrypt expiry notices.
- Dart on `PATH` (the scripts need no pubspec; they import only `dart:io`).

### The three commands

**1. Create the VM, the static IP, and the firewall rules.**

```
dart run relay/deploy/create_relay_vm.dart
```

Creates, all in the active project:

- a **reserved static external IP** named `iroh-relay-ip`. Reserved rather than
  ephemeral because the DNS A record points at it and spot preemption would
  otherwise hand back a different address on restart;
- firewall rules `iroh-relay-allow-http` (tcp:80, tcp:443) and
  `iroh-relay-allow-quic` (udp:7842), scoped to the `iroh-relay` network tag
  and open to `0.0.0.0/0`. Metrics is not opened;
- a spot `e2-medium` / `ubuntu-2404-lts-amd64` VM with a 50 GB disk, tagged
  `iroh-relay`, holding that IP. Spot with
  `--instance-termination-action=STOP`, so preemption stops the VM and keeps
  the disk (and the cached certificate on it) rather than deleting it.

The startup script installs `build-essential`, bazelisk, and a 4 GB swapfile.
The swap is not optional: e2-medium has 4 GB of RAM and this build has ~350
crates in it. Pass `--machine-type=e2-standard-4` if it still thrashes; the
relay only needs e2-medium to *run*.

It prints the IP and stops.

**2. Create the DNS record, then edit `relay.toml`.** Nothing automates this.

```
relay.<your-domain>.   A   <the printed IP>
```

Wait until `dig +short relay.<your-domain>` returns it. Then set
`tls.hostname` and `tls.contact` in `relay/relay.toml`. `deploy_relay.dart`
refuses to run while either is a placeholder, because ordering a certificate
for `relay.example.com` would burn a Let's Encrypt rate-limit slot for a domain
nobody here controls.

The A record must resolve *before* the relay first starts: Let's Encrypt
resolves the name and connects back on port 80 to answer the HTTP-01 challenge.
While iterating, set `prod_tls = false` in `relay.toml` to use the staging
directory — staging certificates are untrusted by browsers, so a browser peer's
`wss://` will not work against them, but its rate limits are far looser.

**3. Build, install, start.**

```
dart run relay/deploy/deploy_relay.dart iroh-relay
```

Uploads the frustrate source tree, runs `bazel build -c opt --jobs=2
//relay:iroh_relay` on the VM (15-40 minutes on e2-medium), then installs:

| On the VM | From |
| --- | --- |
| `/usr/local/bin/iroh-relay` | the Bazel output |
| `/etc/iroh-relay/relay.toml` | `relay/relay.toml` |
| `/etc/systemd/system/iroh-relay.service` | `relay/iroh-relay.service` |

and runs `systemctl enable --now iroh-relay`. The unit runs under
`DynamicUser=yes` with `AmbientCapabilities=CAP_NET_BIND_SERVICE` — unprivileged,
but able to bind 80 and 443 — and keeps its ACME cache in the systemd-managed
`/var/lib/iroh-relay/certs`, which is what makes a restart cheap instead of a
re-issue.

### Verifying

From your own machine, not the VM:

```
bazel run //relay:relay_smoke -- https://relay.<your-domain>
```

That one command exercises DNS, the certificate chain (`relay_smoke`
deliberately does *not* use `insecure_skip_verify`, so a bad certificate fails
it), the websocket upgrade, access control, and the relaying itself. Then, to
check it survives a reboot:

```
gcloud compute instances reset iroh-relay
# wait ~60s
bazel run //relay:relay_smoke -- https://relay.<your-domain>
```

### Operating it

- **Preemption.** Spot means it will be stopped, without warning, and every
  relayed connection dies with it. `gcloud compute instances start iroh-relay`
  brings it back on the same IP; systemd starts the relay on boot. The app is
  guaranteed only to show a `Left` per peer at that moment — whether it can
  produce an `Offline` → `Online` transition is unverified.
- **Logs.** `gcloud compute ssh iroh-relay --command 'sudo journalctl -u iroh-relay -f'`
- **Metrics.** Loopback only, by design:
  `gcloud compute ssh iroh-relay --command 'curl -s localhost:9090/metrics'`
- **Secrets.** `IROH_RELAY_ACCESS_TOKEN` (with `access.shared_token`),
  `IROH_RELAY_HTTP_BEARER_TOKEN` (with `access.http`), and
  `IROH_RELAY_ACME_URL` (to point at staging or a local pebble) are read from
  `/etc/iroh-relay/env`, which the unit loads if present and which is not in
  git.
- **Tearing it down.** The VM, the firewall rules, and the address are three
  separate deletes; the address keeps costing while it is reserved and unused:
  ```
  gcloud compute instances delete iroh-relay
  gcloud compute firewall-rules delete iroh-relay-allow-http iroh-relay-allow-quic
  gcloud compute addresses delete iroh-relay-ip --region=<region>
  ```

---

## Testing it

`//relay:relay_server_test` starts the relay, relays a datagram between two
clients and shuts it down, inside `bazel test`, with no server started by a
human.

Run it with the network fence on, and remember the second flag:

```
$ bazel test //relay:relay_server_test --sandbox_default_allow_network=false \
    --nocache_test_results
```

`--nocache_test_results` is not optional — without it Bazel replays the cached
result and executes nothing, so the fence proves nothing. The fence itself
deserves a positive control before it is trusted: a throwaway `rust_test` that
connects to `1.1.1.1:443` passes without the flag and fails under it with
`Operation not permitted (os error 1)`.

**A local run does not prove the relay carries messages.** Two peers on one
host hole-punch to a direct loopback path seconds after connecting, and the
app's path badge reads `direct`. What a local relay demonstrably carries is
connection establishment. A conversation whose data path stays relayed for its
whole life is the browser's situation, by physics.

Local runs are plaintext `http://` throughout; `wss://` only happens against a
real deployment.
