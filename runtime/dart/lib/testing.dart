/// Run generated bindings against a fake instead of a library.
///
/// ```dart
/// import 'package:frustrate/testing.dart';
/// import 'package:my_app/src/api.frustrate.dart';
///
/// class Docs extends FakeTextDoc {
///   @override
///   int lenChars() => 3;
/// }
///
/// class Api extends FakeMyApi {
///   @override
///   FakeTextDoc textDocNew() => Docs();
/// }
///
/// void main() {
///   setUp(() {
///     Frustrate.activate(FakeRuntime(FakeMyApiBridge(Api())));
///     addTearDown(Frustrate.reset);
///   });
/// }
/// ```
///
/// `FakeMyApi`, `FakeTextDoc` and `FakeMyApiBridge` are generated into the
/// bindings, one family per crate, with every member defaulting to a throw —
/// so a fake declares only what its test exercises, and a member the test did
/// not expect to be called arrives as a panic naming it rather than as a
/// plausible zero.
///
/// ## What a fake really exercises
///
/// The whole Dart side. Requests are encoded by the same generated encoders a
/// real call uses and decoded by the harness with the same walkers; responses
/// go back through [decodeEnvelope] and the generated decoders. Cancellation,
/// deferred actor completions, FIFO ordering on an executor, stream
/// backpressure and cancel, callback round trips, handle lifetime and the
/// never-throw-synchronously contract all run their real implementations —
/// `FakeRuntime` supplies the same `PendingCalls`, `StreamRouter` and handle
/// bookkeeping the platform transports do. What it cannot exercise is Rust:
/// its codec, its locks, its schedulers. See [FakeRuntime] for the list and
/// the reasons.
///
/// ## Why this is a separate library from the harness's contract
///
/// The generated harness is emitted *into* the bindings, so it is compiled
/// into every build. It names only package:frustrate/fake_contract.dart, which
/// declares interfaces and value classes and no second `FrustrateRuntime`.
/// This library is where `FakeRuntime` — which is one — lives, so importing
/// it is the opt-in, exactly as importing package:frustrate/intercept.dart is
/// for a decorator.
library;

export 'fake_contract.dart';
export 'src/envelope.dart' show decodeEnvelope;
export 'src/fake_runtime.dart';
