/// Platform-shared state and contract for opaque handles. Not part of the
/// public package surface; the public OpaqueHandle is the per-platform
/// subclass (conditional export in frustrate.dart).
library;

import 'consumed.dart';
import 'runtime_core.dart';

/// A Dart-owned handle to a Rust object. Generated subclasses provide the
/// per-type [HandleDrop] (looked up from the generated `frustrate_drop_<Type>`
/// symbol).
///
/// **Two fields, and that is the whole object.** A raw is a value one
/// transport handed out, meaningful only to that transport, and sending it to
/// another is not something the wire can detect on arrival — it carries a bare
/// `u64`. So which bridge minted a handle has to be known on this side. It is,
/// but not as a third field: the stamp and the comparison are both inside
/// `assert`s ([stampHandleBridge], [checkHandleBridge]), so a build with
/// asserts off carries neither, and `Frustrate.activate` refuses a
/// bridge-identity change there instead. A shipping app cannot reach a stale
/// handle, rather than paying a word and a compare per handle to be told
/// about one.
abstract base class OpaqueHandleBase implements Consumable {
  int? _raw;
  final HandleDrop _drop;

  OpaqueHandleBase(int raw, HandleDrop drop) : _raw = raw, _drop = drop {
    assert(stampHandleBridge(this));
    drop.attach(this, raw);
  }

  bool get isDisposed => _raw == null;

  /// The raw handle value, for encoding into a request.
  ///
  /// Every path to the wire goes through here — a method's receiver, a
  /// parameter, a handle nested in a struct or a list, a trait-typed value —
  /// which is what lets one check cover all of them.
  int get handleValue {
    final r = _raw;
    if (r == null) {
      assert(reportConsumed(this));
      throw StateError('$runtimeType used after dispose()');
    }
    // Checked after the dispose test so the more specific fact stays the
    // reported one. Bridge identity rather than the active runtime object: a
    // decorator over this handle's transport is the same bridge, and a handle
    // must survive one going on and coming off.
    assert(checkHandleBridge(this));
    return r;
  }

  /// Eagerly release the Rust object. Idempotent; after this, any use of the
  /// handle throws StateError.
  ///
  /// Exempt from [handleValue]'s bridge check, deliberately, and so are
  /// [isDisposed] and the GC finalizer: they read [_raw] and [_drop] directly
  /// and reach the transport that minted this handle, which is retained for
  /// the life of the isolate. A handle stranded by a bridge swap must not
  /// thereby become unfreeable.
  void dispose() {
    final r = _raw;
    if (r == null) return;
    _raw = null;
    _drop.detach(this);
    _drop.drop(r);
  }

  /// The raw for a consuming call's request, with the object still ours.
  ///
  /// Goes through [handleValue] rather than reading `_raw`, so the disposed
  /// test and the bridge-identity check stay on one path.
  @override
  int peekHandle(String member) => handleValue;

  /// Give the object up to `member`. Detaches the finalizer and *does not*
  /// drop: Rust takes the object, so freeing it here would be the double free
  /// the token exists to prevent. The handle reads as disposed from here on.
  @override
  void spendHandle(String member) {
    final r = _raw;
    if (r == null) {
      assert(reportConsumed(this));
      throw StateError('$runtimeType used after dispose()');
    }
    assert(recordConsumedBy(this, member));
    _raw = null;
    _drop.detach(this);
  }
}
