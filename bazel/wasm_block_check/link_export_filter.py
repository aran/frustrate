#!/usr/bin/env python3
"""The check configuration's linker: `rust-lld` with the export list cut back
to the check roots.

Reached only through `-Clinker=`, in the `--cfg frustrate_block_check`
configuration: `tools/check_block.dart` sets it for the cargo flow and
`frustrate_block_check`'s transition for the Bazel one. Nothing that ships is
linked through it.

The check artifact's premise is **exports == roots** — the module's only entry
points are one `frustrate_check_block_*` root per claimed member, so
`--gc-sections` deletes what the claims cannot reach and a surviving
`memory.atomic.wait` means a claimed body reaches a wait. `--cfg` can suppress
frustrate's own `#[no_mangle]`s, but not a `#[wasm_bindgen]` export inside a
dependency rlib. rustc passes wasm-ld one explicit `--export` argument per
exported symbol, those included, so dropping the argument here removes the
export and lets the GC delete the body.

# What this can and cannot break

It only ever *removes* `--export` arguments, so an export it fails to remove
survives into the module — where //bazel/wasm_block_check refuses it by name
("every function export must be a check root"). This can fail to help; it
cannot make a scan mean less than it says.

Dropping a *root* is the direction that would be dangerous, and the fail-closed
check in `main` is what guards it: a check module is linked only when some claim
wants a root, so zero roots kept means rustc no longer emits one `--export` per
symbol, and the link stops rather than producing a module scanned over nothing.

# The arguments

Both spellings are handled: `--export <sym>` is rustc's form for a collected
symbol, `--export=<sym>` its form for `__heap_base`/`__data_end` and what
`-Clink-arg` produces. Nothing else is touched — `--export-if-defined`,
`--export-all` and `--export-table` pass through, and if one of them ever
creates an export the scanner names it.

`@response-file` arguments are expanded before filtering, and re-spilled after.
No flow here spills today (the largest export list measured is ~151 KB against a
1 MB `kern.argmax`), but a platform with a smaller limit would, and a filter that
walked argv without expanding would pass every export through in silence.
"""

import os
import shlex
import shutil
import subprocess
import sys
import tempfile

#: Codegen's check roots. Kept, always — they are the module's reason to exist.
#: Spelled the same as `ROOT_PREFIX` in main.rs.
ROOT_PREFIX = "frustrate_check_block_"

#: Exports that are not codegen's and are kept anyway.
#:
#: `__heap_base` and `__data_end` are rustc's own, on every wasm link.
#: `__stack_pointer`, `__wasm_init_tls`, `__tls_base`, `__tls_size` and
#: `__tls_align` come from the +atomics toolchain's link arguments under Bazel.
#: Only `__wasm_init_tls` is a *function* export, and the scanner exempts
#: exactly it (`LINKER_EXPORTS` in main.rs); the other four are globals, which
#: its export walk skips. Dropping them would be a deviation from the
#: configuration the toolchain asked for, bought for nothing.
KEEP_EXPORTS = frozenset([
    "__heap_base",
    "__data_end",
    "__stack_pointer",
    "__wasm_init_tls",
    "__tls_base",
    "__tls_size",
    "__tls_align",
])

#: The linker actually run. Resolved on PATH rather than by a written-down path:
#: rustc prepends its own toolchain's `lib/rustlib/<host>/bin` to the child's
#: PATH in both flows (measured), so this finds the toolchain's `rust-lld` — the
#: rustup one under cargo, the Bazel-staged one under Bazel, with no table here
#: that could name the wrong one.
LINKER = "rust-lld"

#: How deep `@file` nesting may go before this calls it a cycle.
MAX_RESPONSE_DEPTH = 8

_PROG = "wasm_block_check link filter"


class _Fail(Exception):
    """A named refusal. Printed and turned into a non-zero exit."""


def expand_response_files(args, read_file, depth=0):
    """`args` with every `@file` replaced by the arguments it holds.

    `read_file` is injected so the self-test can run without touching the disk:
    it runs inside every sandboxed link action, and a self-test that reads the
    environment turns a logic regression into a sandbox flake.
    """
    if depth > MAX_RESPONSE_DEPTH:
        raise _Fail(
            "@response-file nesting is more than %d deep, which is a cycle."
            % MAX_RESPONSE_DEPTH
        )
    out = []
    for a in args:
        if not a.startswith("@") or len(a) == 1:
            out.append(a)
            continue
        name = a[1:]
        try:
            text = read_file(name)
        except OSError as e:
            raise _Fail("cannot read the response file %s: %s" % (name, e))
        out.extend(
            expand_response_files(shlex.split(text), read_file, depth + 1))
    return out


def filter_exports(args):
    """`args` with every `--export` of a symbol this module has no use for gone.

    Returns `(kept_args, kept_roots, dropped)`. Order is otherwise preserved
    exactly: `-flavor wasm` stays first, and the object files, `-L` paths,
    `--gc-sections` and the memory arguments the toolchain set are untouched.
    """
    out = []
    kept_roots = []
    dropped = 0
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--export":
            if i + 1 >= len(args):
                raise _Fail("`--export` is the last argument, with no symbol "
                            "after it. This is not a command line rustc emits.")
            sym = args[i + 1]
            if _keep(sym):
                out.append(a)
                out.append(sym)
                if sym.startswith(ROOT_PREFIX):
                    kept_roots.append(sym)
            else:
                dropped += 1
            i += 2
            continue
        if a.startswith("--export="):
            sym = a[len("--export="):]
            if _keep(sym):
                out.append(a)
                if sym.startswith(ROOT_PREFIX):
                    kept_roots.append(sym)
            else:
                dropped += 1
            i += 1
            continue
        out.append(a)
        i += 1
    return out, kept_roots, dropped


def _keep(sym):
    return sym.startswith(ROOT_PREFIX) or sym in KEEP_EXPORTS


def check_flavor(args):
    """Refuse a link that is not the wasm one.

    Removing `--export` arguments is right for the check module and wrong
    everywhere else, so a wrapper that found itself on a native link should stop
    rather than quietly change what that link produces. `-flavor` absent is not
    an error: it only means rustc stopped spelling the invocation that way.
    """
    for i, a in enumerate(args):
        if a == "-flavor" and i + 1 < len(args) and args[i + 1] != "wasm":
            raise _Fail(
                "this is a `-flavor %s` link, not a wasm one. Nothing but the "
                "block check's throwaway module should be linked through this."
                % args[i + 1])


def _repair_darwin_rpath(linker):
    """Make `rust-lld` loadable on macOS, where its own rpath does not.

    `rust-lld` is dynamically linked against `libLLVM.dylib` and its only
    usable `LC_RPATH` is `@loader_path/../lib` — which resolves to the sysroot
    lib directory, while the toolchain ships the dylib at its *root* `lib/`.
    Running it therefore dies with `Library not loaded: @rpath/libLLVM.dylib`
    before any argument is read.

    Nothing else notices, which is why this went unseen: rustc links
    `wasm32-unknown-unknown` with `wasm-component-ld`, a self-contained binary
    that has no such problem. Only this filter reaches for `rust-lld` directly,
    so only the block check breaks — silently, as "the check artifact did not
    build".

    `DYLD_LIBRARY_PATH` is what dyld consults for the leaf name;
    `DYLD_FALLBACK_LIBRARY_PATH` is not consulted for an `@rpath` reference and
    does not help. The path is derived from the resolved linker rather than
    written down, so it follows whichever toolchain rustc put on PATH:
    `<toolchain>/lib/rustlib/<host>/bin/rust-lld` -> `<toolchain>/lib`.

    A no-op off macOS, and a no-op if the dylib is not where this expects —
    better to let the linker report its own failure than to invent an
    environment on a platform whose layout this has not been checked against.
    """
    if sys.platform != "darwin":
        return
    lib = os.path.abspath(
        os.path.join(os.path.dirname(os.path.realpath(linker)), "..", "..", ".."))
    if not os.path.exists(os.path.join(lib, "libLLVM.dylib")):
        return
    existing = os.environ.get("DYLD_LIBRARY_PATH")
    os.environ["DYLD_LIBRARY_PATH"] = lib + (os.pathsep + existing if existing else "")


def quote_response(args):
    """`args` as the body of a response file lld will tokenize back to `args`.

    One argument per line, POSIX-quoted. LLVM's non-Windows response-file
    tokenizer takes the same quoting `shlex` writes and `shlex.split` reads, so
    the round trip is exact; the self-test pins it.
    """
    return "".join(shlex.quote(a) + "\n" for a in args)


def _self_test():
    """Run on every invocation, the same way the scanner self-tests its decoder.

    Pure: no PATH lookup, no exec, no file of any kind. It costs well under a
    millisecond, and it means every `bazel test //...` and every
    `dart run tools/check_block.dart` exercises this file's logic without a
    target of its own — which matters, because on a graph the filter is a no-op
    for, a broken filter and a working one look identical from outside.
    """

    def eq(got, want, what):
        if got != want:
            raise _Fail("self-test failed (%s): %r, wanted %r" %
                        (what, got, want))

    # Both spellings, the allowlist, and everything else passed through in order.
    args = [
        "-flavor", "wasm",
        "--export", "frustrate_check_block_0_add",
        "--export", "some_wasm_bindgen_export",
        "--export=__heap_base",
        "--export=__data_end",
        "--export=__wbindgen_malloc",
        "--gc-sections",
        "-o", "out.wasm",
        "--export=__wasm_init_tls",
        "--export=__tls_base",
    ]
    kept, roots, dropped = filter_exports(args)
    eq(kept, [
        "-flavor", "wasm",
        "--export", "frustrate_check_block_0_add",
        "--export=__heap_base",
        "--export=__data_end",
        "--gc-sections",
        "-o", "out.wasm",
        "--export=__wasm_init_tls",
        "--export=__tls_base",
    ], "filtering")
    eq(roots, ["frustrate_check_block_0_add"], "kept roots")
    eq(dropped, 2, "dropped count")

    # Flags that merely start the same way are not exports and are not touched.
    kept, roots, _ = filter_exports(
        ["--export-if-defined", "x", "--export-all", "--export-table"])
    eq(kept, ["--export-if-defined", "x", "--export-all", "--export-table"],
       "export-lookalike flags")
    eq(roots, [], "no roots among lookalikes")

    # Fail-closed input: a link with no root left is reported as zero roots, and
    # `main` refuses on that.
    _, roots, _ = filter_exports(["--export", "unrelated"])
    eq(roots, [], "fail-closed detection")

    # `@file` expansion happens before filtering, so a root inside one is kept
    # and a stray inside one is dropped.
    files = {
        "args.rsp": "--export frustrate_check_block_1_b\n--export stray\n@more.rsp\n",
        "more.rsp": "'--export=__heap_base'\n",
    }
    expanded = expand_response_files(["-flavor", "wasm", "@args.rsp"],
                                     lambda n: files[n])
    eq(expanded, [
        "-flavor", "wasm",
        "--export", "frustrate_check_block_1_b",
        "--export", "stray",
        "--export=__heap_base",
    ], "response-file expansion")
    kept, roots, dropped = filter_exports(expanded)
    eq(roots, ["frustrate_check_block_1_b"], "roots from a response file")
    eq(dropped, 1, "strays from a response file")

    # And what this writes back, lld tokenizes to what it was given — including
    # the paths with a space in them that make a naive writer produce two
    # arguments.
    awkward = ["-o", "/a b/out.wasm", "--export", "it's", '--say="hi"', "@x"]
    eq(shlex.split(quote_response(awkward)), awkward, "response-file quoting")


def main(argv):
    _self_test()
    args = expand_response_files(argv[1:], _read)
    check_flavor(args)
    kept, roots, dropped = filter_exports(args)
    if not roots:
        raise _Fail(
            "this link exports no `%s*` symbol, so there is nothing to check.\n"
            "\n"
            "A check module is linked only when some `#[bridge(no_block)]` "
            "claim wants artifact\nevidence, and codegen emits one root per "
            "such claim, so every link that reaches this\nfilter carries at "
            "least one. Zero means the mechanism this rests on changed — rustc "
            "no\nlonger passing one `--export` argument per exported symbol is "
            "the way that happens.\nStopping, because the alternative is a "
            "module scanned over none of its claims that\nstill reports "
            "clean. Re-measure with `rustc --print link-args`." % ROOT_PREFIX)

    linker = shutil.which(LINKER)
    if linker is None:
        raise _Fail(
            "`%s` is not on PATH.\n\n"
            "rustc puts its own toolchain's lib/rustlib/<host>/bin on the "
            "linker's PATH, which is\nwhere this expects to find it. A PATH "
            "without it means the invocation did not come\nfrom rustc, or the "
            "toolchain is incomplete." % LINKER)

    _repair_darwin_rpath(linker)

    # Re-spill only if rustc spilled. The filtered line is strictly shorter than
    # what arrived, so a command line that fit still fits; one that did not fit
    # is handed back the same way it came. TMPDIR rather than beside the
    # original, which lives in an output tree a sandbox need not let anyone
    # write to.
    if any(a.startswith("@") and len(a) > 1 for a in argv[1:]):
        fd, path = tempfile.mkstemp(prefix="frustrate-link-", suffix=".rsp")
        try:
            with os.fdopen(fd, "w") as f:
                f.write(quote_response(kept))
            return subprocess.call([linker, "@" + path])
        finally:
            os.unlink(path)
    os.execv(linker, [linker] + kept)


def _read(name):
    with open(name, "r") as f:
        return f.read()


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv))
    except _Fail as err:
        sys.stderr.write("%s: %s\n" % (_PROG, err))
        sys.exit(2)
