/// The frustrate wire codec, Dart side.
///
/// Mirrors runtime/rust/src/codec.rs exactly: little-endian fixed-width
/// scalars, i64 lengths, UTF-8 strings. Cross-language golden tests pin the
/// format; do not change one side without the other.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'js_numbers.dart';

// ---------------------------------------------------------- bulk typed data --

/// True on dart2wasm, where typed data is **not** a byte buffer.
///
/// dart2wasm backs each typed list with a `WasmArray` of that element type
/// (`F64List` holds a `WasmArray<WasmF64>`), and WasmGC arrays cannot be
/// reinterpreted — so a `Float64List` obtained from a *byte* buffer is
/// `SlowF64List`, which composes each double out of eight `WasmI8` reads. See
/// the SDK's `lib/_internal/wasm/lib/typed_data.dart`: `_I8ByteBuffer` does not
/// override `asFloat64List`, so it inherits `ByteBufferBase`'s
/// `SlowF64List._withMutability(...)`.
///
/// Consequence: on dart2wasm the "bulk" copy is per byte in *both* directions,
/// and a bulk decode would additionally hand the caller a list that stays slow
/// for every later element read. So dart2wasm keeps the element loop. That is
/// not a regression — it is exactly what shipped, and the web `Vec<f64>` row is
/// already ahead of the comparison bridge.
///
/// A compile-time constant, so the branch folds away on every backend.
const bool _kWasmTypedData = bool.fromEnvironment('dart.tool.dart2wasm');

/// Whether the bulk typed-list path is both **correct** and a **win** here,
/// given the two facts that decide it.
///
/// A function taking its inputs rather than reading the constants, for the same
/// reason `jsNumberVerdict` is one: those constants are compile-time facts of
/// whichever backend is compiling, so on any backend that can run a test at
/// most one row of this table is reachable. Taking them as arguments is what
/// makes the whole table checkable — and the mistake that matters here is a
/// table mistake, not arithmetic.
///
/// * [littleEndianHost] — the wire is little-endian and a typed-data view is
///   *host*-endian, so reinterpreting bytes as `f64`/`i32`/… is only the same
///   value on a little-endian host. There is no big-endian Dart target today;
///   the guard is here so that the day one exists it produces slow numbers
///   rather than wrong ones.
/// * [wasmTypedData] — see [_kWasmTypedData].
bool bulkTypedDataOk({
  required bool wasmTypedData,
  required bool littleEndianHost,
}) => !wasmTypedData && littleEndianHost;

/// [bulkTypedDataOk] for the backend this was compiled for.
final bool _kBulkTypedData = bulkTypedDataOk(
  wasmTypedData: _kWasmTypedData,
  littleEndianHost: Endian.host == Endian.little,
);

/// [bulkTypedDataOk], narrowed by the one thing an `i64` bulk copy needs that
/// the narrower widths do not: an `int` that really is 64 bits.
///
/// Reinterpreting eight wire bytes as an `int` yields what the element loop
/// would only where `int` *is* a 64-bit integer. On a JS-number backend it is
/// a 53-bit double, and the element path there goes through
/// `jsWriteI64`/`jsReadI64`, which **throw** past 2^53 rather than truncating
/// — the codec's loudness contract. A byte copy makes no such check, so taking
/// it there would trade a loud failure for a silent wrong value.
///
/// **No backend can reach this guard today, and it is written out anyway** —
/// it stands exactly where [bulkTypedDataOk]'s `littleEndianHost` stands, and
/// for the same reason. dart2js cannot construct an `Int64List` at all
/// (`Int64List(n)` and `Int64List.fromList` both throw `UnsupportedError`), so
/// [BinaryWriter.writeI64List]'s `is Int64List` test is already false there
/// and [BinaryReader.readI64List] already throws before either of its arms
/// runs. Leaving the exclusion to that would rest the codec's loudness on an
/// SDK's inability; the property the codec depends on has to be the codec's
/// own.
bool bulkI64Ok({required bool bulkTypedData, required bool jsNumbers}) =>
    bulkTypedData && !jsNumbers;

/// [bulkI64Ok] for the backend this was compiled for: the VM alone. dart2wasm
/// is excluded by [bulkTypedDataOk], dart2js by [kJsNumbers].
final bool _kBulkI64 = bulkI64Ok(
  bulkTypedData: _kBulkTypedData,
  jsNumbers: kJsNumbers,
);

/// Replace an external writer's backing store with one of at least [newCap]
/// bytes, **carrying the first [written] bytes across**, and return a view over
/// the new store starting at its offset 0.
///
/// The callback does the copy, not the writer, and that division is the whole
/// point: on native the two stores are FFI blocks, and the owner has to copy
/// the prefix and free the old block in one step. If the writer copied
/// afterwards it would be reading memory the owner had already released.
///
/// [newCap] is the writer's own growth policy applied to the request (never
/// less than `written`), so an owner implements allocation and copying and
/// nothing else.
typedef BinaryWriterGrow = Uint8List Function(int newCap, int written);

class BinaryWriter {
  Uint8List _buf;
  ByteData _view = ByteData(0);
  int _len = 0;

  /// Non-null when the backing store belongs to somebody else. See
  /// [BinaryWriter.external].
  final BinaryWriterGrow? _grow;

  /// Whether the store belongs to somebody else — see [takeBytes], the
  /// only place the distinction is observable.
  final bool _external;

  /// Set when the owner has taken its memory back. See [close].
  bool _closed = false;

  // ---------------------------------------- scatter-gather byte segments --
  /// Large byte payloads recorded by reference instead of copied into [_buf].
  /// Interleaved with [_buf] ranges in write order by [takePieces].
  final List<Uint8List> _pieces = [];

  /// How much of [_buf] has already been emitted into [_pieces].
  int _flushed = 0;

  /// Bytes living in segments rather than in [_buf].
  int _segBytes = 0;

  /// Above this, a byte payload is referenced rather than copied.
  ///
  /// **Fitted, not guessed — but not fitted against a penalty, because there
  /// isn't one.** Sweeping a single payload from 64 B to 16 KiB with
  /// segmentation forced off and on: a JS-backed payload wins everywhere and
  /// the win grows with size, and a Dart-heap payload is unharmed at every
  /// size and *ahead* from a couple of KiB up. Referencing skips the
  /// `setRange` into [_buf] — one full-size copy — whichever backing the
  /// payload has, so the only thing that scales with backing is whether the
  /// transport's read is a memcpy or the per-byte loop.
  ///
  /// So this bounds **fragmentation**, not per-payload cost: a request with
  /// many small byte fields would otherwise become many pieces, each carrying
  /// the transport's fixed per-piece work, and that shape is not in the sweep.
  /// 256 is the same crossover [WebRuntime._bulkReadThreshold] carries for the
  /// structurally identical decision on the read side.
  ///
  /// Re-derive rather than trust it if the codec or the SDK's conversion
  /// changes; the sweep is one payload through `echo_bytes_async` at both
  /// settings, and what it must show is the absence of a Dart-heap penalty.
  static const int segmentThreshold = 256;

  /// [sizeHint] is a capacity hint and **nothing else**: a LOWER BOUND on the
  /// bytes about to be written, never an assertion about them.
  ///
  /// It is always safe to be wrong in either direction. Too small and [_ensure]
  /// grows the buffer exactly as it would have from the default; too large and
  /// the tail of the buffer is simply never written ([takeBytes] is bounded by
  /// what was actually written, not by the capacity). No code path here or
  /// downstream may assume the buffer is big enough, and nothing asserts on the
  /// hint — so a generated encoder is free to sum only the parameters whose
  /// size it can compute for free and ignore the rest.
  ///
  /// That freedom is the point. `codegen`'s `size_hint_expr` sums the top-level
  /// parameters of a call whose widths are known before writing (scalars, the
  /// length of a `Uint8List`/`String`/`List<scalar>`) and contributes zero for
  /// everything else, so a 1 MiB `Vec<u8>` argument allocates its buffer once
  /// instead of doubling into it — no zero-filled 2 MiB scratch to hold 1 MiB.
  ///
  /// Values below the 64-byte default are rounded up to it, so a partial sum
  /// (or a negative one, which cannot happen but costs nothing to tolerate)
  /// never makes the writer worse than it was.
  BinaryWriter([int sizeHint = 64])
    // `Uint8List(n)` always owns its buffer at offset 0, which is what lets
    // `_view` index by `_len` alone. Do not switch this to a slice of a
    // pooled buffer without also biasing every `_view` offset.
    : _buf = Uint8List(sizeHint < 64 ? 64 : sizeHint),
      _grow = null,
      _external = false {
    _view = _buf.buffer.asByteData();
  }

  /// A writer over storage somebody else owns.
  ///
  /// The native transport passes the FFI block the call is about to hand Rust,
  /// so the generated encoder writes the request straight into its final
  /// destination and the staging memcpy that used to sit between them is gone.
  ///
  /// [buffer] must be a typed list at **offset 0 of its own buffer** — which is
  /// what both `Uint8List(n)` and `Pointer<Uint8>.asTypedList(n)` give you, and
  /// what lets `_view` index by `_len` alone (see the warning on the default
  /// constructor). A slice of a larger buffer would silently write at the wrong
  /// offsets, so it is rejected here rather than debugged later.
  ///
  /// The storage has a **lifetime**, which heap storage never had: it is valid
  /// until the owner calls [close]. That is why [close] exists at all — see it
  /// for what a writer that outlives its memory does.
  BinaryWriter.external(Uint8List buffer, BinaryWriterGrow grow)
    : _buf = buffer,
      _grow = grow,
      _external = true {
    if (buffer.offsetInBytes != 0) {
      throw ArgumentError.value(
        buffer,
        'buffer',
        'frustrate: an external writer needs a buffer at offset 0 of its own '
            'ByteBuffer; every write here indexes by length alone',
      );
    }
    _view = _buf.buffer.asByteData();
  }

  /// The capacity check, and NOTHING else.
  ///
  /// Every write calls this, and a `Vec<i64>` request calls it once per
  /// element, so its body is a hot loop's inner statement and it has to stay
  /// small enough for the VM to inline. The slow path lives in [_growBuffer]
  /// for that reason alone.
  ///
  /// [close] is what lets even the closed check live down there: it swaps the
  /// backing store for an empty one, so after closing every write of a
  /// non-zero number of bytes fails this test and lands in the slow path.
  void _ensure(int n) {
    if (_len + n <= _buf.length) return;
    _growBuffer(n);
  }

  /// Out of line from [_ensure] on purpose — see there.
  void _growBuffer(int n) {
    if (_closed) {
      throw StateError(
        'frustrate: this BinaryWriter is closed — its buffer belonged to a '
        'bridge call that has already returned, and the memory is gone. A '
        'codec hook (BytesCodec.toBytes, a custom encoder) must finish '
        'writing before it returns; it must not stash the writer.',
      );
    }
    var cap = _buf.length * 2;
    while (cap < _len + n) {
      cap *= 2;
    }
    final grow = _grow;
    if (grow == null) {
      final next = Uint8List(cap);
      next.setRange(0, _len, _buf);
      _buf = next;
    } else {
      // The owner allocates, copies the prefix and releases the old store, all
      // inside the callback — see [BinaryWriterGrow]. Nothing is copied here,
      // because by the time this returns the old store may be freed.
      _buf = grow(cap, _len);
      if (_buf.length < cap) {
        throw StateError(
          'frustrate: a BinaryWriter grow callback returned '
          '${_buf.length} bytes for a request of $cap',
        );
      }
    }
    _view = _buf.buffer.asByteData();
  }

  /// Bytes written so far. The transport uses this instead of [takeBytes] when
  /// the encoder wrote straight into its own buffer: there is nothing to take.
  int get length => _len;

  /// Release the writer from its storage.
  ///
  /// Called by whoever owns the buffer, in the `finally` that frees it. After
  /// this every write and every read is a named [StateError].
  ///
  /// On the heap that was never needed — a writer stashed past its call was
  /// harmless garbage. Backed by a transport block it is a write into freed
  /// native memory, and "never silent, never UB" is not negotiable, so the
  /// guard is unconditional rather than external-only: one contract, both
  /// platforms, no rule that only holds where somebody remembered it.
  ///
  /// Idempotent: the runtime closes in a `finally`, and a path that closed and
  /// then unwound must not replace a real failure with a confusing second one.
  ///
  /// The store is dropped as well as flagged, and that is what makes the guard
  /// free: with an empty buffer every subsequent write fails `_ensure`'s
  /// capacity test and reaches the flag, so the flag costs nothing on the path
  /// that matters. Dropping the store is also the stronger statement — a stale
  /// `_view` write can no longer reach released memory even if some future path
  /// forgets to call `_ensure`; it gets a `RangeError` instead.
  ///
  /// The one thing that stays silent is a write of *zero* bytes
  /// (`writeByteArray(v, 0)`), which passes the capacity test at length 0. It
  /// writes nothing by definition, so there is nothing for it to corrupt.
  void close() {
    _closed = true;
    _buf = _closedBuf;
    _view = _closedView;
  }

  static final Uint8List _closedBuf = Uint8List(0);
  static final ByteData _closedView = ByteData(0);

  /// The encoded bytes. The writer must not be used afterwards.
  ///
  /// On a heap writer this is a **view** of the writer's own store, which is
  /// what every caller inside this package wants and what it has always been.
  ///
  /// On a [BinaryWriter.external] writer it is a **copy**, and it has to be.
  /// External storage belongs to a bridge call: a grow frees it mid-encode and
  /// the call frame frees it on the way out, so a view handed back to user code
  /// would be a window onto memory that is about to disappear — a silent
  /// use-after-free, which is exactly the failure [close] exists to make loud.
  /// [close] cannot help here, because a view taken before the close survives
  /// it. Nothing in the transport takes this path (it uses [length]; the
  /// encoder wrote straight into the destination and there is nothing to take),
  /// so the copy is off every hot path and is only ever paid by a caller who
  /// asked for bytes the writer does not own.
  Uint8List takeBytes() {
    if (_closed) {
      throw StateError(
        'frustrate: this BinaryWriter is closed; takeBytes would read memory '
        'that has been freed.',
      );
    }
    if (_pieces.isNotEmpty) {
      // Assemble. Callers that still want one contiguous buffer get today's
      // bytes and today's cost; only the piecewise consumer benefits.
      final out = Uint8List(totalLength);
      var off = 0;
      for (final piece in takePieces()) {
        out.setRange(off, off + piece.length, piece);
        off += piece.length;
      }
      return out;
    }
    final written = Uint8List.sublistView(_buf, 0, _len);
    return _external ? Uint8List.fromList(written) : written;
  }

  void writeBool(bool v) => writeU8(v ? 1 : 0);

  void writeU8(int v) {
    _ensure(1);
    _buf[_len++] = v;
  }

  void writeI8(int v) {
    _ensure(1);
    _view.setInt8(_len, v);
    _len += 1;
  }

  void writeU16(int v) {
    _ensure(2);
    _view.setUint16(_len, v, Endian.little);
    _len += 2;
  }

  void writeI16(int v) {
    _ensure(2);
    _view.setInt16(_len, v, Endian.little);
    _len += 2;
  }

  void writeU32(int v) {
    _ensure(4);
    _view.setUint32(_len, v, Endian.little);
    _len += 4;
  }

  void writeI32(int v) {
    _ensure(4);
    _view.setInt32(_len, v, Endian.little);
    _len += 4;
  }

  /// The one place `i64` is written, and so — through [writeUsize],
  /// [writeIsize] and [writeLen] — the width every string, list and byte array
  /// puts its length through. [kJsNumbers] is const, so the branch folds away
  /// on the VM and dart2wasm (js_numbers.dart).
  void writeI64(int v) {
    _ensure(8);
    if (kJsNumbers) {
      jsWriteI64(_view, _len, v);
    } else {
      _view.setInt64(_len, v, Endian.little);
    }
    _len += 8;
  }

  /// u64 crosses as BigInt. Split into two 32-bit
  /// halves because ByteData has no BigInt accessor and values in
  /// [2^63, 2^64) don't fit a signed Dart `int` — so one codec implementation
  /// serves every supported backend (the VM and dart2wasm) with no
  /// per-platform branch: the charter's "no per-platform codec cliff".
  ///
  /// This split is not a JS-number affordance — it is forced by `u64` itself,
  /// and it predates the JS arm. It happens to run unmodified on a JS-number
  /// backend, which is a free side effect and not the reason it is written
  /// this way. [writeI64] and [writeHandle] are where that backend needs an
  /// actual branch (js_numbers.dart), and they carry an exactness contract
  /// this method does not need.
  void writeU64(BigInt v) {
    if (v.isNegative || v.bitLength > 64) {
      throw ArgumentError.value(v, 'v', 'u64 value must be in [0, 2^64)');
    }
    _ensure(8);
    _view.setUint32(_len, (v & _mask32).toInt(), Endian.little);
    _view.setUint32(_len + 4, (v >> 32).toInt(), Endian.little);
    _len += 8;
  }

  static final BigInt _mask32 = BigInt.from(0xFFFFFFFF);

  // i128/u128 share the u64 big-integer codec, extended to 16 bytes: one
  // big-integer story, not two. The value is written as
  // two u64 halves — low 64 bits then high 64 bits, each through [writeU64] —
  // which is byte-identical to Rust's `i128/u128::to_le_bytes()`. Both halves
  // are in [0, 2^64) by construction, so they carry no per-platform branch (as
  // writeU64 already documents) and reuse its exact accessor story.
  static final BigInt _mask64 = (BigInt.one << 64) - BigInt.one;
  static final BigInt _i128Min = -(BigInt.one << 127);
  static final BigInt _i128Max = (BigInt.one << 127) - BigInt.one;
  static final BigInt _u128Max = (BigInt.one << 128) - BigInt.one;

  void _writeU128Bits(BigInt u) {
    writeU64(u & _mask64); // low 64 bits (8 LE bytes)
    writeU64(u >> 64); // high 64 bits (8 LE bytes)
  }

  /// u128 crosses as BigInt on the u64 codec extended to 16 bytes. Loud at the
  /// boundary: throws for negative or ≥ 2^128 before crossing (mirrors writeU64).
  void writeU128(BigInt v) {
    if (v.isNegative || v > _u128Max) {
      throw ArgumentError.value(v, 'v', 'u128 value must be in [0, 2^128)');
    }
    _writeU128Bits(v);
  }

  /// i128 crosses as BigInt. Loud on out-of-range; encodes as 128-bit two's
  /// complement, then shares the same u64-halves path as u128.
  void writeI128(BigInt v) {
    if (v < _i128Min || v > _i128Max) {
      throw ArgumentError.value(
        v,
        'v',
        'i128 value must be in [-2^127, 2^127)',
      );
    }
    _writeU128Bits(v.isNegative ? (BigInt.one << 128) + v : v);
  }

  void writeF32(double v) {
    _ensure(4);
    _view.setFloat32(_len, v, Endian.little);
    _len += 4;
  }

  void writeF64(double v) {
    _ensure(8);
    _view.setFloat64(_len, v, Endian.little);
    _len += 8;
  }

  /// usize crosses as i64.
  void writeUsize(int v) {
    if (v < 0) {
      throw ArgumentError.value(v, 'v', 'usize value must be non-negative');
    }
    writeI64(v);
  }

  void writeIsize(int v) => writeI64(v);

  void writeLen(int v) => writeUsize(v);

  /// The browser's UTF-8 encoder, installed by the web runtime on dart2wasm.
  ///
  /// Null everywhere else, and [_kWasmTypedData] keeps the branch that reads it
  /// folded away — this library imports no `dart:js_interop` and must not.
  ///
  /// **It must be byte-identical to `utf8.encode`.** Rust's `read_string` is
  /// the peer and does not negotiate: a lone surrogate becomes U+FFFD, a
  /// leading U+FEFF is ordinary content and is encoded faithfully.
  /// `TextEncoder` satisfies both (WHATWG USVString conversion), and
  /// `string_encoder_hook_test` is what says so rather than this comment.
  ///
  /// It pays **because the result is referenced rather than staged**. Being
  /// JS-backed, it reaches the transport's `Uint8Array.set` as a memcpy when
  /// [_deferSegment] takes it; copied into [_buf] instead it would cost a
  /// JS<->wasm crossing per byte and lose more than the encode saves.
  static Uint8List Function(String)? stringEncodeHook;

  void writeString(String v) {
    if (_kWasmTypedData) {
      final hook = stringEncodeHook;
      // `v.length` is UTF-16 units — a lower bound on the UTF-8 byte count —
      // so at or above the threshold the encoded bytes always qualify for
      // [_deferSegment] on a heap writer, which is what makes the hook pay.
      // Below it Dart's encoder avoids the hook's fixed JS-call cost. A short
      // string can still encode to three times its length and stay on the Dart
      // path; that miss is bounded and deliberate.
      if (hook != null && v.length >= segmentThreshold) {
        final bytes = hook(v);
        writeLen(bytes.length);
        if (_deferSegment(bytes)) return;
        _ensure(bytes.length);
        _buf.setRange(_len, _len + bytes.length, bytes);
        _len += bytes.length;
        return;
      }
    }
    final bytes = utf8.encode(v);
    writeLen(bytes.length);
    _ensure(bytes.length);
    _buf.setRange(_len, _len + bytes.length, bytes);
    _len += bytes.length;
  }

  /// A Rust `char` crosses as its u32 Unicode scalar value. The Dart source is
  /// a `String`, which is UTF-16 and can hold zero, one, or many code points —
  /// so this is loud, never lossy: it throws unless [v] is *exactly one*
  /// Unicode scalar (a single code point that is not a lone surrogate). Empty,
  /// multi-character, and lone-surrogate strings all raise before crossing,
  /// rather than silently truncating to the first unit (the `char` contract).
  void writeChar(String v) {
    final it = v.runes.iterator;
    if (!it.moveNext()) {
      throw ArgumentError.value(
        v,
        'v',
        'char must be exactly one Unicode scalar value, got ""',
      );
    }
    final cp = it.current;
    if (it.moveNext()) {
      throw ArgumentError.value(
        v,
        'v',
        'char must be exactly one Unicode scalar value, got a multi-character string',
      );
    }
    if (cp >= 0xD800 && cp <= 0xDFFF) {
      throw ArgumentError.value(
        v,
        'v',
        'char cannot be a lone surrogate (U+${cp.toRadixString(16).toUpperCase()})',
      );
    }
    writeU32(cp);
  }

  /// A span crossing to a Rust `std::time::Duration`, which is **unsigned**.
  ///
  /// Dart's `Duration` is signed, so the two types are not the same set of
  /// values and the mismatch cannot be encoded away: `-5µs` reinterpreted as
  /// an unsigned microsecond count is 584542 years, which is a plausible-
  /// looking value no downstream check would question. Loud here, before it
  /// crosses — and the message names the peer types that *do* accept a
  /// negative span, because switching to one is the fix when the value is
  /// legitimately negative.
  ///
  /// Only the `std` peer is written this way; `chrono::Duration` and
  /// `time::Duration` are signed and take the plain [writeI64] path.
  void writeUnsignedDuration(Duration v) {
    if (v.isNegative) {
      throw ArgumentError.value(
        v,
        'v',
        'a negative Duration cannot cross to std::time::Duration, which is '
            'unsigned — declare the Rust peer as chrono::Duration or '
            'time::Duration if the value may be negative',
      );
    }
    writeI64(v.inMicroseconds);
  }

  void writeBytes(Uint8List v) {
    writeLen(v.length);
    if (_deferSegment(v)) return;
    _ensure(v.length);
    _buf.setRange(_len, _len + v.length, v);
    _len += v.length;
  }

  /// Record [v] by reference instead of copying it into [_buf], if it is worth
  /// it. Returns whether the caller should skip its own copy.
  ///
  /// A payload staged through [_buf] is read out again by the transport, and
  /// on dart2wasm getting bytes *out* of the Dart heap costs a JS<->wasm
  /// crossing per byte (SDK `js_helper_patch.dart`, `_copyFromWasmI8Array`).
  /// A JS-backed payload staged this way pays that twice — inward through
  /// `setRange`, outward through `toJS` — where a referenced one reaches the
  /// transport with its backing intact and is copied once, by `Uint8Array.set`.
  ///
  /// `_external` writers keep the copy: native writes straight into the FFI
  /// block, which is the destination, so there is nothing to defer.
  bool _deferSegment(Uint8List v) {
    if (_external || v.length < segmentThreshold) return false;
    if (_len > _flushed) {
      _pieces.add(Uint8List.sublistView(_buf, _flushed, _len));
      _flushed = _len;
    }
    _pieces.add(v);
    _segBytes += v.length;
    return true;
  }

  /// The request as an ordered list of pieces.
  ///
  /// Each piece keeps its own backing. The transport copies them into linear
  /// memory one at a time, so a JS-backed payload reaches `Uint8Array.set`
  /// (a memcpy) instead of being staged through the Dart heap first, which
  /// costs a JS<->wasm crossing per byte.
  List<Uint8List> takePieces() {
    final out = List<Uint8List>.of(_pieces);
    if (_len > _flushed) out.add(Uint8List.sublistView(_buf, _flushed, _len));
    return out;
  }

  /// Total request length across [_buf] and every segment.
  int get totalLength => _len + _segBytes;

  /// Fixed-length byte array ([u8; N] on the Rust side).
  void writeByteArray(Uint8List v, int expectedLength) {
    if (v.length != expectedLength) {
      throw ArgumentError.value(
        v,
        'v',
        'expected exactly $expectedLength bytes, got ${v.length}',
      );
    }
    // Same deferral as writeBytes: a large `[u8; N]` is as worth referencing
    // as a `Vec<u8>`, and there is no length prefix to change.
    if (_deferSegment(v)) return;
    _ensure(v.length);
    _buf.setRange(_len, _len + v.length, v);
    _len += v.length;
  }

  /// Guard for a `[T; N]` value, called before its elements are written.
  ///
  /// A fixed array puts its length in the *type*, so the wire carries N
  /// elements and no count. That makes a wrong-length list something worse
  /// than short: the far side would read N elements regardless and take the
  /// difference out of whatever field comes next. Refused here, on the
  /// caller's own value, before any of it crosses — the same check
  /// [writeByteArray] makes for `[u8; N]`, which writes its payload in one go
  /// and so can carry it inline.
  void checkArrayLength(int actual, int expected) {
    if (actual != expected) {
      throw ArgumentError(
        'fixed array: expected exactly $expected elements, got $actual',
      );
    }
  }

  // ------------------------------------------------- typed numeric lists --
  //
  // `Vec<f64>` and friends used to encode one element at a time — n calls to
  // `writeF64`, each a bounds check, a `ByteData` store and a `_len` bump. The
  // elements of a Dart typed list are already the wire's bytes in the wire's
  // order (little-endian host, see [bulkTypedDataOk]), so the whole payload is
  // one `setRange` — a memcpy.
  //
  // **The length prefix is the caller's**, unlike [writeBytes]/[writeString]
  // which write their own. That split is deliberate and load-bearing: a Rust
  // `VecDeque` is two slices with one length, so the generated Rust encoder
  // writes `write_len(v.len())` once and then feeds both halves to the bulk
  // writer. Keeping Dart's shape identical means the two seams read the same
  // and the length stays visible in generated code. Do not "unify" these with
  // `writeBytes`.
  //
  // The fallback arm is *definitionally* the loop that shipped — the same
  // per-element method, called n times — so it needs no separate correctness
  // argument.
  //
  // **The parameter is `List<int>`/`List<double>`, not the typed class**, and
  // the typed representation is a runtime question (`v is Int32List`) rather
  // than a static one. That is what lets ONE generic data class serve every
  // instantiation: a `#[bridge(data)] struct Page<T> { items: Vec<T> }`
  // declares `List<T>` on the Dart side, so `Page<i32>` holds a `List<int>`
  // that may or may not be an `Int32List`, while its codec is still the
  // `Vec<i32>` one. Nothing else changes: the wire is the same bytes either
  // way, the memcpy is still taken whenever the representation allows it, and
  // every monomorphic call site still passes the typed list its signature
  // declares.

  /// Copy `bytes` bytes of [v]'s own storage straight into the buffer.
  ///
  /// [v] may be a view (`Float64List.sublistView`), so the source window starts
  /// at `v.offsetInBytes` — never at the start of `v.buffer`.
  void _writeBulk(TypedData v, int bytes) {
    _ensure(bytes);
    _buf.setRange(
      _len,
      _len + bytes,
      Uint8List.view(v.buffer, v.offsetInBytes, bytes),
    );
    _len += bytes;
  }

  void writeI8List(List<int> v) {
    if (_kBulkTypedData && v is Int8List) {
      _writeBulk(v, v.length);
    } else {
      for (final x in v) {
        writeI8(x);
      }
    }
  }

  void writeU8List(List<int> v) {
    if (_kBulkTypedData && v is Uint8List) {
      _writeBulk(v, v.length);
    } else {
      for (final x in v) {
        writeU8(x);
      }
    }
  }

  void writeI16List(List<int> v) {
    if (_kBulkTypedData && v is Int16List) {
      _writeBulk(v, v.length * 2);
    } else {
      for (final x in v) {
        writeI16(x);
      }
    }
  }

  void writeU16List(List<int> v) {
    if (_kBulkTypedData && v is Uint16List) {
      _writeBulk(v, v.length * 2);
    } else {
      for (final x in v) {
        writeU16(x);
      }
    }
  }

  void writeI32List(List<int> v) {
    if (_kBulkTypedData && v is Int32List) {
      _writeBulk(v, v.length * 4);
    } else {
      for (final x in v) {
        writeI32(x);
      }
    }
  }

  void writeU32List(List<int> v) {
    if (_kBulkTypedData && v is Uint32List) {
      _writeBulk(v, v.length * 4);
    } else {
      for (final x in v) {
        writeU32(x);
      }
    }
  }

  void writeF32List(List<double> v) {
    if (_kBulkTypedData && v is Float32List) {
      _writeBulk(v, v.length * 4);
    } else {
      for (final x in v) {
        writeF32(x);
      }
    }
  }

  void writeF64List(List<double> v) {
    if (_kBulkTypedData && v is Float64List) {
      _writeBulk(v, v.length * 8);
    } else {
      for (final x in v) {
        writeF64(x);
      }
    }
  }

  /// The bulk arm here is [bulkI64Ok]'s, one conjunct narrower than the widths
  /// above: the VM alone. Everywhere else this is the element loop, which goes
  /// through [writeI64] and so carries the JS-number range check on the one
  /// backend that needs it.
  void writeI64List(List<int> v) {
    if (_kBulkI64 && v is Int64List) {
      _writeBulk(v, v.length * 8);
    } else {
      for (final x in v) {
        writeI64(x);
      }
    }
  }

  // `u64`/`i128`/`u128` cross as `BigInt` and have no typed list to bulk at
  // all, so they are absent here and stay on the element loop everywhere.

  /// Opaque handle (u64 pointer value).
  void writeHandle(int v) {
    _ensure(8);
    if (kJsNumbers) {
      jsWriteHandle(_view, _len, v);
    } else {
      _view.setUint64(_len, v, Endian.little);
    }
    _len += 8;
  }
}

class BinaryReader {
  final Uint8List _data;
  final ByteData _view;
  int _pos = 0;

  BinaryReader(Uint8List data)
    : _data = data,
      _view = ByteData.sublistView(data);

  bool get isAtEnd => _pos == _data.length;

  /// Assert the buffer is fully consumed — a loud, attributable codec error
  /// otherwise. The generated response decode calls this after decoding a
  /// return value so trailing/garbage bytes are rejected, not silently
  /// ignored (a corrupt-envelope class).
  void assertConsumed() {
    if (_pos != _data.length) {
      throw StateError(
        'frustrate codec: ${_data.length - _pos} trailing '
        'byte(s) after decoded value (buffer not fully consumed)',
      );
    }
  }

  int _advance(int n) {
    final p = _pos;
    // Compare against the remaining space rather than `p + n` so the check
    // stays correct when a corrupt length prefix is near i64::MAX: `p + n`
    // would overflow to a negative int and slip past a naive `> length` test,
    // surfacing an unattributable RangeError deeper in the read instead of the
    // codec's own "truncated buffer". `n` is always non-negative here (lengths
    // are validated in readUsize before reaching this point).
    if (n > _data.length - p) {
      throw StateError(
        'frustrate codec: truncated buffer '
        '(need $n byte(s) at offset $p, ${_data.length - p} remaining)',
      );
    }
    _pos = p + n;
    return p;
  }

  bool readBool() {
    final b = readU8();
    if (b > 1) throw StateError('frustrate codec: invalid bool byte $b');
    return b == 1;
  }

  int readU8() => _data[_advance(1)];
  int readI8() => _view.getInt8(_advance(1));
  int readU16() => _view.getUint16(_advance(2), Endian.little);
  int readI16() => _view.getInt16(_advance(2), Endian.little);
  int readU32() => _view.getUint32(_advance(4), Endian.little);
  int readI32() => _view.getInt32(_advance(4), Endian.little);
  int readI64() {
    final p = _advance(8);
    return kJsNumbers ? jsReadI64(_view, p) : _view.getInt64(p, Endian.little);
  }

  /// See [BinaryWriter.writeU64] for the two-halves rationale.
  BigInt readU64() {
    final p = _advance(8);
    final lo = _view.getUint32(p, Endian.little);
    final hi = _view.getUint32(p + 4, Endian.little);
    return (BigInt.from(hi) << 32) | BigInt.from(lo);
  }

  /// See [BinaryWriter.writeU128]: two u64 halves, low then high.
  BigInt readU128() {
    final lo = readU64();
    final hi = readU64();
    return (hi << 64) | lo;
  }

  /// See [BinaryWriter.writeI128]: read the unsigned 128-bit value, then
  /// reinterpret the top bit as sign (two's complement).
  BigInt readI128() {
    final u = readU128();
    return u >= (BigInt.one << 127) ? u - (BigInt.one << 128) : u;
  }

  double readF32() => _view.getFloat32(_advance(4), Endian.little);
  double readF64() => _view.getFloat64(_advance(8), Endian.little);

  int readUsize() {
    final v = readI64();
    if (v < 0) throw StateError('frustrate codec: negative usize');
    return v;
  }

  int readIsize() => readI64();
  int readLen() => readUsize();

  String readString() {
    final len = readLen();
    final p = _advance(len);
    // Dart's UTF-8 decoder strips a byte-order mark from the *start* of its
    // input — a document convention that does not apply to a length-prefixed
    // wire field, where a leading U+FEFF is ordinary content. Left alone it
    // makes Rust -> Dart silently lossy in one direction only (the encoder
    // emits EF BB BF faithfully), so `echoString('\u{FEFF}x')` would return
    // 'x'. Skip the marks ourselves and put them back: the decoder removes at
    // most one, so the remainder must not start with another.
    var q = p;
    var boms = 0;
    while (p + len - q >= 3 &&
        _data[q] == 0xEF &&
        _data[q + 1] == 0xBB &&
        _data[q + 2] == 0xBF) {
      q += 3;
      boms++;
    }
    try {
      // Reject malformed UTF-8 (allowMalformed stays false) so a corrupt byte
      // stream cannot smuggle a lossy/wrong string. Re-wrap the decoder's
      // FormatException in the codec's own attributable form, naming the buffer
      // offset and length so the failure is traceable to the wire, not just to
      // "some string somewhere".
      final rest = utf8.decode(Uint8List.sublistView(_data, q, p + len));
      return boms == 0 ? rest : ('\u{FEFF}' * boms) + rest;
    } on FormatException catch (e) {
      throw StateError(
        'frustrate codec: invalid UTF-8 in string field at '
        'offset $p (length $len): ${e.message}',
      );
    }
  }

  /// Decode a `char` from its u32 codepoint into a one-character String.
  /// Rejects a lone surrogate or an out-of-range codepoint with a loud,
  /// attributable codec error (mirrors the UTF-8 check in [readString]) — a
  /// correctly generated peer only ever sends a valid scalar.
  String readChar() {
    final cp = readU32();
    if (cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) {
      throw StateError(
        'frustrate codec: invalid Unicode scalar value '
        'U+${cp.toRadixString(16).toUpperCase()} in char field',
      );
    }
    return String.fromCharCode(cp);
  }

  Uint8List readBytes() {
    final len = readLen();
    final p = _advance(len);
    return Uint8List.sublistView(_data, p, p + len);
  }

  Uint8List readByteArray(int length) {
    final p = _advance(length);
    return Uint8List.sublistView(_data, p, p + length);
  }

  int readHandle() {
    final p = _advance(8);
    return kJsNumbers
        ? jsReadHandle(_view, p)
        : _view.getUint64(p, Endian.little);
  }

  // ------------------------------------------------- typed numeric lists --
  //
  // `Vec<f64>` used to decode as
  // `Float64List.fromList(List.generate(n, (_) => r.readF64()))`: n *boxed*
  // doubles into a growable `List<double>`, then a second pass copying them
  // into the typed list. These read the payload in one `setRange` — a memcpy —
  // straight into the typed list that is returned.
  //
  // **Why the copy is not avoidable.** The wire offset of a `Vec<f64>` is
  // whatever the fields before it added up to, so it is not generally
  // 8-byte-aligned and `Float64List.view` over the response buffer is illegal,
  // so the byte range is copied somewhere aligned first. A *sync* bridge also
  // can never make zero-copy the contract: the buffer is a Rust pointer that is
  // freed after the call. (frustrate's native transport
  // happens to copy the response into a Dart list before the reader sees it,
  // but an aligned view would then alias and pin that whole buffer behind one
  // decoded field, which is a different bug rather than a win.)
  //
  // So the copy stays, and **the copy was never the cost**: what these remove
  // is the boxing and the growable intermediate.
  //
  // The length prefix is the caller's — `r.readF64List(r.readLen())`, which is
  // well defined because Dart evaluates arguments before the call. See the
  // matching note on [BinaryWriter.writeF64List] for why the split exists.
  //
  // The fallback arm reads with the same `ByteData` accessor and endianness the
  // single-element reader uses, so it is the loop that shipped.

  /// Bounds-check `n` elements of `width` bytes and advance past them.
  ///
  /// The check is on the **count**, before any multiplication: `n * width` for
  /// a corrupt `n` overflows to a small or negative number, and the allocation
  /// that follows would then be an out-of-memory abort rather than something a
  /// caller can catch. `n <= remaining ~/ width` implies `n * width <=
  /// remaining`, so nothing after this can overflow either.
  int _advanceElems(int n, int width) {
    final p = _pos;
    if (n < 0 || n > (_data.length - p) ~/ width) {
      throw StateError(
        'frustrate codec: truncated buffer '
        '(need $n element(s) of $width byte(s) at offset $p, '
        '${_data.length - p} remaining)',
      );
    }
    _pos = p + n * width;
    return p;
  }

  /// Copy `bytes` wire bytes from offset `p` into [dst]'s own storage.
  void _readBulk(TypedData dst, int p, int bytes) {
    Uint8List.view(
      dst.buffer,
      dst.offsetInBytes,
      bytes,
    ).setRange(0, bytes, _data, p);
  }

  Int8List readI8List(int n) {
    final p = _advanceElems(n, 1);
    final out = Int8List(n);
    if (_kBulkTypedData) {
      _readBulk(out, p, n);
    } else {
      for (var i = 0; i < n; i++) {
        out[i] = _view.getInt8(p + i);
      }
    }
    return out;
  }

  Uint8List readU8List(int n) {
    final p = _advanceElems(n, 1);
    final out = Uint8List(n);
    if (_kBulkTypedData) {
      _readBulk(out, p, n);
    } else {
      for (var i = 0; i < n; i++) {
        out[i] = _data[p + i];
      }
    }
    return out;
  }

  Int16List readI16List(int n) {
    final p = _advanceElems(n, 2);
    final out = Int16List(n);
    if (_kBulkTypedData) {
      _readBulk(out, p, n * 2);
    } else {
      for (var i = 0; i < n; i++) {
        out[i] = _view.getInt16(p + i * 2, Endian.little);
      }
    }
    return out;
  }

  Uint16List readU16List(int n) {
    final p = _advanceElems(n, 2);
    final out = Uint16List(n);
    if (_kBulkTypedData) {
      _readBulk(out, p, n * 2);
    } else {
      for (var i = 0; i < n; i++) {
        out[i] = _view.getUint16(p + i * 2, Endian.little);
      }
    }
    return out;
  }

  Int32List readI32List(int n) {
    final p = _advanceElems(n, 4);
    final out = Int32List(n);
    if (_kBulkTypedData) {
      _readBulk(out, p, n * 4);
    } else {
      for (var i = 0; i < n; i++) {
        out[i] = _view.getInt32(p + i * 4, Endian.little);
      }
    }
    return out;
  }

  Uint32List readU32List(int n) {
    final p = _advanceElems(n, 4);
    final out = Uint32List(n);
    if (_kBulkTypedData) {
      _readBulk(out, p, n * 4);
    } else {
      for (var i = 0; i < n; i++) {
        out[i] = _view.getUint32(p + i * 4, Endian.little);
      }
    }
    return out;
  }

  Float32List readF32List(int n) {
    final p = _advanceElems(n, 4);
    final out = Float32List(n);
    if (_kBulkTypedData) {
      _readBulk(out, p, n * 4);
    } else {
      for (var i = 0; i < n; i++) {
        out[i] = _view.getFloat32(p + i * 4, Endian.little);
      }
    }
    return out;
  }

  Float64List readF64List(int n) {
    final p = _advanceElems(n, 8);
    final out = Float64List(n);
    if (_kBulkTypedData) {
      _readBulk(out, p, n * 8);
    } else {
      for (var i = 0; i < n; i++) {
        out[i] = _view.getFloat64(p + i * 8, Endian.little);
      }
    }
    return out;
  }

  /// The bulk arm is [bulkI64Ok]'s — the VM alone; see
  /// [BinaryWriter.writeI64List].
  ///
  /// **Nothing here runs on dart2js**, and not because of the arms:
  /// `Int64List(n)` itself throws `UnsupportedError` there. That is exactly the
  /// cliff the `Int64List.fromList(List.generate(n, (_) => r.readI64()))` this
  /// replaces already had — unchanged, and described in `docs/ANNOTATIONS.md`
  /// under "dart2js/DDC development loop".
  ///
  /// The element arm carries [readI64]'s whole `kJsNumbers` conditional rather
  /// than a copy of one half of it, so it *is* that loop by construction
  /// instead of by argument. The condition is compile-time-constant, so each
  /// backend keeps exactly one side of it.
  Int64List readI64List(int n) {
    final p = _advanceElems(n, 8);
    final out = Int64List(n);
    if (_kBulkI64) {
      _readBulk(out, p, n * 8);
    } else {
      for (var i = 0; i < n; i++) {
        final at = p + i * 8;
        out[i] = kJsNumbers
            ? jsReadI64(_view, at)
            : _view.getInt64(at, Endian.little);
      }
    }
    return out;
  }
}
