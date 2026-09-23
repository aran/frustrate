/// Native actor handle interface.
library;

import 'dart:ffi';

import 'actor_handle_base.dart';

export 'actor_handle_base.dart' show ActorHandleContract;

/// Implemented by every generated actor class: the user-facing handle to a
/// thread-hosted instance.
///
/// Implements [Finalizable], which every generated actor inherits by
/// implementing this. Without it a method could be finalized *midway through
/// its own call*: the receiver is dead the moment its handle value has been
/// encoded into the request, so the GC would be free to collect it, fire the
/// reaper, and pull the executor out from under a call that is still being
/// issued. The failure would not be a use-after-free — `reap` removes the map
/// entry before the drop runs, so a call that lands after it is refused rather
/// than served — but a live call would fail with a dead-host StateError for no
/// reason the caller could see. Same reasoning as `OpaqueHandle`'s.
abstract interface class ActorHandle
    implements ActorHandleContract, Finalizable {}
