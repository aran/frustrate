/// Web actor handle interface.
library;

import 'actor_handle_base.dart';

export 'actor_handle_base.dart' show ActorHandleContract;

/// Implemented by every generated actor class: the user-facing handle to a
/// worker-hosted instance.
///
/// Web has no `Finalizable` equivalent, so there is no VM-level guarantee that
/// a handle outlives a call still issuing against it. The transport
/// compensates structurally, the same way it does for `OpaqueHandle`: a
/// request is fully encoded before any await, and `dart:core`'s [Finalizer]
/// only runs callbacks between event-loop turns — so the reaper cannot
/// interleave with the synchronous encode-and-post.
abstract interface class ActorHandle implements ActorHandleContract {}
