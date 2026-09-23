# Wasm platform facts

The README's "Choosing a web platform" is the decision procedure. This page
holds the platform facts behind it.

## What each target supports

With a stock std:

|  | `wasm32` | `wasm32` + `bindgen` | `wasm32_wasi` | `wasm32_threads` |
|---|---|---|---|---|
| triple | `wasm32-unknown-unknown` | same | `wasm32-wasip1` | `wasm32-unknown-unknown` |
| Rust channel | stable | stable | stable | nightly + local std |
| `SystemTime::now` | build error | build error | works | build error |
| `Instant::now` | aborts at run time | via `js-sys` | works | aborts at run time |
| `getrandom` crate | compile error | via `wasm_js` | works | compile error |
| `println!` | build error | build error | to the console | build error |
| `web-sys` / `js-sys` | — | yes | panics per call | — |
| real threads | no | no | no | yes |

`//bazel:wasm32_custom` ([custom_std.md](custom_std.md)) turns the clock,
`println!` and abort cells into working implementations without changing
target. It does not fix the `getrandom` row: that crate never asks std, so it
needs `ext/getrandom` on either `wasm32` column.

The build errors come from frustrate's std-facility check, not rustc.
`Instant::now` is deliberately left out of that check, so nothing at build time
tells you a graph needed a clock.

## Browser APIs and wasi do not compose

"A clock and a WebSocket" is an ordinary thing to want, and no single target
gives both.

The first error you meet, `` `wasm_js` backend can be enabled only for OS-less
WASM targets! ``, is getrandom refusing its browser backend on a wasi target.
On wasip1 its `wasi` backend is the right one and frustrate's host serves it.

wasm-bindgen itself does not refuse. Its externs are gated
`not(target_os = "wasi")`, and on wasi every generated body is
`panic!("function not implemented…")`. A wasip1 module that carries web-sys
compiles, and each call panics when reached. No build will stop you.

If you need both: move the browser I/O to Dart and take wasi, or take the
wasm-bindgen target and get your clock and entropy from JS too, which is what
that ecosystem expects.

## Never hand-stub a wasm-bindgen import

A module whose wasm-bindgen imports are satisfied by hand-written stubs
instantiates and returns wrong answers without any error. A stubbed
`getRandomValues` gives a CSPRNG that returns zeros. Use the `bindgen`
attribute, which runs the real post-pass.

## What wasip1 does not give you

No sockets: `socket2` does not support wasip1, so networking needs the
wasm-bindgen target, or Dart.

No component model. frustrate uses preview1 on purpose: wasip2 and wasip3
produce components, which `wasm-component-ld` refuses to build over frustrate's
raw imports and which no browser instantiates.

## Adding a wasm triple to an app's MODULE.bazel

Two lists, and nothing checks that they agree:

```python
rust.toolchain(extra_target_triples = ["wasm32-wasip1", ...])          # replaces the default
crate.from_cargo(supported_platform_triples = ["wasm32-wasip1", ...])  # replaces the default
```

Both replace the default list rather than extending it, and they fail
differently. A triple missing from `extra_target_triples` fails at analysis
with "No matching toolchains found", which names neither wasm nor the list. A
triple missing from `supported_platform_triples` does not fail: the crate
resolves to `//:incompatible` and shows up as a `target_compatible_with` skip,
far from the cause. A per-platform `build_test` on every artifact the app ships
catches both.

## Module size

Build with `-c opt` before reaching for wasm-opt. Bazel's default `fastbuild`
barely optimizes, and a `fastbuild` module run through wasm-opt is still far
larger than an `-c opt` module that never saw it.
