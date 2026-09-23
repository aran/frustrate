/// A `locked` object under a holder that is **suspended**, not merely running.
///
/// This state was unreachable until the `locked` model's lock became async.
/// `Counter.addSlowly` is a Rust `async fn` that yields while holding the write
/// guard; every other call on that object then has to wait for a holder that is
/// parked as heap data rather than occupying a thread.
///
/// Why it is worth its own file, on every configuration:
///
///   * **Single-threaded web** is the one that could not survive it. There is
///     one thread and nothing to wait *on* — `std`'s `no_threads` RwLock answers
///     contention with `rtabort!`, and a `block_on`-style acquisition spins
///     forever because `Parker::park` is an empty function there. Only an
///     awaited acquisition, on a body dispatched through the cooperative
///     executor, can yield instead. If either half of that regressed, this test
///     is where the page dies.
///   * **Native and threaded web** get the ordinary version of the same
///     question: a suspended holder must not wedge a pool worker.
///
/// The assertions are on *values*, not on timing. Interleaving is the
/// scheduler's business; what the lock owes is that no update is lost and no
/// call is stranded.
@Timeout(Duration(minutes: 2))
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test(
    'an async fn on a locked type holds its guard across the await',
    () async {
      final c = Counter.new_();
      expect(await c.addSlowly(delta: 5), 5);
      expect(await c.get(), 5);
    },
  );

  test('a no-await async fn on a locked type is a plain shared read', () async {
    final c = Counter.new_();
    await c.add(delta: 7);
    // `peek` has no `.await` in its body at all. It was refused before the
    // lock changed — the *generated* block's await was enough to make the
    // `std` guard's `!Send` fatal — which is what showed the old refusal was
    // over-broad rather than precise.
    expect(await c.peek(), 7);
  });

  test('calls queue behind a suspended holder instead of aborting', () async {
    final c = Counter.new_();
    // Issued together, so the second acquisition happens while the first
    // holder is parked mid-body. Before the async lock this could not be
    // written; with a blocking acquisition it would abort or spin.
    final results = await Future.wait([
      c.addSlowly(delta: 1),
      c.addSlowly(delta: 1),
      c.addSlowly(delta: 1),
    ]);
    // Whatever the interleaving, the lock serialized them: the three results
    // are 1, 2, 3 in some order, and the final value counted every one.
    expect(results..sort(), [1, 2, 3]);
    expect(await c.get(), 3);
  });

  test('a suspended holder does not strand later calls', () async {
    final c = Counter.new_();
    final slow = c.addSlowly(delta: 10);
    final fast = c.add(delta: 1);
    await Future.wait([slow, fast]);
    expect(await c.get(), 11);
  });
}
