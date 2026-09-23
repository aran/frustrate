/// The JS-number arm of the wire codec: `i64` and opaque handles carried as
/// two 32-bit halves, for backends where Dart's `int` is a JavaScript number.
///
/// Why this exists at all: dart2js and DDC represent `int` as an IEEE-754
/// double, so `ByteData`'s 64-bit *integer* accessors (`setInt64`/`getInt64`,
/// `setUint64`/`getUint64`) throw `UnsupportedError` there. Four codec methods
/// use them, and between them they carry every string length, every collection
/// length, every `usize`/`isize`, and every opaque handle — so on a JS-number
/// backend, nothing crosses the bridge at all, not even `greet(String)`.
///
/// That is the only thing standing between frustrate and a **web development
/// loop**: `flutter run -d chrome` (DDC) offers hot reload and hot restart,
/// while `flutter run --wasm` offers neither (measured 2026-08-03 — the same
/// app shows `r`/`R` and a VM service under DDC, and only `h/d/c/q` under
/// `--wasm`, because flutter_tools gates the service protocol on
/// `!webUseWasm`). dart2wasm is still the web *platform*; this is the arm that
/// makes iterating on it bearable.
///
/// ## The contract, and its hard edge
///
/// A JS number represents every integer of magnitude ≤ 2^53 exactly, and this
/// encoding reconstructs those values **exactly** — two `Uint32` halves, no
/// rounding anywhere. Past that it does not truncate: it throws. Both
/// directions, and at the same boundary, so a value that can be written can
/// always be read back (see [jsMaxExact]).
///
/// **What this does NOT protect.** The guard is on the wire, not on the
/// language. On a JS-number backend the app's own arithmetic is 53-bit, and
/// its bitwise operations are *32*-bit — `1 << 62` is `0` and `-1 >>> 1` is
/// `2147483647`, both silently, both measured. A wrong value computed that way
/// is usually small, so it crosses this codec without tripping anything. The
/// exception guarantees the bridge never lies; it does not guarantee the
/// program is right. This is why the backend is development-only and why
/// [FrustrateWeb.init] refuses it in a release build.
///
/// ## Cost on the supported backends
///
/// [kJsNumbers] is a compile-time constant, so `if (kJsNumbers)` is a constant
/// condition and the VM and dart2wasm keep their single-instruction
/// `setInt64`/`getInt64` with the JS arm compiled out. That matters because
/// `writeLen` delegates here for every string, list and byte array.
///
/// Verified rather than assumed, 2026-08-03, two ways:
///
/// - **Structurally.** This library's range-error string is absent from the
///   AOT `native_bench` binary, while `writeUsize`'s "usize value must be
///   non-negative" is present — so unreachable literals are dropped there and
///   reachable ones are not, and the arm is in the first group. Symbol names
///   are stripped regardless (`setInt64` is absent too), which is why the
///   check is on a literal and carries its own control.
/// - **By measurement.** `native_bench` before and after: `echoI64s` at 1 MiB
///   is 131072 elements through `writeI64` and `readI64` each way, and moved
///   −2.7%. The run's noise floor is far wider than that — `poolNthPrime`,
///   which is pure Rust CPU and touches none of this, moved −8.2% — so the
///   useful reading is the ordering, not the number: the i64-saturated rows
///   sat at −4%..−1% while `echoF64s`, whose element loop has no `i64` in it
///   at all, drifted +8%..+11%. A live branch would have to invert that.
library;

import 'dart:typed_data';

/// True exactly on a backend where `int` is a JavaScript number: dart2js and
/// DDC, but **not** dart2wasm, which has real 64-bit integers.
///
/// Built from the two variables the compilers declare rather than from the
/// `identical(0, 0.0)` idiom, which works but is an observable of how a
/// compiler happens to represent numbers rather than anything specified.
/// `js_numbers_test` pins that the two agree.
///
/// Note this cannot be a conditional import: configurable imports resolve
/// `dart.library.*` only — `dart.tool.dart2wasm` is visible to
/// `fromEnvironment` but *not* to an `if (...)` in an import (verified by
/// compiling both ways; dart2wasm took the default arm). So the arm has to be
/// a constant branch inside one file, which also means a *signature* can never
/// differ between the two web backends.
const bool kJsNumbers =
    bool.fromEnvironment('dart.library.js_interop') &&
    !bool.fromEnvironment('dart.tool.dart2wasm');

/// 2^53 — the largest magnitude an integer can have and still be exact as a
/// JS number. Inclusive on both sides, and identical for reads and writes.
const int jsMaxExact = 9007199254740992;

const int _2p32 = 4294967296;

/// High half of [jsMaxExact]. A high half beyond this is out of range on its
/// own; at exactly this value the low half must be zero.
const int _maxHi = 2097152; // 2^21

Never _rangeError(String what, String detail) {
  throw UnsupportedError(
    'frustrate codec: this $what does not fit the 2^53 exact-integer range '
    'of a JavaScript-number backend ($detail). dart2js/DDC is a '
    'development-only target — build with dart2wasm (`--wasm`) or run on '
    'the Dart VM for the full 64-bit range.',
  );
}

/// Write [v] as a little-endian two's-complement `i64` at [at].
///
/// Decomposed with `%` and `~/`, which need no argument about floating point
/// to read: `v % _2p32` is non-negative for a positive divisor, and `v - lo`
/// is a multiple of 2^32, so the halves fall out as integers.
///
/// `(v / _2p32).floor()` would be equally correct and was tried first —
/// division by a power of two is an exponent shift, so it is exact for every
/// value this function accepts. It is not used because being *told* that is
/// weaker than not having to know it.
void jsWriteI64(ByteData view, int at, int v) {
  if (v > jsMaxExact || v < -jsMaxExact) _rangeError('i64 value', '$v');
  final lo = v % _2p32; // Dart's % is non-negative for a positive divisor
  final hi = (v - lo) ~/ _2p32;
  view.setUint32(at, lo, Endian.little);
  view.setInt32(at + 4, hi, Endian.little);
}

/// Read a little-endian two's-complement `i64` from [at].
///
/// The range check is on the halves, deliberately, and runs *before* the
/// reconstruction. Checking the reconstructed value instead would have a hole
/// at exactly 2^53 + 1: it is not representable, so it rounds down to 2^53 on
/// the way in and then passes a `> 2^53` test.
int jsReadI64(ByteData view, int at) {
  final lo = view.getUint32(at, Endian.little);
  final hi = view.getInt32(at + 4, Endian.little);
  if (hi > _maxHi || hi < -_maxHi || (hi == _maxHi && lo != 0)) {
    _rangeError('i64 value', 'hi=$hi lo=$lo');
  }
  return hi * _2p32 + lo;
}

/// Write [v] as a little-endian `u64` opaque handle at [at].
void jsWriteHandle(ByteData view, int at, int v) {
  if (v < 0) {
    throw ArgumentError.value(v, 'v', 'handle must be non-negative');
  }
  if (v > jsMaxExact) _rangeError('handle', '$v');
  final lo = v % _2p32;
  final hi = (v - lo) ~/ _2p32;
  view.setUint32(at, lo, Endian.little);
  view.setUint32(at + 4, hi, Endian.little);
}

/// What a build running on this backend is allowed to do.
enum JsNumberVerdict {
  /// Real 64-bit integers: the VM and dart2wasm. Nothing to say.
  supported,

  /// A JS-number backend outside a release build — the development loop.
  /// Allowed, and warned about once, because the hazard is invisible.
  developmentOnly,

  /// A JS-number backend in a release build. Refused.
  refused,
}

/// The fence, as a decision rather than a docstring.
///
/// Separated from the constants that feed it ([kJsNumbers], `dart.vm.product`,
/// `frustrate.allowJsNumbers`) for one reason: those are compile-time facts of
/// whichever backend is compiling, so on any backend that can run a test, at
/// most one row of this table is reachable. Taking them as arguments is what
/// makes the whole table checkable — and the mistakes that matter here are
/// table mistakes, a dropped `!` or an inverted opt-in, not arithmetic.
JsNumberVerdict jsNumberVerdict({
  required bool jsNumbers,
  required bool releaseMode,
  required bool allowedInRelease,
}) {
  if (!jsNumbers) return JsNumberVerdict.supported;
  if (releaseMode && !allowedInRelease) return JsNumberVerdict.refused;
  return JsNumberVerdict.developmentOnly;
}

/// Why a release build on a JS-number backend is refused, and the two ways
/// out — the supported one first.
const String jsNumberRefusal =
    'frustrate: refusing to run a release build on a JavaScript-number '
    'backend (dart2js). Dart `int` is 53-bit there and bitwise operations are '
    '32-bit, so `i64` carries one eighth of its advertised range and the '
    "application's own integer arithmetic is silently wrong past 2^53 — "
    'frustrate can make the wire loud, but not your code. dart2js/DDC is '
    'supported for development only (it is what gives `flutter run -d chrome` '
    'hot reload); build the web release with `flutter build web --wasm`. If '
    'you want this anyway, compile with -Dfrustrate.allowJsNumbers=true and '
    'own the 53-bit contract.';

/// Said once per Dart heap on a development build. The hazard has no other
/// cue: nothing throws, the app just computes different numbers than it will
/// in production.
const String jsNumberWarning =
    'frustrate: running on a JavaScript-number backend (dart2js/DDC). `int` '
    'is 53-bit here and bitwise operations are 32-bit — `1 << 62` is 0 — so '
    'results can differ from a dart2wasm or native build without any error. '
    'The bridge throws rather than truncating, but your own arithmetic will '
    'not. Verify numeric behaviour with --wasm or on the VM; this backend is '
    'for iteration.';

/// Read a little-endian `u64` opaque handle from [at].
///
/// In practice a wasm32 handle is a 32-bit linear-memory offset, so the high
/// half is zero and this never fires. It is checked anyway: the day a 64-bit
/// wasm target exists, a silently truncated pointer is the worst possible
/// failure and this is the only place to catch it.
int jsReadHandle(ByteData view, int at) {
  final lo = view.getUint32(at, Endian.little);
  final hi = view.getUint32(at + 4, Endian.little);
  if (hi > _maxHi || (hi == _maxHi && lo != 0)) {
    _rangeError('handle', 'hi=$hi lo=$lo');
  }
  return hi * _2p32 + lo;
}
