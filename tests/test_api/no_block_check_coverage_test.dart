/// `test_api_no_block_check` covers the claim shapes it was built to cover.
///
/// The check's roots come from whatever this fixture claims, so deleting a
/// `#[bridge(no_block)]` here does not turn the check red: it shrinks what the
/// check covers while it still says "clean". This reads the claim census
/// codegen writes, the only place a claim exists before a module does, and
/// fails when a pinned member stops being claimed or changes how it is
/// settled. If a member genuinely may block now, pin a replacement of the same
/// shape, with the reason it earns its place.
@TestOn('vm')
library;

import 'dart:io';

import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

/// Member, census settlement, and what losing it would un-test.
const _pinned = [
  (
    'add_i32',
    'artifact',
    'the sync claim: the simplest shape a red result gives a full '
        'root-to-body witness for',
  ),
  (
    'emit_two',
    'artifact',
    'the stream claim: the only root that links `StreamSink`, so the '
        'sink\'s own code is inside what the scan sees',
  ),
  (
    'LiveProbe::new',
    'artifact',
    'the handed-out handle claim: an opaque with a real `Drop`, which '
        '`dispose()` and the finalizer run on the calling isolate',
  ),
  (
    'find_snapshot',
    'artifact-residue',
    'the drop-edge residue one level in (`Option<Snapshot>` from a '
        'dispatched member), found by the return type-graph walk',
  ),
  (
    'merge_plans',
    'artifact-residue',
    'the decode residue: `BytesCodec::from_bytes` the glue runs on the '
        'calling thread, reached through a collection',
  ),
  (
    'once_setting',
    'placement-dispatch',
    'a pool member whose body reaches a real wait (a `static OnceLock`), '
        'green only because the body is placed',
  ),
  (
    'withdraw_awaiting',
    'placement-dispatch',
    'the same settlement for an `async fn`, whose later polls and '
        'cancellation drop happen on a drain',
  ),
  (
    'Miner::sleep_on_executor',
    'placement',
    'an actor member that really waits, settled only by running on the '
        'actor\'s own executor',
  ),
];

void main() {
  final census = File(
    Runfiles.create().rlocation('_main/tests/test_api/bridge/no_block.claims'),
  ).readAsLinesSync().map((l) => l.split(' ')).toList();

  for (final (member, settled, why) in _pinned) {
    test('$member is claimed and settled by $settled', () {
      final row = census.where((r) => r.length >= 3 && r[2] == member);
      expect(row.map((r) => r[0]), [
        settled,
      ], reason: '$member is pinned as $why.');
    });
  }
}
