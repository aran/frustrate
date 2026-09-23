# Tin Can — a peer-to-peer chat app on frustrate

Two people paste each other's ticket and talk directly, phone to laptop, over
[iroh](https://github.com/n0-computer/iroh). It runs on macOS, iOS, Android,
Linux and in a browser, from one `lib/main.dart` and one bridged Rust surface.

This is frustrate's demanding example: a real dependency with a real graph (379
lock packages, one C-heavy crate), a real async runtime frustrate does not
provide, real platform permissions, and a real network. `e2e/flutter_demo` is
the feature gallery — every bridged shape, nothing else going on. This one is
the opposite: one narrow feature, and everything that comes with shipping it.

## What to take away

Five things this example exists to show, each of which costs real time to
re-derive.

**1. An I/O-owning object wants `#[bridge(actor)]`.** It is the only
model genuinely off the UI thread on every platform — a dedicated OS thread
natively, a Worker-hosted wasm instance on web — and the only one whose object
is a plain `Box<T>` needing neither `Send` nor `Sync`, so it can own a
`tokio::Runtime` and a live `Endpoint` outright. A plain `#[bridge] fn` doing
`RT.block_on(…)` parks a pool thread per call and, on single-threaded web, runs
*inline on the main thread* — where `thread::park` is not a trap but a no-op
(`Parker::park` is `{}` there, and nothing can unpark it), so the call spins the
page's only thread at 100% CPU forever, with no error and no way to recover.
That silent form is the worse one, and it is why `tools/check_no_park.dart`
exists. A
`#[bridge] async fn` is not automatically better: frustrate's cooperative
executor has no reactor of its own, so an unregistered tokio leaf panics at
construction ("there is no reactor running"). On native you can lend it one —
`frustrate::runtime::register` — and then
an `async fn` does `.await` tokio leaves without parking a thread; but that is
native-only, and this app is one binary across five platforms. The actor still
wins here for the reason above: it *owns* the runtime, on every platform.

**2. An actor is serial, so a method that waits wedges the instance.** A
5-second dial would make `ticket()` and `send()` wait 5 seconds. Either keep
actor methods short — inspect state, poke a channel, let the outcome arrive as
an event — or return `Deferred<T>`, which runs the body's synchronous prefix on
the executor and completes the call later, releasing the instance across the
await. `Node::connect` takes the first route and returns `()`; its three
outcomes arrive as `Dialing` / `Connected` / `Failed`.

**3. Events are where typed data lives.** `PeerEvent` is a `#[bridge]` enum, so
Dart pattern-matches a sealed hierarchy and every failure carries a real
`FailureKind`. Reach for that before reaching for error strings.

**4. A `StreamSink<T>` constructor parameter is how you guarantee nothing is
missed.** Dart constructs the `StreamController`, listens, and hands it over, so
no event can be produced before there is a listener. The controller is
single-use and the generated code owns its `onCancel`/`onPause`/`onResume` — the
app installs none of its own and never makes it broadcast.

**5. Release builds lose network permissions that debug builds have.** The
conventional `flutter create` scaffolding grants network access in debug only:
`INTERNET` lands in `android/app/src/debug/AndroidManifest.xml`, and
`macos/Runner/Release.entitlements` gets `app-sandbox` and nothing else. Nothing
warns — the sandbox denies the socket and a peer-to-peer app simply looks like
it has no peers. Both files here carry the permissions in the *main* manifest
and in *both* entitlements, with a comment saying why.

## Running it

```sh
cd e2e/iroh_demo
bazel run @rules_flutter//tools/dev_tool:flutter_bazel -- \
    run --target //:app --device macos
```

Two peers, driven and asserted with no human:

```sh
bazel run //tools:two_peers      # see tools/README.md
```

Other platforms:

```sh
bazel build //:app_ios                     # simulator; //:app_ios_device needs a profile
bazel build //:app_web                     # served bundle
bazel test  //:linux_app_build_test        # cross-compiled from macOS, debug-only
bazel build --config=android //:app_android  # needs ANDROID_HOME/ANDROID_NDK_HOME in .bazelrc.user
```

The Linux line is the interesting one: `flutter build linux` on a macOS host
refuses in under a second, and this cross-builds a GTK bundle with a real ELF
x86-64 `libiroh_rust.so` in it. The command is ordinary; three things behind it
are not, and each cost real time to find.

- **It is a `bazel test`, not a `bazel build`, and it goes through a wrapper.**
  `build_test` builds its targets in whatever configuration the *test* is in —
  macOS here — where `//:app_linux` is an incompatible skip, so the test passes
  having built nothing. `//bazel:linux.bzl`'s `linux_x86_64_build` puts the
  requirement in the graph with an outgoing transition instead. A plain
  `build_test(targets = [":app_linux"])` is silently green.
- **The bundle is always debug, by construction.** Flutter's `gen_snapshot`
  only emits native code for the host OS, so an AOT Linux build from macOS dies
  in analysis with "AOT cross-compilation between desktop platforms is not
  supported". Debug is JIT — a kernel snapshot the engine interprets, which is
  host-independent. A release Linux bundle has to be built on Linux.
- **The C toolchain declares targets, not hosts.** `MODULE.bazel` uses `@llvm`
  and names only `wasm32` and `linux-x86_64` as targets. Naming a macOS target
  would register a darwin `cc_toolchain` that outranks Xcode's for every host
  build, and rules_apple would then link against `@llvm`'s curated SDK subset
  instead of Xcode's. The Linux capability costs ~40 lines in one file and no
  sysroot at all — `@llvm` cross-compiles glibc and libc++ from source.

`e2e/flutter_demo` deliberately does **not** carry that wiring: its `app_linux`
builds on a Linux host and is not cross-buildable from macOS. This demo is
where the Linux cross path is proven.

**Nothing here has run on a device.** Every platform above is covered by a
`build_test`, which proves the bundle assembles with the Rust artifact inside it
at the path the loader will look for — not that a single bridge call crosses.
No iPhone, Android phone, Linux machine, simulator or emulator has been booted;
every functional claim in this repo is a macOS + Chromium claim. Closing that
needs hardware and, for `//:app_ios_device`, a `//device:profile` package
(gitignored; `device.example/` is the template).

**Who the app talks to is a build-time choice**, and it is visible in the
running app's transport chip. `PRESET` defaults to `minimal` — no relays, no
DNS, no pkarr, so the app contacts nobody and pairs by full ticket. `PRESET=n0`
opts into n0's public relays and address lookup; `RELAY_URL` points at a relay
of your own. `//:app_web` is the one target that ships `PRESET=n0`; every other
target, and every automated run, contacts nobody.

## How it is tested

Six layers, cheapest first. Everything except the last runs offline.

| layer | what it proves |
|---|---|
| `//bridge:iroh_rust_test` | frame codec round-trip; two endpoints connect and exchange a message in one process on `127.0.0.1` |
| `//bridge:dependency_logging_test` | iroh's own `log` records arriving on a Dart stream — the wiring in `.bazelrc` that puts this app's crate hub and frustrate's runtime on one `log` |
| `//peerbot:peerbot_test` | the headless peer's escaping, ticket round-trip, and that its ALPN still matches `node.rs`'s |
| `//relay:relay_server_test` | a relay spawned in-process on an ephemeral port actually relays a datagram |
| `//tools:two_peers` | the real app, driven through its UI, exchanging a message with a headless peer — with peerbot's stdout as an independent witness |
| `playwright/web.spec.js` | a browser peer and a native peer exchanging a message through a relay this suite starts |

`bazel test //...` runs the first four. The last two are `bazel run` and
`npx playwright test`: both must invoke bazel themselves, and a test action
holds the workspace lock that inner build needs.

**What no test here asserts.** The path badge flipping to `direct` mid-session:
every automated run pairs on one host, where the badge starts `direct` and never
changes, so the transition is only observable across two real networks. Also the
first Android device install, and anything involving a deployed relay
(`relay/README.md` is explicit that nothing in it has ever been deployed).

## Layout

```
lib/main.dart      the whole UI, byte-identical on every platform
bridge/src/
  api.rs           every #[bridge] item, and nothing else — the only file codegen parses
  node.rs          the iroh implementation                              (native)
  stub.rs          the same surface over a real wasm iroh peer          (web)
  proto.rs         frame codec, symlinked into peerbot so it cannot drift
peerbot/           headless Rust peer: prints a ticket, echoes messages
relay/             a relay of our own — a test fixture, a dev server, deploy scripts
tools/             the driven two-peer test
playwright/        the browser test
android/           the Android runner (manifest, launcher resources)
```

`api.rs` is deliberately thin — types, the actor declaration, one-line
delegations to `imp`, which is `node.rs` natively and `stub.rs` on wasm.
Everything hard lives behind that alias, where frustrate never sees it.

## Known rough edges, kept on purpose

The app is what a competent developer would naively write, so where the naive
thing has a wrong edge, the edge is left visible and named:

- **The self-dial guard is string equality.** `NodeController.connect` compares
  the pasted ticket against the string the app rendered and refuses that one.
  A *re-encoded* ticket for the same endpoint still gets through — an exact
  paste is the case a human hits, and the app does not decode to check.
- **`Phase` has no `offline`, so silence is reported rather than diagnosed.**
  After 10 s with no `Online` the status line stops showing progress and says
  what it does not know — "no relay confirmation after 10 s — a peer holding
  your ticket may not be able to reach you". Reported, never acted on: nothing
  is cancelled, there is no timeout, and no state claims the peer is gone.
- **`NodeController.dispose()` returns a `Future` the framework discards.**
  `ChangeNotifier.dispose` is `void` — a top type, so the override is legal and
  is never awaited. Everything after the first `await` is best-effort. Two
  things make that survivable rather than merely ignored: teardown starts
  synchronously, so on a normal quit the actor's `Drop` runs while the event
  loop is still turning, and `super.dispose()` is called first, so a late
  `notifyListeners` cannot fire into a dead tree. The honest version needs an
  awaited `shutdown()` on a path the framework actually awaits, and
  `State.dispose` is not one.
- **Two of `stub.rs`'s guarantees are weaker than the shared signature says.**
  On web `open()` returns before the bind resolves, so its `Result` can only
  ever be `Ok` and a bind failure arrives later as `PeerEvent::Failed`; and
  `dispose()` returns before the close completes, because a wasm instance
  cannot block in `Drop`. The Dart signature is identical either way, so
  `try { await Node.open(…) }` compiles, looks right, and never fires on web.
  Both are stated in `api.rs`'s rustdoc, which is what a Dart caller reads on
  both platforms — nothing in the type system carries it.
