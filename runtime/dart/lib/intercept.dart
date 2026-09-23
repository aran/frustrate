/// Put something between the generated bindings and the transport: tracing,
/// metrics, an assertion harness, a fault injector.
///
/// ```dart
/// import 'package:frustrate/intercept.dart';
///
/// class Traced extends DelegatingRuntime {
///   Traced(super.inner);
///
///   @override
///   Future<BinaryReader> aroundAsync(
///           int fnId, Future<BinaryReader> Function() next,
///           {bool deferred = false,
///           FrustrateCancelToken? cancel,
///           ActorHost? host}) async {
///     print('-> ${frustrateMemberNames[fnId]}');
///     try {
///       return await next();
///     } finally {
///       print('<- ${frustrateMemberNames[fnId]}');
///     }
///   }
/// }
///
/// FrustrateNative.init(libraryPath);
/// Frustrate.activate(Traced(Frustrate.instance));
/// ```
///
/// `frustrateMemberNames` is generated into the bindings, keyed by the same
/// fn id the hooks receive — a `Map<int, String>`, so the lookup is nullable.
/// Ids are derived from each member's wire facts rather than counted, so an id
/// absent from the map is one this interface has no member for.
///
/// ## Why this is a separate library
///
/// An app that never imports it links exactly one implementation of
/// `FrustrateRuntime`, so the compiler can devirtualise `callSync`/`callAsync`
/// and the free-function floor — the whole of frustrate's crossing cost — is
/// untouched. Importing this is what makes the interface polymorphic, and the
/// import is therefore the opt-in: there is no flag, and there is nothing to
/// turn off in a build that did not ask for it.
///
/// ## What one of these can and cannot see
///
/// Every outbound bridge call reaches [DelegatingRuntime.aroundSync] or
/// [DelegatingRuntime.aroundAsync] — free functions, opaque members,
/// constructors, actor methods (through [DelegatingActorHost]), deferred
/// completions, and the synthetic drop an actor's `dispose()` dispatches.
/// Object lifetime reaches [DelegatingHandleDrop]. Inbound Rust → Dart traffic
/// is reachable but not free: the handlers arrive as closures at
/// `openObject`/`openStream`/`openFunction`, and a subclass that wants to
/// observe stream items or callback invocations wraps them there — the base
/// forwards them untouched so a decorator that only times outbound calls
/// allocates nothing per registration.
///
/// ## Enriching a crash report
///
/// A failure nobody catches is already reported: an unhandled rejection reaches
/// the zone that issued the call, so `runZonedGuarded` and Flutter's
/// `PlatformDispatcher.onError` both see it. What it *carries* is where a
/// decorator earns its place.
///
/// A generated async binding catches its own failure and rethrows it with a
/// frame naming the member, because an async completion has no stack of its
/// own. That names the member and stops there — it cannot name the app's call
/// site, which is gone by the time the call completes. Capturing at *issue*
/// recovers it:
///
/// ```dart
/// @override
/// Future<BinaryReader> aroundAsync(
///     int fnId, Future<BinaryReader> Function() next,
///     {bool deferred = false,
///     FrustrateCancelToken? cancel,
///     ActorHost? host}) async {
///   final issued = StackTrace.current;
///   try {
///     return await next();
///   } catch (e) {
///     Error.throwWithStackTrace(e, issued);
///   }
/// }
/// ```
///
/// The binding's own restack is guarded on the stack being empty, so this
/// richer one survives rather than being overwritten by the one-frame one.
///
/// Three things to get right:
///
///   * **Capture at issue, not in the handler.** `StackTrace.current` inside
///     the `catch` runs after the call completed and describes the completion,
///     not the caller — which is the whole reason the binding's own frame is
///     all it can offer.
///   * **`Error.throwWithStackTrace`, not `rethrow`.** `rethrow` keeps the
///     binding's one-frame stack, so the issuing stack you captured is thrown
///     away and the decorator adds nothing. Swallowing instead of either hands
///     the caller a reader positioned at nothing.
///   * **A `CancelledCallException` is not a failure of the member.** It leaves
///     `next()` like the rest — whether the cancel claimed the call in flight
///     or refused it before it was issued — so a reporter that counts every
///     throw counts cancellations as incidents.
///
/// This costs a stack capture on every async call, which is why it is a
/// decorator and not the default: an app that does not import this library pays
/// nothing for it.
///
/// ## Two orderings
///
/// Both look backwards the first time:
///
///   * **A funnel entry precedes its own call's registrations.** A Rust → Dart
///     channel is opened while the *request is being encoded*, and encoding
///     happens inside the call — so `aroundSync('TextDoc.watch')` is recorded
///     before `openObject` is, not after.
///   * **A channel opened by an actor member registers on the host**, not on
///     the runtime, so overriding `openObject` on a `DelegatingRuntime` alone
///     will not see it. Subclass [DelegatingActorHost] and override
///     `spawnActorHost` to return yours. The two really are different
///     registries: a web actor's producer flag lives in its own worker's wasm
///     instance.
///
/// Typed arguments are deliberately out of reach; so is retry. See
/// [DelegatingRuntime].
library;

export 'src/delegating_runtime.dart';
