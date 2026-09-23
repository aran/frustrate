/// Response-envelope decoding shared by the platform transports. Not part of
/// the public package surface.
library;

import 'dart:async';
import 'dart:typed_data';

import 'binary_codec.dart';
import 'exceptions.dart';

/// The runtime ABI this package speaks; must match `frustrate::RUNTIME_ABI`
/// (runtime/rust/src/lib.rs), which both transports read out of the loaded
/// bridge at init and refuse to proceed on a mismatch.
///
/// One constant rather than one per transport on purpose: the failure it
/// guards is a *stale* half, and a version number stored twice can go stale
/// against itself — a bump applied to one transport would leave the other
/// silently accepting the module it was supposed to reject.
const int frustrateRuntimeAbi = 5;

/// Bytes of the response buffer the transports hand `frustrate_call_sync`,
/// which answers into it rather than leasing a `Vec` whenever the envelope
/// fits (`frustrate::envelope::respond_out`).
///
/// Not part of the ABI — the capacity travels as an argument, so each
/// transport could choose its own and the Rust side never assumes a value.
/// It is shared because both transports want the same trade and there is no
/// reason for them to disagree.
///
/// **Why 128.** The envelope is a status byte plus the encoded return, so this
/// covers every scalar, every small struct and every short string — the shapes
/// that make up the crossing floor — while keeping the one block a sync call
/// allocates inside the small-bin range of the wasm allocator for any ordinary
/// request. Responses past it (byte vectors, long strings) are dominated by
/// copying their own payload, so the crossing the lease costs them is noise.
///
/// Must be at least `frustrate::envelope::RESP_OUT_MIN_CAP` (24), which is
/// where the overflow path writes the lease triple.
const int frustrateRespSlabBytes = 128;

// Envelope statuses; must match runtime/rust/src/envelope.rs.
//
// Public because a fake bridge answers in this vocabulary: a generated
// harness (package:frustrate/fake_contract.dart) builds the same response
// envelopes a real library does, and a second private copy of these bytes
// could go stale against this one — the reason [frustrateRuntimeAbi] is one
// constant rather than one per transport.
const int statusOk = 0;
const int statusError = 1;
const int statusPanic = 2;
const int statusContention = 3;
// Stream and callback events ride the same channel keyed by channel id
// (stream_router.dart).
const int statusStreamItem = 4;
const int statusStreamEnd = 5;
const int statusCallbackCall = 6;
// A terminal, like statusStreamEnd, but reporting abandonment rather than
// completion: the Rust holder was collected without dispose().
const int statusLeaked = 7;
// `Err(e)` where `e` is a bridged type, encoded by value rather than flattened
// to its Display string. Only the generated binding knows how to decode the
// payload, so it supplies a `typedError` decoder at the call site.
const int statusTypedError = 8;

/// The exception a non-ok [status] carries; [r] is positioned at its payload.
///
/// [label] identifies the member that opened the channel (`'TextDoc.watch'`)
/// and is used only by [statusLeaked], whose whole job is attribution. The
/// call-completion paths have a caller and a stack, so they pass none.
/// [typedError] decodes a [statusTypedError] payload into the exception to
/// throw. Only the generated binding for the member being called knows the
/// error's type, so it is threaded in per call rather than looked up here.
Object envelopeException(
  int status,
  BinaryReader r, {
  String? label,
  Object Function(BinaryReader)? typedError,
}) => switch (status) {
  statusError => BridgeException(r.readString()),
  statusTypedError =>
    typedError == null
        // Unreachable in a consistent pair: the error type is part of the
        // IR the schema fingerprint covers, so a binding that does not know
        // about a member's typed error cannot match the library that sends
        // one, and `checkFrustrateSchema` fails at init. Kept because the
        // alternative on the impossible path is decoding a payload as
        // whatever the next case happens to expect.
        ? StateError(
            'frustrate: the library returned a typed error for a call whose '
            'binding does not know one. The generated binding and the Rust '
            'library disagree about this member — rebuild both from the same '
            'api.rs.',
          )
        : typedError(r),
  statusPanic => BridgePanicException(r.readString()),
  statusContention => ContentionException(r.readString()),
  statusLeaked => LeakedChannelError(_leakMessage(r.readString(), label)),
  _ => StateError('frustrate: unknown envelope status $status'),
};

/// Compose the leak message from the two halves that know something: Rust
/// supplies the holder's type, the binding supplies where the channel was
/// opened. Both are best-effort — say what is known and always say the fix.
String _leakMessage(String holderType, String? label) {
  final where = label == null ? '' : ' opened at $label';
  return 'the channel$where was abandoned: its Rust holder $holderType was '
      'garbage-collected without dispose(). Channel-holding handles must be '
      'disposed — call dispose() when done with the $holderType handle.';
}

/// Returns a reader positioned at the payload, or throws the envelope's
/// exception.
BinaryReader decodeEnvelope(
  Uint8List response, {
  Object Function(BinaryReader)? typedError,
}) {
  final r = BinaryReader(response);
  final status = r.readU8();
  if (status == statusOk) return r;
  throw envelopeException(status, r, typedError: typedError);
}

/// Completer variant of [decodeEnvelope] for async transports.
void completeWithEnvelope(
  Completer<BinaryReader> completer,
  Uint8List response, {
  Object Function(BinaryReader)? typedError,
}) {
  final r = BinaryReader(response);
  final status = r.readU8();
  if (status == statusOk) {
    completer.complete(r);
    return;
  }
  // Building the exception can itself throw: a typed error runs a generated
  // decoder here, and `readString` can fail on a truncated buffer. That
  // evaluation happens *inside the argument* to completeError, so a throw
  // escapes this function — and by now `PendingCalls` has already dropped the
  // completer from its maps, leaving a future nothing will ever settle. The
  // throw then lands in whatever called us: on native a bare `RawReceivePort`
  // callback, where `Zone.current` is the root zone and an unhandled error
  // aborts the isolate rather than reaching `runZonedGuarded`.
  //
  // So a failure to decode the failure is reported as the call's failure,
  // which is what every other corrupt envelope already does.
  Object error;
  try {
    error = envelopeException(status, r, typedError: typedError);
  } catch (e) {
    error = StateError(
      'frustrate: the response envelope for this call carried a typed error '
      'that could not be decoded — the payload is corrupt, or the generated '
      'binding and the Rust library disagree about its shape. Underlying: $e',
    );
  }
  completer.completeError(error);
}
