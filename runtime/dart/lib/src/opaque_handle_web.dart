/// Web opaque handle base.
library;

import 'opaque_handle_base.dart';

/// A Dart-owned handle to a Rust object.
///
/// Web has no [Finalizable] equivalent, so there is no VM-level guarantee
/// that a handle outlives a bridge call still using it. The transport
/// compensates structurally: requests are fully encoded (handle values
/// copied out) before any await, and GC finalization is delivered via
/// [Finalizer], which only runs between event-loop turns.
abstract base class OpaqueHandle extends OpaqueHandleBase {
  OpaqueHandle(super.raw, super.drop);
}
