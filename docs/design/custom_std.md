# A customized std for wasm

`wasm32-unknown-unknown` has no OS behind it, so std links stubs for the clock,
entropy, stdio, `available_parallelism` and `sleep`. They panic, discard output,
or (for `HashMap` seeding) quietly fall back to allocation addresses. A browser
can answer all of them. `toolchain/custom_std/` builds a std with those stubs
replaced by calls to the host, and `//bazel:wasm32_custom` links it.

This exists for Rust you do not control. Your own crate can call
`web_time::Instant`; a crate in your dependency graph calls
`std::time::Instant`, and std is the only place that can be fixed.

`atomics`, the threaded build ([threaded_wasm.md](threaded_wasm.md)), comes out
of the same builder.

## Using it

Build the facility set `//bazel:wasm32_custom` declares, then select the
platform:

```sh
bazel run //toolchain/custom_std:build -- --facilities=clock,random,stdio,thread
```

```python
frustrate_wasm_module(..., platform = "@frustrate//bazel:wasm32_custom")
```

The std is pinned to the nightly that built it (rustc refuses rlibs from another
compiler), so rebuild after the nightly pin changes.

**Read the first error, not the last.** If the std is missing or stale, the
generated toolchain package fails at load time with the command that fixes it.
Bazel then goes on to report that `//impl:rust_toolchain` "is not declared in
package 'impl'" and that no toolchain can be found. Those follow-on errors are
the same missing directory restated; they are not a separate defect.

## What your code can rely on

**The clock's resolution depends on the page.** Browsers coarsen
`performance.now()` unless the page is cross-origin isolated, so without
COOP/COEP a short interval between two `Instant`s can read as zero. The host
reports what the browser gives and never smooths it.

**`thread::sleep` throws on the main thread.** It blocks, including inside an
`async fn`, so it is only legal where blocking is: on a worker, such as an Actor
instance, it busy-waits and burns that core for the duration. On the main thread
it traps with an attributed error rather than freezing the page.

**`available_parallelism` reports the machine, not permission to use it.** On a
single-threaded build `thread::spawn` still fails.
