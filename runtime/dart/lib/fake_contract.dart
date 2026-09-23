/// What a generated fake harness is written against: the bridge base class it
/// extends, the wire it answers on, the sink a stream parameter reaches it as,
/// and the response-envelope vocabulary.
///
/// Application code does not import this. **Generated binding surfaces do** —
/// every one of them, unconditionally — which is the whole reason it exists
/// apart from package:frustrate/testing.dart.
///
/// ## Why the split
///
/// A binding surface carries its typed harness inline, because the harness
/// needs the private constructors (`TextDoc._`) and private codecs (`_encPoint`)
/// that only that library has. So whatever the harness names, every build of
/// every app that imports the bindings also names.
///
/// Everything here is an interface, a constant, or a value class. Nothing here
/// implements `FrustrateRuntime`. That is the property being protected: an app
/// links exactly one `FrustrateRuntime` unless it *itself* imports
/// package:frustrate/testing.dart (the fake) or package:frustrate/intercept.dart
/// (a decorator), so `callSync`/`callAsync` stay monomorphic and the crossing
/// floor is untouched — the same argument intercept.dart's library doc makes,
/// and it would be false if the harness had to name the fake transport.
///
/// The fake itself — `FakeRuntime`, its actor hosts, its handle registry —
/// lives in package:frustrate/testing.dart and implements [FakeWire]. That
/// library's own doc is the recipe for writing a fake and activating it.
library;

export 'src/envelope.dart'
    show
        decodeEnvelope,
        statusContention,
        statusError,
        statusOk,
        statusPanic,
        statusTypedError;
export 'src/fake_contract.dart';
