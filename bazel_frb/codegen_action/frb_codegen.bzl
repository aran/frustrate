"""One genrule shape, several environments — the FRB-codegen-as-an-action ladder.

The rungs differ only in `prelude`, so any difference in outcome is the
environment and not the recipe.

Why it copies into a scratch dir rather than running in the execroot: FRB's
codegen writes `bridge/src/frb_generated.rs`, rewrites `Cargo.lock` and
creates `target/`. Under `no-sandbox` the execroot's source entries are
symlinks *into the source tree*, so writing through them could edit the
checkout. Copying makes every rung write only into its own scratch.

Shell `$` is written `$$` throughout because genrule `cmd` is Bazel-expanded
first; `$@` is Bazel's (single) output file.
"""

_CRATE_SRCS = [
    "//bridge:src/lib.rs",
    "//bridge:src/api/mod.rs",
    "//bridge:src/api/simple.rs",
    "//bridge:Cargo.toml",
    "//:Cargo.toml",
    "//:Cargo.lock",
    "//:flutter_rust_bridge.yaml",
    # Added at rung 2's first failure: FRB resolves `dart_output` to a *pub
    # package*, walking up for a pubspec.yaml, and dies with
    # "Fail to detect dart package from dart_file_path=..." without one.
    "//:pubspec.yaml",
    "//:pubspec.lock",
]

# Bazel scrubs the environment, so the host toolchain has to be named
# explicitly. FRB_HOST_HOME arrives via `--action_env=FRB_HOST_HOME=$HOME`;
# `:?` makes a forgotten flag a loud failure rather than a mysterious one.
_HOST_ENV = """
HOME_REAL="$${FRB_HOST_HOME:?run with --action_env=FRB_HOST_HOME=$$HOME}"
export HOME="$$HOME_REAL"
export CARGO_HOME="$$HOME_REAL/.cargo"
# The developer's whole PATH, not a curated one. Rung 2's second failure was
# "Dart/Flutter toolchain not available": FRB's codegen shells out to `dart`
# as well as to cargo, so the action needs BOTH host toolchains. Handing it
# the whole host PATH verbatim is the honest spelling of that dependency.
export PATH="$${FRB_HOST_PATH:?run with --action_env=FRB_HOST_PATH=<your PATH>}"
"""

# The registry cache under ~/.cargo is an UNDECLARED input to rung 2: cargo
# only reaches the network for a crate it has not already unpacked there.
# Pointing CARGO_HOME at an empty scratch dir removes it, which is the
# question "would this action work on a clean machine / a remote executor?".
_EMPTY_CARGO_HOME = _HOST_ENV + """
export CARGO_HOME="$$work/.cargo_home_empty"
mkdir -p "$$CARGO_HOME"
"""

_OFFLINE_VENDORED = _HOST_ENV + """
export CARGO_NET_OFFLINE=true
export CARGO_HOME="$$work/.cargo_home_empty"
mkdir -p "$$CARGO_HOME"
mkdir -p "$$work/.cargo"
cp -R "$$EXECROOT/codegen_action/vendor" "$$work/vendor"
cp "$$EXECROOT/codegen_action/vendor_config.toml" "$$work/.cargo/config.toml"
"""

_PRELUDES = {
    "": "",
    "host_env": _HOST_ENV,
    "empty_cargo_home": _EMPTY_CARGO_HOME,
    "offline_vendored": _OFFLINE_VENDORED,
}

_BODY = """
# Bazel runs genrule cmds under `set -e`; turn it off so a failing rung
# reports its own text instead of vanishing into "(Exit 127)".
set +e
set -u
EXECROOT="$$PWD"
OUT="$$PWD/$(location %NAME%.log)"
OUTTAR="$$PWD/$(location %NAME%_generated.tar)"
work="$$PWD/frb_scratch_%NAME%"
rm -rf "$$work"
mkdir -p "$$work/bridge/src/api" "$$work/lib/src"
cp Cargo.toml Cargo.lock flutter_rust_bridge.yaml pubspec.yaml pubspec.lock "$$work/"
cp bridge/Cargo.toml "$$work/bridge/"
cp bridge/src/lib.rs "$$work/bridge/src/"
cp bridge/src/api/mod.rs bridge/src/api/simple.rs "$$work/bridge/src/api/"
%PRELUDE%
cd "$$work"
%INVOKE% > "$$OUT" 2>&1
echo "=== flutter_rust_bridge_codegen exit status: $$?" >> "$$OUT"
echo "=== generated rust: $$(ls -l bridge/src/frb_generated.rs 2>&1)" >> "$$OUT"
echo "=== generated dart: $$(ls lib/src 2>&1 | tr '\\n' ' ')" >> "$$OUT"
# Byte-level evidence, so "did the action produce the same thing the
# by-hand run produced?" is a diff of hashes rather than an assertion.
echo "=== sha256 of generated files ===" >> "$$OUT"
/usr/bin/shasum -a 256 bridge/src/frb_generated.rs lib/src/*.dart lib/src/api/*.dart 2>&1 \
  | sed "s|$$work/||" >> "$$OUT"
# The generated tree itself, so a rung's output can be diffed against the
# by-hand run byte for byte rather than compared by hash alone.
/usr/bin/tar -cf "$$OUTTAR" -T /dev/null
/usr/bin/tar -cf "$$OUTTAR" bridge/src/frb_generated.rs lib/src 2>/dev/null
exit 0
"""

_INVOKE_DEFAULT = "flutter_rust_bridge_codegen generate"
_INVOKE_HOST = "\"$$HOME_REAL/.cargo/bin/flutter_rust_bridge_codegen\" generate"

def frb_codegen_rung(name, prelude, extra_srcs = [], extra_tags = []):
    """One rung of the ladder.

    Captures the codegen's own stdout/stderr into the declared output and
    then exits 0, so a *failed* rung still leaves readable evidence instead
    of a Bazel action failure with truncated output. `grep 'Done!'` on the
    log is the pass signal.

    Args:
      name: target name; the log lands at `<name>.log`.
      prelude: key into the shared prelude table ("", "host_env",
        "offline_vendored").
      extra_srcs: additional declared inputs (rung 4's vendor tree).
      extra_tags: additional execution tags (`no-sandbox`, `block-network`).
    """
    if prelude not in _PRELUDES:
        fail("unknown prelude %r" % prelude)
    cmd = _BODY.replace("%NAME%", name)
    cmd = cmd.replace("%PRELUDE%", _PRELUDES[prelude])
    cmd = cmd.replace(
        "%INVOKE%",
        _INVOKE_DEFAULT if prelude == "" else _INVOKE_HOST,
    )
    native.genrule(
        name = name,
        srcs = _CRATE_SRCS + extra_srcs,
        outs = [
            name + ".log",
            name + "_generated.tar",
        ],
        cmd = cmd,
        tags = ["manual"] + extra_tags,
    )
