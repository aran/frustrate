/// The one condition a consume carries: on a **shared** model the object can
/// only be taken when no call on that handle is still running.
///
/// It is detected, not raced. Every concurrent holder of a frozen or locked
/// object is an `Arc` clone some dispatch prelude made, so `Arc::try_unwrap`
/// sees it and the consuming call throws `ContentionException` naming the type
/// and the member. The object is released, and the call that was still running
/// finishes normally — both asserted below.
///
/// **How a call is made to be in flight, portably.** The fixture parks the
/// Rust body on a Dart closure (`Tape::count_after`, `Jar::add_after`), which
/// is the Rust → Dart → Rust shape `Counter::hold_write_asking` uses: the
/// consuming call is issued from *inside* that closure, so the outer call is
/// provably still running and provably still holds its clone. No sleep, no
/// timing, and it works on single-threaded web, where a pool "in flight" would
/// not exist.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('a frozen consume with a call in flight is refused, and says so', () async {
    final t = Tape.new_(marks: ['a', 'b', 'c']);
    Object? caught;
    // The closure runs while `count_after`'s future is parked holding a clone.
    final inFlight = t.countAfter(
      f: (n) {
        try {
          t.take().intoCount();
        } catch (e) {
          caught = e;
        }
        return n;
      },
    );
    expect(
      await inFlight,
      3,
      reason: 'the refused consume must not disturb the call it refused for',
    );
    expect(
      caught,
      isA<ContentionException>().having(
        (e) => e.message,
        'message',
        allOf(contains('Tape'), contains('into_count'), contains('take()')),
      ),
    );
    // The handle was spent at issue — the object is gone either way, which is
    // what `take()` documents. Nothing leaked: the release happened on the
    // Rust side when the take was refused.
    expect(t.isDisposed, isTrue);
  });

  test('a locked consume with the write guard held is refused', () async {
    final j = Jar.new_(coins: 10);
    Object? caught;
    final inFlight = j.addAfter(
      f: (coins) {
        try {
          j.take().intoCoins();
        } catch (e) {
          caught = e;
        }
        return 5;
      },
    );
    expect(await inFlight, 15);
    expect(
      caught,
      isA<ContentionException>().having(
        (e) => e.message,
        'message',
        allOf(contains('Jar'), contains('into_coins')),
      ),
    );
    expect(j.isDisposed, isTrue);
  });

  test('the same handle, after the call has finished, is taken', () async {
    final t = Tape.new_(marks: ['one']);
    expect(await t.countAfter(f: (n) => n), 1);
    // Nothing holds a clone now.
    expect(t.take().intoCount(), 1);
    expect(t.isDisposed, isTrue);
  });
}
