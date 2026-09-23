/// Web transport: a wasm instance of the bridge crate, reached via JS
/// interop. Serves both web builds; threading is a property of the module
/// (threaded wasm modules import a shared memory, detected at init).
///
/// On a single-threaded module async bridge calls run inline during
/// `callAsync` — they complete before the caller's `await` resumes and
/// provide no parallelism. Deliberate: Rust must have decoded the request
/// before the future is returned on every platform, and on web the only
/// thread is the caller's. Parallelism on web is the Actor model's job.
///
/// Threaded wasm requires SharedArrayBuffer (cross-origin isolation, or an
/// explicit browser enablement in test harnesses). This runtime creates the
/// shared memory, provides the `frustrate.spawn_worker` hook, and hosts the
/// pool's threads in Workers running the glue's pool pump.
///
/// Rust panics trap rather than unwind (panic=abort). The trap is caught in a
/// JS frame — the glue's `$frustrateCall` — and converted to the same
/// `BridgePanicException` native throws. No destructor ran, so state the
/// panicking call touched may be poisoned: a later acquisition suspends
/// forever rather than answering wrongly.
///
/// The wrapper and both worker pumps live in ONE glue script
/// (lib/src/js/frustrate.js, embedded byte-identically as
/// `frustrateGlueSource`). By default the wrapper is inline-injected and
/// workers ride blob: URLs; strict CSP blocks both, so CSP pages serve the
/// same file as a static asset — `<script src="frustrate.js">` (plain, no
/// async/defer) before init. The served script records its own URL in
/// `$frustrateGlueUrl`, and the runtime then loads workers from that URL
/// instead of blob:. Bazel delivery: `frustrate_web_glue`.
///
/// dart2wasm is the web platform; dart2js/DDC runs for development only.
/// There `int` is a 53-bit double, so the codec's 64-bit sites take a
/// two-halves arm (js_numbers.dart) that throws rather than truncates past
/// 2^53, the app's own arithmetic is silently 53-bit, and a release build on
/// that backend is refused ([_checkNumberBackend]).
library;

import 'dart:async';
import 'dart:convert';
import 'dart:js_interop';
import 'dart:js_interop_unsafe';
import 'dart:typed_data';

import 'binary_codec.dart';
import 'envelope.dart';
import 'exceptions.dart';
import 'glue_source.dart';
import 'js_numbers.dart';
import 'pending_calls.dart';
import 'pool_workers.dart';
import 'trap_explain.dart';
import 'runtime_core.dart';
import 'stream_router.dart';

@JS('WebAssembly.compile')
external JSPromise<JSObject> _wasmCompile(JSAny bufferSource);

@JS('WebAssembly.Module.imports')
external JSArray<JSObject> _wasmModuleImports(JSObject module);

@JS('WebAssembly.Memory')
extension type _WasmMemory._(JSObject _) implements JSObject {
  external _WasmMemory(JSObject descriptor);
}

/// `Object.assign`, for copying an ES module namespace before overriding one
/// of its keys. A namespace object is frozen, so the copy is the only way; and
/// Dart has no spread for a `JSObject`.
@JS('Object.assign')
external JSObject _objectAssign(JSObject target, JSObject source);

/// Maximum size of a threaded wasm shared memory, in 64KiB wasm pages. Must not
/// exceed the module's link-time `--max-memory` (the fixture builds use
/// 1GiB = 16384 pages); a provided maximum above the declared one fails
/// instantiation loudly.
const _sharedMemoryMaxPages = 16384;

/// Initial size of a threaded wasm shared memory (16MiB). Must be at least the
/// module's declared minimum; instantiation fails loudly otherwise.
const _sharedMemoryInitialPages = 256;

/// Bytes of stack per pool worker.
///
/// Stated once and used by both bootstraps, which is the point: the glue
/// allocates the block itself from the module's allocator on the non-bindgen
/// path, and hands the number to `__wbindgen_start` on the post-passed one,
/// where wasm-bindgen's thread transform allocates it instead. Its own default
/// is 2 MiB, so leaving the argument off would silently double the pool's
/// footprint on exactly the builds that already pay the most for memory.
const _poolWorkerStackBytes = 1 << 20;

/// Whether [module] was built for threaded wasm: such modules import their
/// (shared) linear memory rather than declaring one of their own.
///
/// Keyed on the import Kind and nothing else, because the namespace it arrives
/// under is not stable: rustc emits `env.memory`, and wasm-bindgen's thread
/// transform relocates it to the generated sidecar's namespace. "Imports a
/// memory at all" is the fact both spellings share.
///
/// That it is also *shared* cannot be read here — the `WebAssembly.Module`
/// reflection API reports no limits and no share flag — and does not need to
/// be: a single-threaded build never imports a memory. It exports its own,
/// before the post-pass and after it (measured on e2e/iroh_demo's module,
/// which is post-passed and exports `memory` with no memory import at all),
/// because only `--shared-memory` makes rustc pass `--import-memory`. So an
/// imported memory means a threaded module by construction, and
/// //bazel/wasm_import_check gates the rest of that shape at build time.
bool _importsMemory(JSObject module) {
  for (final entry in _wasmModuleImports(module).toDart) {
    if (entry.getProperty<JSString>('kind'.toJS).toDart == 'memory') {
      return true;
    }
  }
  return false;
}

_WasmMemory _makeSharedMemory() {
  final descriptor = JSObject()
    ..setProperty('initial'.toJS, _sharedMemoryInitialPages.toJS)
    ..setProperty('maximum'.toJS, _sharedMemoryMaxPages.toJS)
    ..setProperty('shared'.toJS, true.toJS);
  return _WasmMemory(descriptor);
}

// The Module overload of WebAssembly.instantiate: resolves to the Instance
// itself (the bytes overload resolves to {module, instance}).
@JS('WebAssembly.instantiate')
external JSPromise<JSObject> _wasmInstantiateModule(
  JSObject module,
  JSObject imports,
);

@JS('BigInt')
external JSAny _jsBigInt(int v);

@JS('Number')
external int _jsNumber(JSAny v);

// The schema-hash export returns a wasm i64, which the JS/BigInt integration
// hands back as a JS BigInt (possibly negative — a signed i64). Stringify it
// and parse a full-precision Dart BigInt; Number() would truncate past 2^53.
@JS('String')
external JSString _jsString(JSAny? v);

@JS('fetch')
external JSPromise<_Response> _fetch(JSString url);

extension type _Response._(JSObject _) implements JSObject {
  external bool get ok;
  external int get status;
  external JSPromise<JSArrayBuffer> arrayBuffer();
}

@JS('Uint8Array')
extension type _Uint8ArrayView._(JSUint8Array _) implements JSUint8Array {
  external _Uint8ArrayView(JSObject buffer, int byteOffset, int length);
}

@JS('DataView')
extension type _DataView._(JSDataView _) implements JSDataView {
  external _DataView(JSObject buffer, int byteOffset, int length);
}

/// A freshly allocated JS byte array, owned by us.
///
/// The actor path needs one: it transfers the request buffer, and transferring
/// detaches it. A buffer we allocated is safe to detach; the caller's own is
/// not (see `_WebActorHost.call`).
@JS('Uint8Array')
extension type _JSBytes._(JSUint8Array _) implements JSUint8Array {
  external _JSBytes(int length);
}

/// The JS-owned frame every wasm entry goes through (see [_call]).
///
/// Fixed arity on purpose: the previous `(f, argsArray)` shape made every call
/// build a `JSArray` out of a Dart list, which under dart2wasm is an
/// allocation plus one crossing per element — more than the whole frame costs
/// now. Six covers the widest export the runtime calls; a
/// wasm export ignores arguments past its own arity, so the unused tail is
/// free. The per-primitive costs behind that are in [glue_source.dart]'s frame
/// comment, beside the convention they decided.
@JS(r'$frustrateCall')
external JSAny? _frustrateCall(
  JSFunction f,
  JSAny? a,
  JSAny? b,
  JSAny? c,
  JSAny? d,
  JSAny? e,
  JSAny? g,
);

/// The same frame, declared to return an `int`.
///
/// Two declarations of one JS function, not two frames: the glue is untouched.
/// It exists because `frustrate_call_sync` returns an i32 and the alternative
/// ways to read it back are both worse than the entry it saves — `Number(v)`
/// is a JS call, and going through `JSAny?` gives dart2wasm nothing to
/// specialize. A statically typed `external` returning `int` is the cheapest
/// primitive available here, the same one `_jsNumber` above relies on.
@JS(r'$frustrateCall')
external int _frustrateCallInt(
  JSFunction f,
  JSAny? a,
  JSAny? b,
  JSAny? c,
  JSAny? d,
  JSAny? e,
  JSAny? g,
);

/// What the glue prefixes onto a trap it caught. Everything before it is the
/// JS `Error:` wrapping; everything after is the engine's own message.
const String _trapMarker = 'frustrate-wasm-trap:';

/// Dynamic `import()`, reached through the glue (see frustrate.js). Loading the
/// wasm-bindgen sidecar is the one thing the runtime cannot do in Dart alone.
@JS(r'$frustrateImport')
external JSPromise<JSObject> _frustrateImport(JSString url);

@JS('URL')
extension type _Url._(JSObject _) implements JSObject {
  external _Url(JSString url, JSString base);
  external JSString get href;
}

@JS('location')
external JSObject? get _location;

/// Resolve [url] against the page, so a worker — whose own base URL is the
/// glue's, not the document's — resolves it to the same file.
String _absolute(String url) {
  final here = _location?.getProperty<JSString>('href'.toJS);
  if (here == null) return url;
  return _Url(url.toJS, here).href.toDart;
}

/// The served glue's URL, when the page has one: set by frustrate.js itself
/// when loaded via `<script src>`, or manually before init (the escape hatch
/// for bundlers that rename assets).
String? _servedGlueUrl() {
  final v = globalContext.getProperty<JSAny?>(r'$frustrateGlueUrl'.toJS);
  if (v == null || !v.isA<JSString>()) return null;
  final s = (v as JSString).toDart;
  return s.isEmpty ? null : s;
}

/// Install the JS-owned call frame (see library docs). Three paths: the
/// glue already ran (served `<script src>` — the strict-CSP delivery);
/// `$frustrateGlueUrl` was set manually, so load the served copy (inline
/// injection would be blocked on exactly the CSP pages that need this);
/// else inject the embedded copy inline.
Future<void> _installGlue() async {
  if (globalContext.getProperty<JSAny?>(r'$frustrateCall'.toJS) != null) return;
  final document = globalContext.getProperty<JSObject>('document'.toJS);
  final script = document.callMethod<JSObject>(
    'createElement'.toJS,
    'script'.toJS,
  );
  final servedUrl = _servedGlueUrl();
  if (servedUrl != null) {
    final loaded = Completer<void>();
    script.setProperty('src'.toJS, servedUrl.toJS);
    script.callMethod(
      'addEventListener'.toJS,
      'load'.toJS,
      (() => loaded.complete()).toJS,
    );
    script.callMethod(
      'addEventListener'.toJS,
      'error'.toJS,
      (() => loaded.completeError(
        StateError(
          'frustrate: loading the glue script from \$frustrateGlueUrl '
          '($servedUrl) failed — is the asset served at that URL?',
        ),
      )).toJS,
    );
    document
        .getProperty<JSObject>('head'.toJS)
        .callMethod('appendChild'.toJS, script);
    await loaded.future;
  } else {
    script.setProperty('text'.toJS, frustrateGlueSource.toJS);
    document
        .getProperty<JSObject>('head'.toJS)
        .callMethod('appendChild'.toJS, script);
  }
  if (globalContext.getProperty<JSAny?>(r'$frustrateCall'.toJS) == null) {
    throw StateError(
      'frustrate: installing the JS call wrapper failed — is CSP blocking '
      'inline scripts? Serve the glue as a static asset and load it with '
      '<script src="frustrate.js"></script> (plain, no async/defer) before '
      'FrustrateWeb.init. Bazel: frustrate_web_glue.',
    );
  }
}

/// Set by `flutter build web` (and `flutter build` generally) for a release
/// build; Flutter's own `kReleaseMode` is the same variable. Read directly so
/// the runtime keeps no Flutter dependency.
const bool _kReleaseMode = bool.fromEnvironment('dart.vm.product');

/// The deliberate escape from [_checkNumberBackend]: `-Dfrustrate.allowJsNumbers=true`.
///
/// It exists so the fence is a decision rather than a wall, and it is spelled
/// as a compile-time define on purpose — nothing an app can reach at runtime,
/// and nothing a release build carries unless somebody typed it.
const bool _kAllowJsNumbers = bool.fromEnvironment('frustrate.allowJsNumbers');

bool _warnedAboutJsNumbers = false;

/// Refuse a release build on a JavaScript-number backend, and warn about a
/// development one.
///
/// dart2js and DDC give Dart a 53-bit `int` and 32-bit bitwise operations.
/// frustrate's codec handles its own half of that exactly and loudly — values
/// past 2^53 throw rather than truncate (js_numbers.dart) — but it can do
/// nothing about the app's own arithmetic, where `1 << 62` is `0` silently.
/// That is survivable while iterating and not survivable in production, so the
/// two cases are separated here rather than left to a docstring.
///
/// **What this does not catch**, stated so it is not mistaken for a wall:
/// `dart.vm.product` is set by `flutter build web`, so the default release
/// path is covered — but `flutter build web --debug` is not, and a bare
/// `dart compile js` outside Flutter sets nothing. The fence stops the
/// accident, not a determined deploy.
void _checkNumberBackend() {
  switch (jsNumberVerdict(
    jsNumbers: kJsNumbers,
    releaseMode: _kReleaseMode,
    allowedInRelease: _kAllowJsNumbers,
  )) {
    case JsNumberVerdict.supported:
      return;
    case JsNumberVerdict.refused:
      throw StateError(jsNumberRefusal);
    case JsNumberVerdict.developmentOnly:
      if (_warnedAboutJsNumbers) return;
      _warnedAboutJsNumbers = true;
      // The one diagnostic this runtime prints. It earns that because the
      // hazard is invisible: nothing throws, the app just computes different
      // numbers than it will in production, and there is no other cue.
      // ignore: avoid_print
      print(jsNumberWarning);
  }
}

/// Web entry point: instantiate the bridge wasm module once, then use the
/// generated API.
class FrustrateWeb {
  /// Instantiate the bridge module from its wasm bytes and install the
  /// transport.
  ///
  /// Follows the repeated-init contract stated on [Frustrate], the same one
  /// native follows: naming the same module again does nothing — it does not
  /// reach `WebAssembly.compile`, does not create a second shared memory and
  /// does not instantiate a second module — and naming a *different* one
  /// throws, because the first module stays installed and would keep serving
  /// every call.
  ///
  /// **How a module is identified**, since bytes are not cheap to compare: by
  /// what the caller named it — the URL for [initFromUrl], the module's size
  /// for this entry point — together with [bindgenGlueUrl], which is part of
  /// what the module *is* (the same bytes with a different sidecar are a
  /// different bridge). So two different modules of byte-identical length,
  /// passed as bytes, are not told apart: the check is a fence against the
  /// accident, not an identity comparison.
  ///
  /// The compiled `WebAssembly.Module` is kept: actor hosts post it to their
  /// workers (compiled code is shared; memory is per-instance).
  ///
  /// [bindgenGlueUrl] is required if and only if the bridge crate's dependency
  /// graph contains `wasm-bindgen` — anything reaching a browser API from Rust,
  /// which for a real dependency is the common case rather than the exotic one.
  /// Such a module links against an import namespace of generated shims that
  /// only wasm-bindgen's own output can satisfy; point this at that file
  /// (`<crate>_bg.js`, served beside the `.wasm`). Omitting it when the module
  /// needs it fails at instantiation with a message naming the namespace.
  /// Modules with no wasm-bindgen in the graph need nothing and are unaffected.
  ///
  /// A module built for `wasm32-wasip1` needs no argument: its
  /// `wasi_snapshot_preview1` host is supplied unconditionally from the glue.
  static Future<void> init(
    Uint8List wasmModuleBytes, {
    String? bindgenGlueUrl,
  }) async {
    // Before anything else, because a build that must not exist should not get
    // as far as compiling a module.
    _checkNumberBackend();
    final glue = bindgenGlueUrl == null ? null : _absolute(bindgenGlueUrl);
    // Size stands in for the bytes; see the identity note above.
    final source = ('bytes', wasmModuleBytes.length, glue);
    final description = _describe(
      'a ${wasmModuleBytes.length}-byte module',
      glue,
    );
    if (Frustrate.initializedFrom(source, description)) return;
    await _instantiate(wasmModuleBytes, glue, source, description);
  }

  /// Fetch the bridge module from [url] (same COOP/COEP-isolated origin) and
  /// [init] with its bytes.
  ///
  /// A repeated call naming the same [url] costs nothing at all — the contract
  /// is settled before the fetch, so it is not even a re-download — and one
  /// naming a different URL is refused without going to the network. The URL is
  /// resolved against the page first, so the identity does not depend on which
  /// relative spelling a caller used.
  ///
  /// Note that this and [init] name a module in two different ways, so
  /// initializing through one and then the other is reported as a disagreement.
  static Future<void> initFromUrl(String url, {String? bindgenGlueUrl}) async {
    // Same reason as in [init]: a build that must not exist should not get as
    // far as fetching a module.
    _checkNumberBackend();
    final glue = bindgenGlueUrl == null ? null : _absolute(bindgenGlueUrl);
    final resolved = _absolute(url);
    final source = ('url', resolved, glue);
    final description = _describe('the module at $resolved', glue);
    if (Frustrate.initializedFrom(source, description)) return;
    final response = await _fetch(url.toJS).toDart;
    if (!response.ok) {
      throw StateError(
        'frustrate: fetching bridge module $url failed (HTTP ${response.status})',
      );
    }
    final buffer = await response.arrayBuffer().toDart;
    await _instantiate(buffer.toDart.asUint8List(), glue, source, description);
  }

  /// How an installed module is named in a repeated-init disagreement.
  /// [bindgenGlueUrl] is part of the name because it is part of the bridge.
  static String _describe(String module, String? bindgenGlueUrl) =>
      bindgenGlueUrl == null
      ? module
      : '$module with bindgenGlueUrl $bindgenGlueUrl';

  /// Compile, instantiate and install — everything both entry points share,
  /// reached only once the repeated-init guard has let the call through.
  /// [bindgenGlueUrl] is already resolved against the page.
  static Future<void> _instantiate(
    Uint8List wasmModuleBytes,
    String? bindgenGlueUrl,
    Object source,
    String description,
  ) async {
    // Workers registered while no transport is installed cannot belong to this
    // Dart heap — this is its first init — so they are what a previous one
    // left behind (see [_workerRegistryKey]). Reaped before anything else so a
    // hot restart costs a pool's width once instead of once per restart.
    if (!Frustrate.isInstalled) _reapOrphanedWorkers();
    await _installGlue();
    final runtime = WebRuntime._();
    runtime._bindgenGlueUrl = bindgenGlueUrl;
    final moduleBytes = wasmModuleBytes.toJS; // per-byte: Dart-heap init only
    final module = await _wasmCompile(moduleBytes).toDart;
    runtime._module = module;
    // Threaded wasm modules import a shared memory; create it before
    // instantiation.
    //
    // Cross-origin isolation is required for the *pool*, not for this line:
    // measured on desktop Chrome, a page that is not isolated still builds a
    // shared `WebAssembly.Memory` here (the carve-out blocks the
    // `SharedArrayBuffer` constructor and blocks postMessaging one). Handing a
    // pool worker this memory IS that postMessage, so a non-isolated page fails
    // in `_spawnPoolWorkerInner` instead, loudly and on the pool verdict path.
    // An actor never crosses that line — it creates its memory inside its own
    // Worker — which is why an actor-only app can appear to work without the
    // headers. Serve them (CHARTER.md).
    if (_importsMemory(module)) {
      runtime._sharedMemory = _makeSharedMemory();
    }
    final bindgen = await runtime._loadBindgen(module);
    final instance = await _wasmInstantiateModule(
      module,
      runtime._imports(bindgen),
    ).toDart;
    runtime._start(instance, bindgen);
    // The browser's UTF-8 encoder, for strings big enough to be worth a JS
    // call. Byte-identical to `utf8.encode` — `string_encoder_hook_test` and
    // `utf8_equivalence_test` are what say so — and its result is JS-born, so
    // the writer's segment path hands it to `Uint8Array.set` as a memcpy
    // rather than transcoding in Dart and then crossing per byte.
    //
    // Guarded rather than assumed: a host without `TextEncoder` (d8, jsshell)
    // keeps the Dart path, which is correct there and merely slower.
    if (_textEncoder != null) {
      BinaryWriter.stringEncodeHook = _encodeStringJs;
    }
    Frustrate.install(runtime, source: source, description: description);
  }
}

/// `String` reaches JS in O(1) — on dart2wasm a Dart `String` *is* a JS string
/// — and the result stays JS-backed, because `.toDart` on a `JSUint8Array` is
/// a wrapper rather than a copy. Both halves matter: either one copying would
/// give back the per-byte crossing this exists to avoid.
Uint8List _encodeStringJs(String s) => _textEncoder!.encode(s).toDart;

@JS('TextEncoder')
extension type _TextEncoder._(JSObject _) implements JSObject {
  external factory _TextEncoder();
  external JSUint8Array encode(String s);
}

/// Detected once, the way the SDK detects its own `TextDecoder`.
final _TextEncoder? _textEncoder = () {
  try {
    return _TextEncoder();
  } catch (_) {
    return null;
  }
}();

final class WebRuntime implements FrustrateRuntime {
  late final JSObject _exports;
  late final JSObject _memory;

  /// The compiled bridge module, shared with every worker (both pumps).
  late final JSObject _module;

  /// Absolute URL of the wasm-bindgen sidecar, or null when the module needs
  /// none.
  String? _bindgenGlueUrl;

  /// The head both worker-init messages share. Handing a worker the compiled
  /// module and handing it the sidecar URL are one act: the module's foreign
  /// import namespaces are exactly what only the sidecar can satisfy, so a
  /// worker given the module alone instantiates nothing and fails at startup
  /// with an error telling the developer to pass a URL they already passed.
  /// Built in one place so neither pump can be given half of it —
  /// test/worker_init_message_test.dart pins that it stays that way.
  ///
  /// The URL rather than the namespace object: an ES module namespace is not
  /// structured-cloneable, so each worker imports its own copy. Absolute
  /// already (resolved against the document at init), because a worker's base
  /// URL is the glue's, not the page's.
  JSObject _moduleInitMessage(String type) => JSObject()
    ..setProperty('type'.toJS, type.toJS)
    ..setProperty('module'.toJS, _module)
    ..setProperty('bindgenGlueUrl'.toJS, _bindgenGlueUrl?.toJS);

  /// Namespaces this runtime supplies itself, so an import from one of them is
  /// NOT evidence that the module needs a wasm-bindgen sidecar. Mirrors
  /// `selfSupplied` in frustrate.js.
  ///
  /// `wasi_snapshot_preview1` is here for the same reason `frustrate` is: it is
  /// in the import object [_imports] builds, unconditionally. Omitting it made
  /// every wasm32-wasip1 module fail to load with an error naming wasm-bindgen
  /// — a branch of the platform decision tree that cannot use wasm-bindgen at
  /// all.
  static const Set<String> _selfSuppliedNamespaces = {
    'frustrate',
    'env',
    'wasi_snapshot_preview1',
  };

  /// Load the wasm-bindgen namespace this module needs, or null if it needs
  /// none. Mirrors `bindgenImports` in frustrate.js — same rule, same error,
  /// two contexts.
  Future<JSObject?> _loadBindgen(JSObject module) async {
    final foreign = <String>{};
    for (final entry in _wasmModuleImports(module).toDart) {
      final ns = entry.getProperty<JSString>('module'.toJS).toDart;
      if (!_selfSuppliedNamespaces.contains(ns)) foreign.add(ns);
    }
    if (foreign.isEmpty) return null;
    final url = _bindgenGlueUrl;
    if (url == null) {
      throw StateError(
        'frustrate: this bridge module imports ${foreign.join(', ')}, which '
        "only wasm-bindgen's generated JS can satisfy. Pass its URL as "
        'FrustrateWeb.init(bindgenGlueUrl: ...) and serve it beside the '
        '.wasm module. Bazel: frustrate_wasm_module(bindgen = ...)',
      );
    }
    _bindgenNamespaces = foreign;
    return await _frustrateImport(url.toJS).toDart;
  }

  /// The import namespaces [_loadBindgen] found, all satisfied by the one
  /// module it loaded.
  Set<String> _bindgenNamespaces = const {};

  /// Threads only: the shared linear memory this instance and its pool
  /// workers run on. Null on single-threaded modules (they export their
  /// own memory).
  _WasmMemory? _sharedMemory;

  /// Threads only: the live pool workers, and the verdict when one fails to
  /// start. Fatal fails every call that can no longer complete and latches so
  /// [callAsync] refuses new ones; degraded has no call to attach to and goes
  /// to [_zone].
  late final PoolWorkers<_JSWorker> _poolWorkers = PoolWorkers(
    terminate: _terminateWorker,
    // Both reports are deferred a microtask, for one reason: either verdict
    // can be reached *inside* the reentrant spawn hook, with the triggering
    // call still mid-`PendingCalls.issue` and wasm frames still on the stack.
    // Failing that call from here would settle its completer under `issue`'s
    // feet, and the spawn error unwinding out of wasm a moment later would
    // find its entry gone and go to the zone as a second, uncaught error;
    // running the app's zone handler from here would run app code inside a
    // bridge call it can re-enter. The verdict itself (the latch) is still
    // recorded synchronously, so a later `callAsync` refuses at once — it is
    // only the delivery that waits for the stack to unwind.
    onFatal: (error) {
      final st = StackTrace.current;
      scheduleMicrotask(() => _pending.failAll(error, st));
    },
    onDegraded: (error) {
      final st = StackTrace.current;
      scheduleMicrotask(() => _zone.handleUncaughtError(error, st));
    },
  );

  /// The zone `FrustrateWeb.init` ran in. A pool worker's spawn failure is
  /// discovered on a bare JS event callback — no Dart caller above it, and
  /// nothing to throw at — so the report goes here instead. Captured at
  /// install rather than read at report time because `Zone.current` inside a
  /// JS callback is the *root* zone: an app's `runZonedGuarded` would never
  /// see it, which is the difference between an error it can log, ignore, or
  /// rethrow and one that only ever reaches the console.
  final Zone _zone = Zone.current;

  // The page's single id sequence. Every id that can appear on the wire —
  // a call id on this instance, a call id on any actor host, a stream or
  // callback id opened against any of them — is drawn from here, so ids are
  // unique page-wide and a single router can route by id alone. (Native gets
  // the same property from `_allocId`'s isolate tag; web needs no tag, so
  // ids stay tag-0 and well inside the JS 2^53 integer boundary.)
  //
  // Sharing is load-bearing, not tidiness: an actor host with its own
  // sequence would mint call ids colliding with this router's stream ids,
  // and `deliver` would swallow a call completion as a stream event.
  int _nextCallId = 1;
  int _allocId() => _nextCallId++;

  /// This instance's own async calls. Actor hosts keep their own registries
  /// (their worker's pump completes them), so both contribute to [_inFlight]
  /// rather than sharing this one.
  late final PendingCalls _pending = PendingCalls(
    _allocId,
    cancelCall: (id) => _callCancel(id),
    tally: _inFlight,
  );

  /// Async calls in flight page-wide on this bridge: this instance's plus
  /// every actor host's. Shared with each host's [PendingCalls] at
  /// construction, because a transport cannot enumerate the hosts it spawned.
  final InFlightCalls _inFlight = InFlightCalls();

  /// The page's single stream/callback router. Actor hosts route through it
  /// too (their worker pumps call [StreamRouter.deliver] on it), so a handle
  /// is routable from any channel by its id alone.
  ///
  /// Returning callbacks (DartFunction) are portable via `call_async`: the
  /// router runs the closure and responds through the wasm export, exactly
  /// like native — but deferred to a microtask, since on single-threaded web
  /// the invocation fires inside the Rust poll's synchronous `post` import.
  /// `respond` reaches *this* instance's export, which is why an actor host
  /// still refuses `openFunction` (see `_WebActorHost.openFunction`).
  // `cancel` is the router's throw policy hook (a Dart method that threw
  // terminates its channel). It runs through [cancelStream], which consults
  // [_actorOwner] first: a handle minted during an actor call lives in that
  // worker's own wasm instance, so signalling this instance's registry would
  // be a no-op for it. Producer *observation* stays advisory on web actors
  // (the worker sees the message when its executor idles); what this
  // guarantees is that the signal reaches the right registry.
  late final StreamRouter _router = StreamRouter(
    _allocId,
    respond: _respondToCallback,
    cancel: cancelStream,
    deferDelivery: true,
    zone: _zone,
  );

  /// Which actor host owns a given stream/object id, for ids opened against a
  /// host rather than this instance. The router is page-wide and routes by id
  /// alone, but a *cancel* must reach the wasm instance whose registry holds
  /// the producer's flag — this table is what tells them apart. Retired at
  /// every exit `_WebActorHost._openStreams` is (terminal, cancel, shutdown),
  /// so it never outlives the registration.
  final Map<int, _WebActorHost> _actorOwner = {};

  /// Message captured by the panic hook for the trap currently propagating.
  String? _lastPanic;

  WebRuntime._();

  /// The `frustrate.*` import object (plus `env.memory` when threaded). Built
  /// before instantiation; the closures reach back into this runtime once
  /// `_start` has run (the module cannot call them earlier than its own
  /// instantiation).
  JSObject _imports([JSObject? bindgen]) {
    final post = (JSAny callId, int ptr, JSAny len, JSAny cap) {
      _onPost(_jsNumber(callId), ptr, _jsNumber(len), _jsNumber(cap));
    }.toJS;
    final panic = (int ptr, JSAny len) {
      _lastPanic = utf8.decode(_read(ptr, _jsNumber(len)));
    }.toJS;
    // Cooperative executor Scheduler hook (single-threaded web): Rust's
    // `executor::arrange_drain` calls this when a task becomes ready, and we
    // resume it on a microtask — yielding to the JS event loop between polls,
    // never blocking the main thread (the whole reason an `async fn` can run
    // on single-threaded web at all). Threaded modules drive drains on their
    // pool workers instead and never call this, but providing it is harmless.
    final scheduleDrain = (() {
      scheduleMicrotask(_drainExecutor);
    }).toJS;
    // The std facilities a module built against a custom_std asks for —
    // `now_monotonic_ns`, `now_wall_ns`, `fill_random`, `write_stdio`,
    // `hardware_concurrency` (toolchain/custom_std). Taken from the glue so
    // this site, the pool worker and the actor worker all serve one
    // implementation; a module built against a stock std declares none of
    // them and instantiation never looks them up, so this is unconditional
    // for the same reason the wasip1 host below is.
    final ns =
        globalContext
                .getProperty<JSFunction>(r'$frustrateStdFacilities'.toJS)
                .callAsFunction(null, (() => _memory).toJS)!
            as JSObject;
    ns
      ..setProperty('post'.toJS, post)
      ..setProperty('panic'.toJS, panic)
      ..setProperty('schedule_drain'.toJS, scheduleDrain);
    final imports = JSObject()..setProperty('frustrate'.toJS, ns);
    // The wasip1 host, from the glue (installed before this runs). A module
    // built for wasm32-unknown-unknown imports none of it and instantiation
    // simply never looks the namespace up, so this is unconditional — there
    // is no per-target branch to get wrong. The memory is passed as a getter
    // because `_memory` is not assigned until [_start], after instantiation.
    imports.setProperty(
      'wasi_snapshot_preview1'.toJS,
      globalContext
              .getProperty<JSFunction>(r'$frustrateWasi'.toJS)
              .callAsFunction(null, (() => _memory).toJS)!
          as JSObject,
    );
    // `env.__stack_chk_fail`: toolchains_llvm hardcodes `-fstack-protector`
    // and it is not behind a Bazel feature, so any C in the crate graph leaves
    // exactly one unremovable import here. Always provided — a module that
    // does not import it ignores it.
    //
    // Declared `void` rather than written inline: `toJS` rejects a closure
    // whose return type infers to `Never`. Same shape as the worker `onerror`
    // handlers below, and for the same reason.
    void stackChkFail() {
      throw StateError(
        'frustrate: stack smashing detected in the bridge module',
      );
    }

    final env = JSObject()
      ..setProperty('__stack_chk_fail'.toJS, stackChkFail.toJS);
    imports.setProperty('env'.toJS, env);
    final shared = _sharedMemory;
    if (shared != null) {
      // Threaded wasm: the runtime's pool asks for threads through this hook
      // (synchronously, during the wasm call that first touches the pool).
      ns.setProperty(
        'spawn_worker'.toJS,
        ((int entryPtr) {
          _spawnPoolWorker(entryPtr);
        }).toJS,
      );
      env.setProperty('memory'.toJS, shared);
    }
    if (bindgen != null) {
      // On a threaded module the sidecar is also where wasm-bindgen's thread
      // transform put the memory import, and the generated ES module creates
      // one of its own at the top level — one per realm, so adopting it would
      // hand every Worker a different shared memory. Supply this runtime's
      // instead, on a copy because a module namespace is frozen. Nothing reads
      // the binding we shadow: the module re-exports the memory it imports and
      // every generated view goes through `wasm.memory.buffer` on the
      // instance. Mirrors `bindgenImports` in frustrate.js, which does the
      // same for the two worker pumps.
      final supplied = shared == null
          ? bindgen
          : (_objectAssign(JSObject(), bindgen)
              ..setProperty('memory'.toJS, shared));
      for (final name in _bindgenNamespaces) {
        imports.setProperty(name.toJS, supplied);
      }
    }
    return imports;
  }

  late final JSFunction _callSyncFn;
  late final JSFunction _callAsyncFn;
  late final JSFunction _allocFn;
  late final JSFunction _freeFn;

  void _start(JSObject instance, [JSObject? bindgen]) {
    _exports = instance.getProperty<JSObject>('exports'.toJS);
    // wasm-bindgen's two-step handshake, and the order is load-bearing: hand
    // the generated JS the instance it closes over, then run the module's
    // start function — both before any frustrate export, because a shim
    // called before `__wbg_set_wasm` dereferences `undefined`.
    if (bindgen != null) {
      bindgen.callMethod('__wbg_set_wasm'.toJS, _exports);
      if (_exports.getProperty<JSAny?>('__wbindgen_start'.toJS) != null) {
        _call(_export('__wbindgen_start'));
      }
    }
    _memory = _sharedMemory ?? _exports.getProperty<JSObject>('memory'.toJS);
    _checkRuntimeAbi();
    _callSyncFn = _export('frustrate_call_sync');
    _callAsyncFn = _export('frustrate_call_async');
    _allocFn = _export('frustrate_alloc');
    _freeFn = _export('frustrate_buffer_free');
    _call(_export('frustrate_web_init'));
  }

  @override
  bool get asyncIsParallel => _sharedMemory != null;

  @override
  int get hardwareParallelism {
    final nav = globalContext.getProperty<JSObject?>('navigator'.toJS);
    final n = nav
        ?.getProperty<JSNumber?>('hardwareConcurrency'.toJS)
        ?.toDartInt;
    return (n == null || n < 1) ? 4 : n;
  }

  @override
  int get openChannelCount => _router.openRegistrationCount;

  @override
  List<String> get openChannelLabels => _router.openChannelLabels;

  /// A platform transport *is* its bridge: its raws, its drop exports and its
  /// router are all its own.
  @override
  Object get bridgeIdentity => this;

  @override
  int get inFlightCallCount => _inFlight.count;

  /// Threaded wasm: spawn one pool worker. Without the wasm-bindgen post-pass
  /// the stack and TLS blocks come from the instance's own (thread-safe)
  /// allocator, reentrantly — the module is mid-call when this hook fires. With
  /// it they come from the same allocator on the *worker*, inside
  /// `__wbindgen_start`, so an exhausted memory surfaces there as an `initError`
  /// and a pool verdict rather than here as an exception on the triggering call.
  ///
  /// Which is also why every failure in here is parked on the way out: it has
  /// to cross wasm frames to reach the call that triggered the spawn, and that
  /// crossing keeps only the exception's identity ([_reentrantError]).
  void _spawnPoolWorker(int entryPtr) {
    try {
      _spawnPoolWorkerInner(entryPtr);
    } catch (e, st) {
      // Park the real error FIRST: the verdict below runs app-visible code
      // (the degraded report), and a handler that throws must not cost the
      // triggering call its cause — without the parked error the crossing
      // back through wasm leaves only `[object WebAssembly.Exception]`.
      _reentrantError = (e, st);
      // Every escape from here is a worker that will never run a job, so the
      // verdict layer has to hear about it — the triggering call rejects on
      // its own, but nothing else would ever learn the pool is short a worker
      // (or has none at all).
      _poolWorkers.spawnThrew('$e');
      rethrow;
    }
  }

  /// Whether wasm-bindgen's thread transform owns per-thread bootstrap in this
  /// module.
  ///
  /// The transform deletes `__wasm_init_tls`, `__tls_size` and `__tls_align`
  /// and replaces all three with `__wbindgen_start`, which allocates this
  /// thread's stack and TLS block from inside the module. So the export's
  /// absence is the question, and it decides who allocates: nothing here, or
  /// the two blocks below.
  late final bool _bindgenOwnsThreadInit =
      _exports.getProperty<JSAny?>('__wasm_init_tls'.toJS) == null;

  void _spawnPoolWorkerInner(int entryPtr) {
    final msg = _moduleInitMessage('pool-init')
      ..setProperty('memory'.toJS, _sharedMemory!)
      ..setProperty('stackSize'.toJS, _poolWorkerStackBytes.toJS)
      ..setProperty('entryPtr'.toJS, entryPtr.toJS);
    if (!_bindgenOwnsThreadInit) {
      final allocAligned = _export('frustrate_alloc_aligned');
      int alloc(int size, int align) =>
          _jsNumber(_call(allocAligned, size.toJS, align.toJS)!);
      int globalValue(String name) => _exports
          .getProperty<JSObject>(name.toJS)
          .getProperty<JSNumber>('value'.toJS)
          .toDartInt;

      final stackPtr = alloc(_poolWorkerStackBytes, 16);
      final tlsSize = globalValue('__tls_size');
      final tlsPtr = alloc(tlsSize, globalValue('__tls_align'));
      msg
        ..setProperty('stackTop'.toJS, (stackPtr + _poolWorkerStackBytes).toJS)
        ..setProperty('tlsPtr'.toJS, tlsPtr.toJS);
    }

    // CSP blocking `blob:` workers throws here; the caller latches it like
    // every other escape from this method.
    final _JSWorker worker = _spawnGlueWorker();
    worker.onmessage = ((JSObject e) {
      _onPoolMessage(worker, e.getProperty<JSObject>('data'.toJS));
    }).toJS;
    // The worker never ran its pump (script 404 / parse failure): it cannot
    // post initError itself, so report from here. Throwing instead would only
    // reach the browser's uncaught-error path — this fires from a bare JS
    // event dispatch, with no Dart caller to catch anything.
    worker.onerror = (() {
      _poolWorkers.initFailed(
        worker,
        'the worker script failed to load from ${_workerScriptUrl()}',
      );
    }).toJS;
    worker.postMessage(msg, JSArray());
    // Last, and deliberately: joining the live set is what makes this worker
    // a *candidate* the fatal decision waits for, so nothing joins it until
    // the spawn has fully succeeded. A `postMessage` that threw would
    // otherwise leave a worker in the set that never got its pool-init —
    // enough to make the caller's `spawnThrew` read "degraded" on a pool with
    // no functioning worker in it, which is the silent hang this whole seam
    // exists to prevent. Registered inside the same synchronous turn as the
    // handlers above, so no event can fire against an unregistered worker.
    _poolWorkers.add(worker);
  }

  /// Lazy: only threaded modules export it, and only trap handling calls it.
  late final JSFunction _poolReplenishFn = _export('frustrate_pool_replenish');

  void _onPoolMessage(_JSWorker worker, JSObject m) {
    final type = m.getProperty<JSString>('type'.toJS).toDart;
    switch (type) {
      case 'post':
        _onPost(
          m.getProperty<JSNumber>('callId'.toJS).toDartInt,
          m.getProperty<JSNumber>('ptr'.toJS).toDartInt,
          m.getProperty<JSNumber>('len'.toJS).toDartInt,
          m.getProperty<JSNumber>('cap'.toJS).toDartInt,
        );
      case 'trap':
        // A pool job panicked: under panic=abort the trap killed that worker
        // thread. Replace it immediately — pool width is an invariant
        // (every respawn is one loudly-failed user panic; capping would
        // reintroduce a silently narrowed pool). State the dead job held
        // stays held (no unwinding): poisoned, surfacing as loud
        // attributable errors — never silently wrong. The recorded call id
        // makes the failure attributable to exactly one future.
        // `retire`, not `initFailed`: a worker that trapped is one that ran,
        // and the replenish below replaces it — so the momentarily empty pool
        // this leaves is not a verdict.
        _poolWorkers.retire(worker);
        // Reentrantly fires spawn_worker -> _spawnPoolWorker. Before the
        // attribution branch, so even an unattributable trap's StateError
        // leaves the pool at full width. Guarded because a replenish that
        // itself traps (allocator OOM) would otherwise escape this callback
        // with the pool empty and nothing latched — every pending and future
        // call left hanging, the exact failure this seam exists to prevent.
        try {
          _call(_poolReplenishFn);
        } catch (e) {
          _poolWorkers.spawnThrew('replenishing after a trap failed: $e');
        }
        final callId = m.getProperty<JSNumber>('callId'.toJS).toDartInt;
        final message = m.getProperty<JSString>('message'.toJS).toDart;
        if (!_pending.fail(
          callId,
          BridgePanicException(message),
          StackTrace.current,
        )) {
          throw StateError(
            'frustrate: a pool worker trapped outside any attributable '
            'call (call id $callId): $message',
          );
        }
      case 'initError':
        // Instantiation failed, so this worker never ran a job and never
        // will — respawning it would be a no-progress storm. Whether that is
        // fatal depends on whether anything is left to drain the queue.
        _poolWorkers.initFailed(
          worker,
          'instantiation failed: ${m.getProperty<JSString>('message'.toJS).toDart}',
        );
    }
  }

  JSFunction _export(String name) {
    final f = _exports.getProperty<JSAny?>(name.toJS);
    if (f == null) {
      throw StateError('frustrate: bridge module has no export $name');
    }
    return f as JSFunction;
  }

  /// A Dart error unwinding out of a JS import back through wasm, parked so
  /// the outer [_call] can rethrow it intact.
  ///
  /// The glue re-enters Dart while the module is mid-call (`spawn_worker` ->
  /// [_spawnPoolWorker]), so a failure there has to cross wasm frames to reach
  /// its caller. `$frustrateCall` sees it only as a JS value and stringifies it
  /// to `[object WebAssembly.Exception]`, and any hook-captured panic message
  /// was already consumed by the nested [_call] that raised it. Parking the
  /// real error is what keeps "the shared memory is at its maximum" from
  /// arriving as that opaque string.
  (Object, StackTrace)? _reentrantError;

  /// Run one wasm entry through the JS-owned frame. A trap (Rust panic under
  /// panic=abort) comes back as a rethrown JS error; convert it to the panic
  /// exception the envelope would have carried, preferring a parked reentrant
  /// error, then the hook-captured message.
  ///
  /// Optional positionals rather than a list: a `List<JSAny?>.toJS` per entry
  /// is the single most expensive thing on the web crossing floor: an
  /// allocation plus a crossing per element, against one crossing for the whole
  /// call. Callers pass exactly the
  /// export's own arguments; the glue always calls with six and the export
  /// ignores the tail.
  JSAny? _call(
    JSFunction f, [
    JSAny? a,
    JSAny? b,
    JSAny? c,
    JSAny? d,
    JSAny? e,
    JSAny? g,
  ]) {
    try {
      return _frustrateCall(f, a, b, c, d, e, g);
    } catch (raised, st) {
      _rethrowFromFrame(raised, st);
    }
  }

  /// [_call] for an export that returns an `int` — today only
  /// `frustrate_call_sync`, whose i32 says whether the response fitted the
  /// slab. Identical trap handling, deliberately through the same helper: this
  /// is the frame `trap_attribution_test` pins, and a second copy of that
  /// `catch` is a second thing that can drift away from it.
  int _callInt(
    JSFunction f, [
    JSAny? a,
    JSAny? b,
    JSAny? c,
    JSAny? d,
    JSAny? e,
    JSAny? g,
  ]) {
    try {
      return _frustrateCallInt(f, a, b, c, d, e, g);
    } catch (raised, st) {
      _rethrowFromFrame(raised, st);
    }
  }

  /// Convert what the JS-owned frame threw into what the caller should see:
  /// the panic exception the envelope would have carried, preferring a parked
  /// reentrant error, then the hook-captured message.
  Never _rethrowFromFrame(Object raised, StackTrace st) {
    final text = raised.toString();
    final marker = text.indexOf(_trapMarker);
    // No marker means this never reached the glue's catch: a stale served
    // glue, a missing export, a bad argument. Dressing that up as a Rust
    // panic would misattribute it, so it leaves unchanged.
    if (marker < 0) Error.throwWithStackTrace(raised, st);
    final parked = _reentrantError;
    _reentrantError = null;
    if (parked != null) Error.throwWithStackTrace(parked.$1, parked.$2);
    final msg = _lastPanic;
    _lastPanic = null;
    if (msg != null) throw BridgePanicException(msg);
    // No hook message means the module did not `panic!` — it trapped or
    // aborted, so nothing ran to record a cause and all we hold is whatever
    // the engine said. For the two shapes that are diagnosable, say what they
    // mean; the engine's own text stays first either way, because a wrong
    // guess must not be able to hide the real message.
    throw BridgePanicException(
      explainTrap(text.substring(marker + _trapMarker.length)),
    );
  }

  /// A fresh view of linear memory. Never cached: memory.grow detaches the
  /// previous ArrayBuffer.
  ///
  /// A `JSObject`, not a `JSArrayBuffer`: a threaded module's memory is a
  /// `SharedArrayBuffer`, which dart2js and DDC refuse to cast to the Dart type
  /// behind `JSArrayBuffer` (dart2wasm does not check). So it is only ever
  /// handed to a JS constructor, which takes either kind.
  JSObject get _buffer => _memory.getProperty<JSObject>('buffer'.toJS);

  /// Copy [len] bytes out of linear memory, JS side to JS side.
  ///
  /// `sublist` on a JS-backed list is `JSArrayBufferImpl.cloneAsDataView` (SDK
  /// `js_typed_array.dart`), one JS statement: `new ArrayBuffer(n)` then
  /// `new Uint8Array(dst).set(src)`. A real memcpy.
  ///
  /// It replaced `Uint8List.fromList(view.toDart)`, which was **not** a memcpy.
  /// `.toDart` is free (a wrapper), but `fromList` allocates a WasmGC list and
  /// calls `setRange`, whose JS-typed-array branch is `copyToWasmI8Array` — a
  /// JS `for` loop making one exported-wasm call per byte, because JS cannot
  /// address the Dart heap. That is a per-byte crossing against a bulk copy,
  /// which is an order-of-magnitude difference on a large response and shows up
  /// in `web_bench_main.dart` section D. The loop survives `-O2` and always
  /// will; binaryen optimizes wasm and that loop is JS.
  ///
  /// The result is therefore **JS-backed**, not a Dart-heap list, and that is a
  /// deliberate trade rather than a free win. Scalar decoding gets faster,
  /// because a JS-backed `ByteData` is a `DataView` and the Dart-heap one
  /// assembles an i64 out of a byte array. So does `readString`: `_Utf8Decoder`
  /// tests `is JSUint8ArrayImpl` *before* `is U8List` and routes to
  /// `TextDecoder`. What gets slower is Dart code iterating the returned bytes
  /// one at a time.
  ///
  /// The trade nets out for that caller rather than costing it — the copy it no
  /// longer pays is worth about what the slower iteration costs — and any
  /// caller doing something *other* than a bytewise Dart loop (decoding a
  /// string, reading scalars, handing the bytes back to JS, writing them to a
  /// socket) wins outright. If that balance is ever in doubt, section E of the
  /// web bench is the reader-side instrument.
  ///
  /// **Invariant: no `BinaryReader` is ever built over a `SharedArrayBuffer`.**
  /// `cloneAsDataView` allocates a plain `ArrayBuffer`, so this holds here by
  /// construction even under the threaded fixture, and the actor path's
  /// response arrives as a transferred plain buffer. It is load-bearing:
  /// `TextDecoder` refuses SAB-backed views (the glue works around it at
  /// `frustrate.js`, `panic`), so a reader over shared memory would silently
  /// drop `readString` onto the per-byte fallback. Going zero-copy on threaded
  /// builds looks tempting because a SAB does not detach on `memory.grow` —
  /// it would break this.
  ///
  /// Copying at all (rather than handing out the view) is also load-bearing:
  /// `_onPost` frees the buffer on the next line, `StreamRouter.deliver` defers
  /// routing to a microtask, and on non-shared memory the next `_alloc` can
  /// `memory.grow` and detach the `ArrayBuffer` outright.
  Uint8List _read(int ptr, int len) {
    if (len == 0) return Uint8List(0);
    if (len < _bulkReadThreshold) {
      final small = _Uint8ArrayView(_buffer, ptr, len).toDart;
      return Uint8List.fromList(small); // per-byte: small payloads only
    }
    return _Uint8ArrayView(_buffer, ptr, len).toDart.sublist(0, len);
  }

  /// Below this, the per-byte loop is genuinely cheaper and `_read` keeps it.
  ///
  /// The two costs have different shapes, so neither wins everywhere.
  /// `Uint8List.fromList` is a flat per-byte rate with almost no fixed cost,
  /// while `sublist` has a much lower per-byte rate behind a fixed overhead —
  /// a JS call plus a fresh `ArrayBuffer` allocation. Fitting the two lines
  /// gives a crossover a little below this constant; 256 is that crossover
  /// rounded to a power of two, not a guess.
  ///
  /// **Re-derive it, do not trust this number**, if the engine or the codec
  /// changes: `tests/dart_integration/bench/web_bench_main.dart` is the
  /// instrument, sections A and B (small-payload rows) against D (1 MiB).
  /// Dropping the threshold and reading `sublist` unconditionally is the
  /// experiment that decides it — and it is not free, because every sync call
  /// reads twice (the out-param block, then the envelope), so the fixed cost
  /// lands on every call regardless of payload and shows up on the
  /// `noArgsNoRet` overhead floor rather than on the payload rows.
  ///
  /// A consequence worth naming: responses on either side of this line get
  /// different backings, so `BinaryReader` must be correct over both. It is,
  /// and `js_backed_reader_test.dart` is what says so — it decodes the same
  /// bytes through both backings and requires every method and every error
  /// message to agree.
  static const int _bulkReadThreshold = 256;

  /// Copy [bytes] into linear memory at [ptr]. The one request-direction byte
  /// crossing of the whole transport, deliberately in one place: `callSync`
  /// writes into a block it shares with the out-params and [_writeBytes]
  /// writes into a block of its own, and `no_per_byte_crossing_test` should
  /// have one line to account for, not one per caller.
  void _writeBytesAt(int ptr, Uint8List bytes) {
    final view = _Uint8ArrayView(_buffer, ptr, bytes.length);
    view.callMethod('set'.toJS, bytes.toJS); // per-byte: request direction
  }

  /// Allocate once and copy each piece in at its own offset.
  ///
  /// The point of the split: `_writeBytesAt` ends in `Uint8Array.set(x.toJS)`,
  /// and `toJS` unwraps a JS-backed list in O(1) while copying a Dart-heap one
  /// a byte at a time (SDK `js_interop_patch.dart`, `_copyFromWasmI8Array`).
  /// Staging a JS-born payload through the Dart-heap writer therefore pays
  /// that loop twice — inward via `setRange`, outward via `toJS`. Keeping the
  /// payload in its own piece lets it reach `set` with its backing intact.
  int _writeBytesPieces(List<Uint8List> pieces, int total) {
    if (total == 0) return 0;
    final ptr = _alloc(total);
    var off = 0;
    for (final piece in pieces) {
      if (piece.isNotEmpty) _writeBytesAt(ptr + off, piece);
      off += piece.length;
    }
    return ptr;
  }

  /// Allocate linear memory and copy [bytes] in. Returns 0 for empty input
  /// (the dispatch treats a null request as empty).
  int _writeBytes(Uint8List bytes) {
    if (bytes.isEmpty) return 0;
    final ptr = _alloc(bytes.length);
    _writeBytesAt(ptr, bytes);
    return ptr;
  }

  int _alloc(int len) => _jsNumber(_call(_allocFn, _jsBigInt(len))!);

  void _free(int ptr, int len, int cap) =>
      _call(_freeFn, ptr.toJS, _jsBigInt(len), _jsBigInt(cap));

  /// Read a u64 lease slot. Handles and buffer pointers fit in wasm32's
  /// 4GiB memory, so the high word must be zero.
  int _u64Slot(ByteData slots, int offset) {
    final lo = slots.getUint32(offset, Endian.little);
    final hi = slots.getUint32(offset + 4, Endian.little);
    if (hi != 0) {
      throw StateError('frustrate: lease slot exceeds wasm32 range');
    }
    return lo;
  }

  /// The `(ptr, len, cap)` triple the overflow path writes at the front of the
  /// response slab. Mirrors `frustrate::envelope::RESP_OUT_MIN_CAP`.
  static const int _leaseTripleBytes = 24;

  /// [frustrateRespSlabBytes] as a JS BigInt, converted once. `BigInt(n)` is a
  /// JS call under dart2wasm and this argument never changes, so converting it
  /// per call is pure tax.
  static final JSAny _slabCapBig = _jsBigInt(frustrateRespSlabBytes);

  /// Run a request encoder against an ordinary heap writer, and hand back the
  /// pieces it produced rather than one contiguous buffer.
  ///
  /// The closure protocol exists for native, where the writer can be the FFI
  /// block itself; web implements the same interface with today's heap buffer
  /// and proceeds unchanged. A linear-memory-backed writer was considered and
  /// rejected on its own merits: every scalar write
  /// would become a JS-interop `DataView` store, and the user's payload starts
  /// on the Dart heap regardless, so the per-byte crossing would move rather
  /// than vanish. This costs web nothing and changes no wasm entry count.
  ///
  /// The writer is still closed on the way out, even though nothing here is
  /// freed. A codec hook that stashes the writer is a bug on both platforms;
  /// closing here means it is *found* on both, in a browser test, instead of
  /// only where it happens to corrupt memory.
  static (List<Uint8List>, int) _encodeRequestPieces(
    int sizeHint,
    void Function(BinaryWriter w) encode,
  ) {
    final w = BinaryWriter(sizeHint);
    try {
      encode(w);
      return (w.takePieces(), w.totalLength);
    } finally {
      w.close();
    }
  }

  @override
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  }) {
    final (reqPieces, reqLen) = _encodeRequestPieces(sizeHint, encode);
    // ONE allocation carries the response slab AND the request: the slab at
    // [base], the request bytes immediately after it. Every wasm entry on this
    // path pays a JS frame before the export even runs, so the entry count is
    // the floor — this is three of them: alloc, the call, free.
    // `call_entry_count_test.dart` is what pins that count.
    //
    // The third entry used to be a fourth: `frustrate_call_sync` leased a
    // fresh Rust `Vec` for the response, which had to be handed back. It now
    // answers into the first [frustrateRespSlabBytes] of this block whenever
    // the envelope fits, and there is nothing to hand back. Only a response
    // too large for the slab still leases, and only that call pays the fourth
    // entry (pinned both ways by `call_entry_count_test.dart`).
    //
    // **This is not a slab reused across calls**, which was rejected on
    // soundness, and the difference
    // is the whole safety argument. That one was a buffer shared *across*
    // calls, which cannot survive reentrancy: the threaded pool's spawn hook
    // and every Rust->Dart callback can start a second bridge call while an
    // outer request is still borrowed. This block belongs to one call, is
    // written by Rust only after the body (and anything the body re-entered)
    // has finished, and is freed by the frame that allocated it. A nested call
    // allocates its own. So there is no new lifetime discipline at all — just
    // as there was none when the out-params were folded in.
    //
    // Two details make the layout safe rather than merely smaller. `base` is
    // the start of an allocation from Rust's own allocator, so the lease
    // triple's alignment is whatever that allocator gives (and `respond_out`
    // writes it bytewise, assuming none). And an empty request points one past
    // the end of the slab — never dereferenced, because `request_slice`
    // short-circuits on `req_len == 0` (the generated dispatch; it treats a
    // null *or* empty request as empty).
    final total = frustrateRespSlabBytes + reqLen;
    final bigTotal = _jsBigInt(total);
    final base = _jsNumber(_call(_allocFn, bigTotal)!);
    try {
      // Piecewise, at the same destination: the entry count is unchanged
      // (these are JS-side copies, not wasm entries — `call_entry_count_test`
      // counts frames through `$frustrateCall`, which this does not use).
      var reqOff = base + frustrateRespSlabBytes;
      for (final piece in reqPieces) {
        if (piece.isNotEmpty) _writeBytesAt(reqOff, piece);
        reqOff += piece.length;
      }
      // `_callInt`, not `_call`: the export returns an i32 and reading it back
      // through `Number()` would spend more than the entry it saves.
      final n = _callInt(
        _callSyncFn,
        fnId.toJS,
        (base + frustrateRespSlabBytes).toJS,
        _jsBigInt(reqLen),
        base.toJS,
        _slabCapBig,
      );
      if (n >= 0) {
        // The common case: the envelope is sitting in our own block.
        return decodeEnvelope(_read(base, n), typedError: typedError);
      }
      // Overflow. The lease triple is at the front of the slab; read it in
      // place rather than copying it out — a `JSDataViewImpl` over linear
      // memory costs one JS call and no allocation. Safe with no new lifetime
      // discipline: the three slots are consumed on the next three lines,
      // inside one synchronous region, before any `_free` or `_alloc`, so
      // nothing can `memory.grow` and detach the buffer underneath, and no
      // reference escapes.
      final slots = _DataView(_buffer, base, _leaseTripleBytes).toDart;
      final respPtr = _u64Slot(slots, 0);
      final respLen = _u64Slot(slots, 8);
      final respCap = _u64Slot(slots, 16);
      final resp = _read(respPtr, respLen);
      _free(respPtr, respLen, respCap);
      return decodeEnvelope(resp, typedError: typedError);
    } finally {
      _call(_freeFn, base.toJS, bigTotal, bigTotal);
    }
  }

  /// `frustrate_call_cancel` (executor.rs) — a runtime export since ABI 4, so
  /// it is present on every module `_checkRuntimeAbi` accepted. Claims one
  /// in-flight `async fn` call out of this instance's executor registry so its
  /// future is dropped; `1` means the claim landed and the Dart future is ours
  /// to fail.
  ///
  /// This instance's registry is the right one on both web builds and for the
  /// same reason `cancelStream` reaches for the main-instance export: a plain
  /// bridged `async fn` runs here (inline on single-threaded, on a pool worker
  /// sharing this linear memory on threaded), never in an actor's worker
  /// instance. A deferred actor completion takes the same token but *not* this
  /// export: its future is parked in the actor's own instance, so the claim
  /// goes over that worker's channel instead (`_WebActorHost._postCancelCall`).
  late final JSFunction _callCancelFn = _export('frustrate_call_cancel');

  bool _callCancel(int callId) =>
      _jsNumber(_call(_callCancelFn, _jsBigInt(callId))!) != 0;

  @override
  Future<BinaryReader> callAsync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    FrustrateCancelToken? cancel,
  }) {
    // The encode and the request write are both inside the guarded region:
    // `_alloc` traps on a memory-growth failure and a user codec hook can
    // throw, and either must reject the future like any other issue-path
    // failure rather than throw out of a Future-returning method.
    return _pending.issue(typedError: typedError, cancel: cancel, (callId) {
      // Threaded web only: with no pool worker left, nothing will ever run
      // this. Inside the closure so it rejects the future rather than throwing
      // out of a Future-returning method, and ahead of the encode so a dead
      // pool costs no work at all.
      final poolFailure = _poolWorkers.failure;
      if (poolFailure != null) throw poolFailure;
      final (reqPieces, reqTotal) = _encodeRequestPieces(sizeHint, encode);
      final reqPtr = _writeBytesPieces(reqPieces, reqTotal);
      try {
        // A single-threaded module runs the body inline (see library docs):
        // the frustrate.post import fires during this call. A threaded module
        // queues the job to a worker thread; the completion relays back
        // through the pool pump.
        _call(
          _callAsyncFn,
          fnId.toJS,
          reqPtr.toJS,
          _jsBigInt(reqTotal),
          _jsBigInt(callId),
        );
      } finally {
        if (reqTotal != 0) _free(reqPtr, reqTotal, reqTotal);
      }
    });
  }

  /// The cooperative executor's drain entry (`frustrate_drain`). Present on
  /// every build; only single-threaded modules call it (via the microtask the
  /// `schedule_drain` import queues).
  ///
  /// The "only" is load-bearing rather than descriptive. On a threaded module
  /// the executor routes its drains to the pool instead (`arrange_drain` in
  /// executor.rs), which is what places a `#[bridge(no_block)]` `async fn`'s
  /// polls — and the drop of a cancelled future — off this isolate. Calling
  /// this export on a threaded instance would poll those futures here, on the
  /// main thread, in a module that contains `memory.atomic.wait32`. An actor's
  /// instance drains through its own worker's copy of this export, never
  /// through this one.
  late final JSFunction _drainFn = _export('frustrate_drain');

  /// The call id `frustrate_drain` was polling when it trapped (executor.rs);
  /// `_drainExecutor` reads it to attribute a mid-poll trap to one future.
  /// Present on every build.
  late final JSFunction _currentDrainCallFn = _export(
    'frustrate_current_drain_call',
  );

  /// Poll ready executor tasks. Runs on a microtask scheduled by the
  /// `schedule_drain` import, so a Rust `async fn` that returned Pending
  /// (having yielded to the event loop) resumes here once its waker fired.
  /// Goes through the JS-owned frame so a poll that traps (panic=abort) is
  /// caught rather than escaping the microtask.
  void _drainExecutor() {
    try {
      _call(_drainFn);
    } on BridgePanicException catch (e) {
      // An async-fn body trapped mid-poll. Under panic=abort the executor's
      // catch_unwind never ran, so it never posted the panic envelope. Recover
      // the call id it recorded before the trapping poll and reject exactly
      // that future with the same BridgePanicException the completion path
      // would have carried (no second panic path). Other ready tasks each have
      // their own scheduled microtask, so the run queue keeps draining; the
      // trapped task's Rust registry entry is abandoned (poisoned — no
      // destructors ran), matching the trap stance elsewhere.
      final callId = _jsNumber(_call(_currentDrainCallFn)!);
      // No attributable pending call (id 0, or an already-settled id): a
      // trap in executor plumbing rather than a user poll — surface it
      // loudly instead of swallowing a real trap.
      if (!_pending.fail(callId, e, StackTrace.current)) rethrow;
    }
  }

  void _onPost(int callId, int ptr, int len, int cap) {
    final bytes = _read(ptr, len);
    _free(ptr, len, cap);
    // Stream events route by id (single-threaded modules post them inline
    // during the producing call; threaded modules relay them from pool
    // workers — same path).
    if (_router.deliver(callId, bytes)) return;
    // Outside the assert: asserts are stripped in release, and this settles
    // the future.
    final settled = _pending.complete(callId, bytes);
    // A completion for a call we no longer know about would indicate a
    // bridge bug (call ids are never reused within a page).
    assert(settled, 'frustrate: completion for unknown call id $callId');
  }

  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) => _router.openObject(
    methods,
    onError,
    onDone,
    label: label,
    reclaim: reclaim,
  );

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) => _router.open(
    onItem,
    onError,
    onDone,
    label: label,
    reclaim: reclaim == null || reclaim.isEmpty ? null : reclaim[0],
  );

  // Lazy: a bridge crate with no stream members has no generated reference
  // to frustrate::stream, so the export may be absent — and never needed.
  late final JSFunction _streamCancelFn = _export('frustrate_stream_cancel');
  late final JSFunction _streamPauseFn = _export('frustrate_stream_pause');
  late final JSFunction _streamResumeFn = _export('frustrate_stream_resume');

  @override
  void cancelStream(int id) {
    _router.cancelLocal(id);
    // An actor-owned id's producer runs in that worker's instance, whose
    // cancel registry is a different linear memory; signal it through the
    // worker's pump instead of this instance's export.
    final host = _actorOwner[id];
    if (host != null) {
      host._postCancel(id);
      return;
    }
    // Single-threaded: the producer only ever runs during a call on this
    // thread, so the flag is simply seen at its next run (a stored sink's
    // next push). Threaded: the registry lives in the shared linear
    // memory — pool workers observe it immediately.
    _call(_streamCancelFn, _jsBigInt(id));
  }

  // Backpressure on a main-instance stream: flip the flag in this instance's
  // registry. Threaded producers observe it immediately; a single-threaded
  // inline producer runs during the opening call and cannot be reached
  // mid-run, so `send()` there simply never parks.
  @override
  void pauseStream(int id) => _call(_streamPauseFn, _jsBigInt(id));

  @override
  void resumeStream(int id) => _call(_streamResumeFn, _jsBigInt(id));

  @override
  void failStream(int id, Object error, StackTrace st) =>
      _router.fail(id, error, st);

  // Void callbacks (DartCallback) register through openStream; returning
  // callbacks (DartFunction) through openFunction. Both are portable: the
  // awaited `call_async` path parks a future on the executor, so the router
  // answers the invocation through the wasm export (see `_respondToCallback`).
  @override
  int openFunction(
    BinaryWriter Function(BinaryReader) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  }) => _router.openFunction(
    onInvoke,
    label: label,
    onDeclaredError: onDeclaredError,
    reclaim: reclaim,
  );

  // Lazy: only a bridge crate with a DartFunction member references the
  // callback machinery, so the export exists exactly then (and never needed
  // otherwise) — mirroring the native transport's lazy lookup.
  late final JSFunction _callbackRespondFn = _export(
    'frustrate_callback_respond',
  );

  /// Answer one returning-callback invocation: hand the encoded response to
  /// the wasm `frustrate_callback_respond` export, which fills the parked
  /// `CallFuture`'s slot and wakes it (single-threaded: the response is the
  /// microtask that lets the awaiting `async fn` resume). Rust copies the
  /// buffer during the call, so the lease ends when it returns.
  void _respondToCallback(int invocationId, Uint8List response) {
    final ptr = _writeBytes(response);
    try {
      _call(
        _callbackRespondFn,
        _jsBigInt(invocationId),
        ptr.toJS,
        _jsBigInt(response.length),
      );
    } finally {
      if (response.isNotEmpty) _free(ptr, response.length, response.length);
    }
  }

  /// Refuse a module built against a different frustrate runtime, before
  /// anything calls into it.
  ///
  /// The native transport has done this since ABI 2; web had nothing.
  /// `frustrate_schema_hash` is not a substitute — it fingerprints the
  /// *interface* a bridge exposes, so a module whose `#[bridge]` items are
  /// unchanged matches it exactly no matter how old the runtime underneath is.
  /// The export whose shape moved at ABI 3 (`frustrate_call_sync`) is on the
  /// hot path of every sync call and takes raw pointers, so calling an old one
  /// with the new argument list would put the response capacity where a
  /// pointer used to be and write the answer over the request. Loud here, or
  /// silent corruption there.
  ///
  /// A module too old to have the export at all fails inside [_export] with
  /// its own named error, which is the same verdict by a different route.
  void _checkRuntimeAbi() {
    // A u64 export: same full-precision decimal-string route checkSchemaHash
    // takes, for the same reason (Number() truncates past 2^53).
    final raw = _call(_export('frustrate_runtime_abi'));
    final abi = BigInt.parse(_jsString(raw).toDart).toUnsigned(64);
    if (abi != BigInt.from(frustrateRuntimeAbi)) {
      throw StateError(
        'frustrate: runtime ABI mismatch — the wasm module speaks ABI $abi, '
        'this package speaks $frustrateRuntimeAbi. Rebuild the bridge crate '
        'and regenerate its bindings.',
      );
    }
  }

  @override
  void checkSchemaHash(BigInt expected) {
    // The i64 export return arrives as a JS BigInt; read it at full precision
    // through its decimal string, then compare as unsigned 64-bit (the wasm
    // integration may sign-extend the i64 — normalizing both to unsigned
    // makes them agree).
    final result = _call(_export('frustrate_schema_hash'));
    final native = BigInt.parse(_jsString(result).toDart).toUnsigned(64);
    final want = expected.toUnsigned(64);
    if (native != want) {
      throw StateError(
        'frustrate: generated Dart bindings are stale — regenerate '
        '(schema 0x${want.toRadixString(16)} != '
        'native 0x${native.toRadixString(16)})',
      );
    }
  }

  /// One [HandleDrop] per drop symbol, for the life of the page's transport.
  ///
  /// Memoized because the hooks have to *stay alive*, not merely to save an
  /// export lookup: generated classes read this at every mint, and each
  /// `_WebHandleDrop` owns the [Finalizer] that backs the GC path. A fresh one
  /// per handle would be collected along with the handle it was meant to
  /// outlive.
  final Map<String, HandleDrop> _drops = {};

  @override
  HandleDrop handleDrop(String symbol) =>
      _drops[symbol] ??= _WebHandleDrop(this, _export(symbol));

  /// The same hook. A resident needs its `Drop` to run on the thread that
  /// built the object, and on web there is only ever one: a wasm instance is
  /// single-threaded, and a web worker is a *separate* instance with separate
  /// memory that no handle can reach. [_WebHandleDrop] is already backed by a
  /// `dart:core` [Finalizer] rather than a `NativeFinalizer`, which is the
  /// property native has to arrange for and this already has.
  @override
  HandleDrop residentHandleDrop(String symbol) => handleDrop(symbol);

  /// Nothing can be lost here: web has no isolates to exit, and every resident
  /// touch is on the one Dart thread (see `frustrate::resident`).
  @override
  int get residentLeakCount => 0;

  @override
  Future<ActorHost> spawnActorHost({String? debugName}) async {
    final host = _WebActorHost(this, debugName);
    await host._readyC.future;
    return host;
  }
}

// ------------------------------------------------------------- workers --
// Both worker pumps (the threaded pool pump and the actor pump) live in the
// glue script's worker branch — one file, dispatching on the message type.
// See lib/src/js/frustrate.js for the pump sources and their contracts.

@JS('Blob')
extension type _Blob._(JSObject _) implements JSObject {
  external _Blob(JSArray<JSAny?> parts, JSObject options);
}

@JS('URL.createObjectURL')
external JSString _createObjectUrl(JSObject blob);

@JS('Worker')
extension type _JSWorker._(JSObject _) implements JSObject {
  external _JSWorker(JSString url);
  external void postMessage(JSAny? message, JSArray<JSAny?> transfer);
  external void terminate();
  external set onmessage(JSFunction f);
  external set onerror(JSFunction f);
}

/// Where the page's live frustrate Workers are recorded — pool workers and
/// actor hosts alike — so that a Dart heap which never met them can still end
/// them.
///
/// A Worker belongs to the *document*, not to the Dart heap that constructed
/// it. Flutter web hot restart resets the heap and re-runs `main()` without
/// unloading the document: the previous [WebRuntime], its [PoolWorkers] set,
/// every `_WebActorHost`, and every Dart-side handle to their Workers are all
/// simply gone — while the Workers keep running, each pool worker parked in
/// `frustrate_worker_entry` on a shared memory nothing can address any more.
///
/// Which is why a `dispose()` would not have been a fix. No Dart teardown of
/// any kind runs on hot restart — not `State.dispose`, not a finalizer, not a
/// zone callback — so there is nobody left to call one, and nothing on the Dart
/// side to call it *on*. The registry has to live where the Workers do: on
/// `globalThis`, which is already load-bearing across exactly this boundary
/// ([_installGlue] returns early when `$frustrateCall` survives from a previous
/// life, and `$frustrateGlueUrl` is read the same way).
///
/// Page *close* needs nothing here: a worker's owner is the document, and
/// destroying the document terminates every worker it owns. Hot restart is the
/// case that recurs — once per developer per restart, a full pool width each
/// time.
const String _workerRegistryKey = r'$frustrateWorkers';

JSArray<JSObject> _workerRegistry() {
  final existing = globalContext.getProperty<JSAny?>(_workerRegistryKey.toJS);
  if (existing != null && existing.isA<JSArray<JSAny?>>()) {
    return existing as JSArray<JSObject>;
  }
  final fresh = JSArray<JSObject>();
  globalContext.setProperty(_workerRegistryKey.toJS, fresh);
  return fresh;
}

/// Terminate every Worker a previous Dart heap left registered. Detaches the
/// list first, so a registration racing the sweep joins the new list rather
/// than being terminated by it.
///
/// **Load-bearing on the development loop — measured 2026-08-03.** This exists
/// for a Dart heap reset that leaves the document standing, which is exactly
/// what a Flutter web hot restart does. Under `flutter run -d chrome` (DDC),
/// with a real bridge and a live actor, the registry stays at one Worker
/// across three consecutive restarts; with this call disabled it grows 1 → 2 →
/// 3, one leaked Worker per restart, each still holding whatever its actor
/// owned.
///
/// It is not reachable on the *release* target, and that has not changed:
/// `flutter run --wasm -d chrome` offers no hot restart at all — its key
/// commands are `h/d/c/q`, with no `r` or `R`, and no VM service, because
/// flutter_tools gates the service protocol on `!debuggingOptions.webUseWasm`
/// (`resident_web_runner.dart`, "Only non-wasm debug builds of the web support
/// the service protocol"). So this is the JS-number backend's mechanism, in a
/// file whose release compiler cannot exercise it — which is precisely why it
/// has an explicit test (`hot_restart_workers_test.dart`) that manufactures
/// the JS-observable state a restart leaves behind, rather than relying on a
/// restart it cannot perform.
void _reapOrphanedWorkers() {
  final orphans = _workerRegistry().callMethod<JSArray<JSObject>>(
    'splice'.toJS,
    0.toJS,
  );
  for (final worker in orphans.toDart) {
    _JSWorker._(worker).terminate();
  }
}

/// Terminate [worker] and forget it. The single place either half happens, so
/// a live worker can never be dropped from the registry and a dead one can
/// never be left in it (a stale entry would be terminated again by the next
/// heap, and the list would grow one entry per trap-replenishment forever).
void _terminateWorker(_JSWorker worker) {
  worker.terminate();
  final registry = _workerRegistry();
  final registryIndex = registry
      .callMethod<JSNumber>('indexOf'.toJS, worker)
      .toDartInt;
  if (registryIndex >= 0) {
    registry.callMethod('splice'.toJS, registryIndex.toJS, 1.toJS);
  }
}

String? _glueBlobUrlCache;

/// Where workers come from: the served glue when the page has one (the
/// strict-CSP path — blob: workers are blocked there), else a blob: URL of
/// the embedded copy. Read per spawn, so a manually set `$frustrateGlueUrl`
/// takes effect from the next worker on.
String _workerScriptUrl() {
  if (_servedGlueUrl() case final url?) return url;
  if (_glueBlobUrlCache case final cached?) return cached;
  final options = JSObject()..setProperty('type'.toJS, 'text/javascript'.toJS);
  final blob = _Blob(
    [frustrateGlueSource.toJS].toJS as JSArray<JSAny?>,
    options,
  );
  return _glueBlobUrlCache = _createObjectUrl(blob).toDart;
}

/// Create a worker running the glue's worker branch. A synchronous throw
/// here is CSP blocking blob: workers (a DOM SecurityError) — convert it to
/// the attributable error naming the fix.
_JSWorker _spawnGlueWorker() {
  final url = _workerScriptUrl();
  final _JSWorker worker;
  try {
    worker = _JSWorker(url.toJS);
  } on Object catch (e) {
    throw StateError(
      'frustrate: creating a worker from $url failed ($e) — is CSP '
      'blocking blob: workers? Serve the glue as a static asset and load '
      'it with <script src="frustrate.js"></script> before '
      'FrustrateWeb.init so workers come from a same-origin URL. Bazel: '
      'frustrate_web_glue.',
    );
  }
  // Registered at birth, in the one place a frustrate Worker is born, so no
  // creation path can escape the page registry — including one that abandons
  // the worker before its owner has recorded it anywhere.
  _workerRegistry().callMethod('push'.toJS, worker);
  return worker;
}

// ----------------------------------------------------------- actor hosts --

/// One Worker hosting one wasm instance — the actor's executor. The worker
/// runs the glue script (served URL under strict CSP, blob: otherwise); its
/// pump's JS frame catches traps directly — no `$frustrateCall` needed off
/// the main thread.
final class _WebActorHost implements ActorHost {
  /// The page runtime, for the shared id sequence and the shared router.
  /// Ids this host mints must not collide with any other channel's, since
  /// one router routes them all (see [WebRuntime._nextCallId]).
  final WebRuntime _rt;
  final _JSWorker _worker;
  final Completer<void> _readyC = Completer();

  /// Calls in flight *on this host's worker*, drawing from the page-wide id
  /// sequence. Deliberately per-host rather than shared: only this worker's
  /// pump completes these, and keeping them here preserves the association
  /// between a call and the worker running it — which [shutdown] needs, and
  /// which a page-wide map would erase.
  late final PendingCalls _pending = PendingCalls(
    _rt._allocId,
    cancelCall: _postCancelCall,
    tally: _rt._inFlight,
  );
  bool _stopped = false;

  /// The actor's type name (generated constructors pass it), so
  /// host-attributed errors — the disposed-deferred StateError above all —
  /// name the actor rather than an anonymous worker.
  final String? _debugName;

  /// Deferred calls in flight on this worker (`ActorHost.call(deferred:)`).
  /// Added at issue, removed when the future settles. These are the one kind
  /// of call [shutdown] may legitimately find still pending — their
  /// completions are detached from the dispatch turn — and the dispose
  /// contract is theirs: terminate() destroys their futures with the
  /// instance, and each is failed with the named disposed-deferred error.
  final Set<int> _deferredInFlight = {};

  @override
  Object get bridgeIdentity => _rt.bridgeIdentity;

  _WebActorHost(this._rt, this._debugName) : _worker = _spawnGlueWorker() {
    _worker.onmessage = ((JSObject e) {
      _onMessage(e.getProperty<JSObject>('data'.toJS));
    }).toJS;
    // The worker never ran its pump (script 404 / parse failure): it cannot
    // post initError itself, so fail the pending spawn from here.
    _worker.onerror = (() {
      _failedToStart(
        StateError(
          'frustrate: the actor worker script failed to load from '
          '${_workerScriptUrl()} — check \$frustrateGlueUrl',
        ),
      );
    }).toJS;
    final init = _rt._moduleInitMessage('init')
      ..setProperty('memInitial'.toJS, _sharedMemoryInitialPages.toJS)
      ..setProperty('memMaximum'.toJS, _sharedMemoryMaxPages.toJS);
    _worker.postMessage(init, JSArray());
  }

  /// This host never came up. Terminate the worker before failing the spawn:
  /// [spawnActorHost] rethrows, so the host object becomes unreachable and
  /// nothing will ever terminate it afterwards — one leaked Worker per failed
  /// spawn, held for the page's lifetime. Both entry paths (a script that
  /// never loaded, an instantiation that failed) end here, matching the pool,
  /// which already terminated on both of its own.
  /// No-op once the host is up: `onerror` also fires for a *later* worker
  /// error, and a host that already served calls is not a failed spawn.
  void _failedToStart(StateError error) {
    if (_readyC.isCompleted) return;
    _stopped = true;
    _terminateWorker(_worker);
    _readyC.completeError(error);
  }

  void _onMessage(JSObject m) {
    final type = m.getProperty<JSString>('type'.toJS).toDart;
    switch (type) {
      case 'ready':
        _readyC.complete();
      case 'initError':
        _failedToStart(
          StateError(
            'frustrate: actor worker failed to initialize: '
            '${m.getProperty<JSString>('message'.toJS).toDart}',
          ),
        );
      case 'resp':
        final callId = m.getProperty<JSNumber>('callId'.toJS).toDartInt;
        final bytes = m
            .getProperty<JSArrayBuffer>('buf'.toJS)
            .toDart
            .asUint8List();
        // Stream events from this actor's instance relay through the same
        // pump; route by id before the completer path. The router is the
        // page's single one — ids are globally unique, so an event for this
        // worker's handle can never be confused with another channel's.
        if (_rt._router.deliver(callId, bytes)) return;
        // Outside the assert: asserts are stripped in release, and this
        // settles the future.
        final settled = _pending.complete(callId, bytes);
        // A completion for a call [shutdown] already failed: the worker
        // posted it before terminate() and it sat in this isolate's event
        // queue. The future has its answer (the disposed-deferred
        // StateError), so the late payload is dropped — deliberately, not
        // silently-wrongly: the answer-once here is Dart's, mirroring the
        // executor's gate on native.
        if (!settled && _stopped) return;
        assert(settled, 'frustrate: completion for unknown actor call $callId');
      case 'cancelled':
        // The worker's answer to a 'cancel-call' — the second half of the
        // claim protocol, which on this platform is a round trip because the
        // executor holding the future is in the worker's own linear memory
        // (see [_postCancelCall]).
        final callId = m.getProperty<JSNumber>('callId'.toJS).toDartInt;
        if (!m.getProperty<JSBoolean>('claimed'.toJS).toDart) {
          // Not claimed: the completion already happened, and postMessage is
          // FIFO in this direction too — so its 'resp' was posted before this
          // reply and has already settled the future above. Nothing to do,
          // which is exactly the "a cancel that arrives too late does nothing"
          // half of the contract.
          return;
        }
        // Claimed: no 'resp' will ever come for this id. A false answer means
        // the call is no longer in flight here — shutdown() got to it first
        // with the disposed-deferred error, which is an answer, so the cancel
        // is late rather than lost.
        _pending.failCancelled(callId);
      case 'trap':
        final callId = m.getProperty<JSNumber>('callId'.toJS).toDartInt;
        final message = m.getProperty<JSString>('message'.toJS).toDart;
        // After a trap no destructors ran on the worker instance; the host
        // is degraded but still attributable.
        _pending.fail(
          callId,
          BridgePanicException(message),
          StackTrace.current,
        );
    }
  }

  /// Ask this host's worker to claim [callId] out of *its* executor — the
  /// `FrustrateCancelToken` route for a deferred actor completion.
  ///
  /// Always returns false, which is [PendingCalls]'s "not claimed in this
  /// frame": the claim cannot be answered here. An actor's future is parked on
  /// a cooperative executor inside the worker's own wasm instance, so only the
  /// worker can run `frustrate_call_cancel` against it, and only it knows the
  /// answer. It posts that answer back as 'cancelled', and [_onMessage] fails
  /// the future there.
  ///
  /// **The round trip buys exactness, not just reachability.** Claiming
  /// optimistically here and tombstoning a late response would settle the
  /// future as cancelled even when the answer was already computed and sitting
  /// in this isolate's event queue — a different outcome from native for the
  /// same race, on a contract the token documents. Going through the worker
  /// makes the two platforms agree by riding FIFO in both directions: a
  /// response posted before the cancel was processed arrives before the
  /// 'cancelled' reply and settles the future first, and a claim that succeeds
  /// guarantees no response was ever posted.
  ///
  /// The cost is one round trip of latency, inside the physics already
  /// documented for this channel — an actor observes a signal only once its
  /// executor goes idle, exactly as `cancelStream` does.
  bool _postCancelCall(int callId) {
    // The worker is gone; shutdown() owns failing whatever was outstanding
    // (with the disposed-deferred error), and no reply can arrive.
    //
    // Cancel-then-dispose does not land here, and does not settle as
    // disposed-instead-of-cancelled either: the generated `dispose()` awaits
    // its drop call, which the worker cannot answer until it has drained the
    // 'cancel-call' queued ahead of it, and that reply is FIFO ahead of the
    // drop's response. So by the time `shutdown()` runs, a cancelled call has
    // already been settled as cancelled — the same order native gets from a
    // synchronous claim. Pinned by cancel_deferred_test's cancel-then-dispose
    // case.
    if (_stopped) return false;
    final msg = JSObject()
      ..setProperty('type'.toJS, 'cancel-call'.toJS)
      ..setProperty('callId'.toJS, callId.toJS);
    _worker.postMessage(msg, JSArray());
    return false;
  }

  /// The shut-down refusal is *inside* the issue closure, not ahead of it, so
  /// it rejects the returned future instead of throwing synchronously out of a
  /// Future-returning method — the same StateError `_NativeActorHost.call`
  /// raises for the same mistake.
  @override
  Future<BinaryReader> call(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    bool deferred = false,
    FrustrateCancelToken? cancel,
  }) {
    int? issued;
    final future = _pending.issue(typedError: typedError, cancel: cancel, (
      callId,
    ) {
      if (_stopped) {
        throw StateError('frustrate: actor host was shut down');
      }
      issued = callId;
      // Encoded here, inside the guarded region, so a throwing codec hook
      // rejects the future instead of escaping a Future-returning method —
      // and after the shut-down check, so a refused call does no encoding.
      final (reqPieces, reqLen) = WebRuntime._encodeRequestPieces(
        sizeHint,
        encode,
      );
      // Assemble into a buffer WE allocated, then transfer that.
      //
      // Ownership is the whole argument. `postMessage` detaches what it
      // transfers, so it may only ever be handed a buffer Dart does not still
      // see. A piece may be the caller's own JS-backed list — `toJS` unwraps
      // those rather than copying — so transferring a piece directly would
      // detach the caller's data out from under them. Copying each piece into
      // a buffer of our own keeps that impossible by construction rather than
      // by discipline.
      //
      // It is still one copy per payload, not two: `set` takes a JS-backed
      // piece as a memcpy, where staging through the writer's Dart-heap buffer
      // would have crossed the wasm boundary once per byte in each direction.
      //
      // Allocated at exactly the request's size, so the buffer *is* the
      // request and the worker reads it whole — no offset or length to post
      // beside it.
      final jsBytes = _JSBytes(reqLen);
      var pieceOff = 0;
      for (final piece in reqPieces) {
        if (piece.isNotEmpty) {
          final jsPiece = piece.toJS; // per-byte: Dart-heap piece only
          jsBytes.callMethod('set'.toJS, jsPiece, pieceOff.toJS);
        }
        pieceOff += piece.length;
      }
      final buffer = jsBytes.getProperty<JSArrayBuffer>('buffer'.toJS);
      final msg = JSObject()
        ..setProperty('type'.toJS, 'call'.toJS)
        ..setProperty('callId'.toJS, callId.toJS)
        ..setProperty('fnId'.toJS, fnId.toJS)
        ..setProperty('buf'.toJS, buffer);
      _worker.postMessage(msg, [buffer as JSAny?].toJS);
    });
    // Mirror of `_NativeActorHost.call`: track deferred ids for the dispose
    // contract; the send closure ran synchronously inside issue.
    final id = issued;
    if (deferred && id != null) {
      _deferredInFlight.add(id);
      // .ignore(), not unawaited(): whenComplete's derived future re-raises
      // the call's own error, which the caller handles on the ORIGINAL
      // future — the derived one must swallow it or every failed deferred
      // call double-reports as an unhandled async error.
      future.whenComplete(() => _deferredInFlight.remove(id)).ignore();
    }
    return future;
  }

  /// Router entries this host opened that are still live. Needed because the
  /// router is now page-wide: a terminated worker's entries used to be
  /// collected along with its private router, but in a page-lifetime map they
  /// would leak (and their consumers hang) unless [shutdown] retires them.
  /// Kept accurate by retiring on every exit — terminal, cancel, or shutdown.
  final Set<int> _openStreams = {};

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) => openObject(
    [onItem],
    onError,
    onDone,
    label: label,
    reclaim: reclaim == null || reclaim.isEmpty ? null : [reclaim[0]],
  );

  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    late final int id;
    // `openObject` only allocates and stores; it never runs a handler, so
    // `id` is assigned before either closure can observe it.
    id = _rt._router.openObject(
      methods,
      (e, st) {
        _retire(id);
        onError(e, st);
      },
      () {
        _retire(id);
        onDone();
      },
      reclaim: reclaim,
    );
    _openStreams.add(id);
    _rt._actorOwner[id] = this;
    return id;
  }

  /// Drop this host's bookkeeping for [id]. Both collections are retired
  /// together at every exit, so neither can outlive the registration.
  void _retire(int id) {
    _openStreams.remove(id);
    _rt._actorOwner.remove(id);
  }

  @override
  void cancelStream(int id) {
    // Tombstone page-wide (delivery stops now, whichever channel the event
    // arrives on), but signal *this* worker: each actor worker runs its own
    // wasm instance, so the producer's cancel registry lives in that
    // instance's memory and nowhere else.
    _rt._router.cancelLocal(id);
    _postCancel(id);
  }

  /// Signal the cancel to this host's worker and retire the id. Shared by the
  /// consumer's `onCancel` ([cancelStream]) and the router's throw policy,
  /// which reaches here through `WebRuntime.cancelStream` — both must reach
  /// *this* instance's registry, not the main one's.
  ///
  /// A tombstoned entry leaves `_streams` without running either handler, so
  /// retire it here rather than waiting for a terminal that will not come.
  void _postCancel(int id) {
    _retire(id);
    if (_stopped) return;
    final msg = JSObject()
      ..setProperty('type'.toJS, 'cancel'.toJS)
      ..setProperty('streamId'.toJS, id.toJS);
    _worker.postMessage(msg, JSArray());
  }

  // Backpressure routes to *this* worker's instance, like cancel — no
  // tombstone (the stream stays open). Worker-instance physics still apply: a
  // producer mid-method observes the message only once its executor goes idle,
  // so an async actor producer with `.await` points sees pause/resume while a
  // never-yielding one does not — advisory, exactly as cancel is. pause and
  // resume ride the same FIFO channel, so a delivered pause implies its later
  // resume delivers too — no lost-resume deadlock.
  @override
  void pauseStream(int id) => _postStreamSignal(id, 'pause');

  @override
  void resumeStream(int id) => _postStreamSignal(id, 'resume');

  void _postStreamSignal(int id, String type) {
    if (_stopped) return;
    final msg = JSObject()
      ..setProperty('type'.toJS, type.toJS)
      ..setProperty('streamId'.toJS, id.toJS);
    _worker.postMessage(msg, JSArray());
  }

  @override
  void failStream(int id, Object error, StackTrace st) =>
      _rt._router.fail(id, error, st);

  // Load-bearing that this still throws rather than forwarding to the now-
  // shared `_rt._router.openFunction`: the router answers an invocation
  // through the *main* instance's `frustrate_callback_respond`, but an actor's
  // parked `CallFuture` lives in its worker's own wasm instance and memory.
  // Responding on the main instance would fill an unrelated slot in a
  // different linear memory — silent corruption, not a missing feature.
  @override
  int openFunction(
    BinaryWriter Function(BinaryReader) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  }) => throw StateError(
    'frustrate: returning callbacks (DartFunction) are native-only; '
    'the web surface cannot contain a member that takes one',
  );

  @override
  Future<void> shutdown() async {
    _stopped = true;
    // Native drains its FIFO to a Stop marker; this kills the worker outright.
    // That is not the divergence it looks like, and the assert is the standing
    // proof: a non-deferred member's dispatch arm runs the body and posts the
    // response in one statement — a dequeued call is a finished call.
    // postMessage is FIFO both ways, and the generated `dispose()` clears its
    // handle before issuing the drop call, so nothing joins the queue behind
    // it. Every earlier non-deferred call has therefore answered by the time
    // we get here.
    //
    // Deferred calls are the one legitimate exception, by design: their
    // completions are detached from the dispatch turn, so they may still be
    // pending here — and the dispose contract is theirs alone. terminate()
    // destroys their futures with the instance's memory (nothing Rust-side
    // runs, not even `Drop`), and each is failed with the identical named
    // StateError native throws for the same fact.
    // Pinned across all three fixtures by
    // //tests/dart_integration:actor_shutdown_drain_test (non-deferred drain)
    // and deferred_test's dispose fence (the cancellation).
    assert(
      _pending.ids.every(_deferredInFlight.contains),
      'frustrate: the actor worker was terminated with non-deferred calls '
      'still in flight — generated code cannot reach this, so it is a '
      'bridge bug',
    );
    final disposed = disposedDeferredError(_debugName);
    final st = StackTrace.current;
    for (final id in _deferredInFlight.toList()) {
      _pending.fail(id, disposed, st);
    }
    _deferredInFlight.clear();
    _teardown('the actor host was shut down before this completed');
  }

  /// Kill the worker and retire everything that was routed to it.
  ///
  /// Shared by [shutdown] and the GC reaper, which differ only in what they
  /// may assume on the way in — [shutdown] can assert the queue drained, a
  /// collected handle cannot (see [_reapFromGc]) — and not at all in what the
  /// teardown itself has to do.
  void _teardown(String reason) {
    // The termination itself. A teardown that skips this leaks one Worker per
    // actor for the life of the page — at `ActorPool`'s default width, one per
    // core per pool. Pinned by test/actor_worker_teardown_test.dart
    // (browser-only — it needs real Workers), which measures the worker's
    // liveness rather than counting terminate() calls: this line went missing
    // for four days under a comment that said it was here, and a call counter
    // would not have noticed.
    _terminateWorker(_worker);
    // The backstop for when shutdown's assert is wrong, and the ordinary case
    // for a GC reap. Retiring here keeps the page-wide router and this map from
    // accumulating dead entries, and turns what would be a silent forever-hang
    // into an attributable error. And it must follow the terminate(): the
    // worker is what would otherwise still be able to post against the entries
    // being retired. `_openStreams` is the reachable half: a sink clone stored
    // in the instance's own globals outlives the actor's drop, and dies with
    // the worker's memory (native's equivalent clone lives in shared process
    // memory and survives — an asymmetry of per-instance memory, not of
    // terminate).
    final error = StateError('frustrate: $reason');
    final st = StackTrace.current;
    for (final id in _openStreams.toList()) {
      _rt._actorOwner.remove(id);
      _rt._router.fail(id, error, st);
    }
    _openStreams.clear();
    _pending.failAll(error, st);
  }

  /// The handle was collected without `dispose()`. Best-effort by
  /// construction: `dart:core`'s [Finalizer] promises only that a callback
  /// *may* run, which is the whole difference between this and native's
  /// `NativeFinalizer` (see `ActorHost.attachReaper`).
  ///
  /// **Not `shutdown()`**, for one specific reason: its drain assert does not
  /// hold here. That assert is justified by the *dispose* path — the generated
  /// `dispose()` clears its handle before issuing the drop call, so nothing
  /// joins the queue behind it. A collected handle made no such promise: a
  /// caller can still hold a Future for an in-flight call while the handle
  /// that issued it is unreachable, so `_pending` may legitimately be
  /// non-empty. Asserting there would turn a leak this exists to fix into a
  /// debug-mode crash.
  void _reapFromGc() {
    if (_stopped) return;
    _stopped = true;
    _teardown('the actor was garbage collected without dispose()');
  }

  /// One finalizer for every actor host on the page. Static so it stays
  /// reachable, and so a host cannot be kept alive by the very thing that is
  /// supposed to notice it died: the callback receives `this` as its token,
  /// which is safe only because `Finalizer` holds the token weakly with
  /// respect to the *attached* object, never the other way round.
  static final Finalizer<_WebActorHost> _reaper = Finalizer<_WebActorHost>(
    (host) => host._reapFromGc(),
  );

  @override
  void attachReaper(Object owner) => _reaper.attach(owner, this, detach: owner);

  @override
  void detachReaper(Object owner) => _reaper.detach(owner);
}

final class _WebHandleDrop implements HandleDrop {
  final WebRuntime _runtime;
  final JSFunction _dropFn;
  late final Finalizer<int> _finalizer = Finalizer(drop);

  _WebHandleDrop(this._runtime, this._dropFn);

  @override
  void attach(Object owner, int raw) =>
      _finalizer.attach(owner, raw, detach: owner);

  @override
  void detach(Object owner) => _finalizer.detach(owner);

  @override
  void drop(int raw) => _runtime._call(_dropFn, raw.toJS);
}
