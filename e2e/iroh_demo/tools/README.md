# The driven two-peer test

```sh
cd e2e/iroh_demo
bazel run //tools:two_peers
```

That is the whole command. From a clean checkout it builds both halves, starts
a headless peer, launches the app, drives it, asserts a real message crossed in
both directions, writes a screenshot, shuts everything down, and exits 0.

## What it asserts, and who witnesses it

| # | claim | witness |
|---|---|---|
| 1 | the app bound an endpoint | `ticketText` stops reading `no ticket yet` |
| 2 | the app dialled the bot | peerbot's `connected <id> drivenapp` on stdout |
| 3 | the app sees the peer | `peer:<botId>` appears in the widget tree |
| 4 | the message crossed | peerbot's `recv <appId> ping from the driven app` |
| 5 | the reply crossed back | `messageLog` contains `echo: ping from the driven app` |
| 6 | the path is direct | `path:<botId>` reads `direct` |
| 7 | nothing failed quietly | `failureText` reads `no failures` |

Claims 2 and 4 are peerbot's: it read the frame off a QUIC stream and decoded
it with the same `proto.rs` the app encoded it with (`peerbot/src/proto.rs` is
a symlink to `bridge/src/proto.rs`, so there is one copy of the codec, not
two). Claims 1, 3, 5, 6 and 7 are the app's own state, read back over the dev
tool's HTTP control channel. The nickname in claim 2 is the `--dart-define`
this run launched the app with, so the peer that dialled is provably this app.

The screenshot is an artifact, never evidence: nothing is concluded from it.

## Flags

| flag | effect |
|---|---|
| `--screenshot PATH` | where the PNG goes (default `$TMPDIR/tin_can_two_peers.png`) |
| `--no-shutdown` | leave the app and the bot running after the assertions |
| `--probe-restart` | after passing, issue `app.restart` and report whether the control channel survives (it does not, when the native library changed) |
| `--relay URL` | put **both** peers on `presets::Minimal` through that relay: one relay of ours, no n0 service at all. Start one with `bazel run //relay:relay_dev` |
| `--n0` | build the app with `PRESET=n0` — n0's public relays, pkarr and DNS. The only reason to use it is to check that path still works |

`--relay` and `--n0` are mutually exclusive: they are two answers to one
question, and the driver refuses both rather than silently preferring one.

On failure it exits 1 and prints everything peerbot said, the app's console
output from `/sessions/{appId}/logs`, and the tail of the dev tool's stderr.

## peerbot on its own

```sh
bazel run //peerbot:peerbot -- --nickname bob
```

Prints `peerbot: id …`, `peerbot: ticket …`, `peerbot: ready`, then one line
per event. Paste the ticket into a running app's connect field to chat with it
by hand. `--connect <ticket>` dials instead of waiting; `--preset n0` swaps the
hermetic loopback configuration for n0's relays, for a cross-network run.
Shutdown is stdin EOF or the line `quit` — **not** SIGINT: peerbot shares the
app's crate hub, whose one feature-unified graph cannot add `tokio/signal`.

## Two things this is not

**It is not a `bazel test`.** It has to invoke bazel (the dev tool builds and
installs the app), and inside a test action bazel is off `PATH`,
`BUILD_WORKSPACE_DIRECTORY` is empty, and `$TEST_TMPDIR` makes an inner bazel
fork a second server with its own output base — which would rebuild iroh's 373
crates from scratch on every run. So `bazel test //...` does not run it and
nothing forces it to keep working.

**It is not a browser test.** Everything here is native, and the one peer that
*cannot* be hermetic without extra machinery is the one this driver never runs:
a browser has no UDP socket, so a web peer reaches the world only through a
relay. That is `playwright/web.spec.js`, which starts its own.

## It is hermetic

Both peers bind `presets::Minimal` — no relay, no DNS, no pkarr — and pair by a
full ticket, which carries addresses, so iroh dials loopback directly and no
packet leaves the host. Two peers on one machine genuinely connect this way;
nothing is being stubbed.

**Hermeticity has to be a bridge parameter, not a flag**, which is why the app
takes a `Preset` alongside its relay. A `Node::open` that hardcodes its preset
cannot be asked for anything else by a dart-define or a build flag, and a
configurable constructor behind `#[cfg(test)]` is unreachable from the cdylib
the app loads.

Step 0 of the drive asserts it: a run whose transport chip reads anything but
`no relay — direct only` fails. A build that quietly acquired a third party
cannot pass.
