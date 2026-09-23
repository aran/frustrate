/// Native opaque handle base.
library;

import 'dart:ffi';

import 'opaque_handle_base.dart';

/// A Dart-owned handle to a Rust object.
///
/// Implements [Finalizable] so the VM keeps the handle reachable for the
/// duration of any function that uses it — it cannot be finalized midway
/// through a bridge call that borrows it.
abstract base class OpaqueHandle extends OpaqueHandleBase
    implements Finalizable {
  OpaqueHandle(super.raw, super.drop);
}
