/// A bridge whose Rust calls JavaScript the app supplies through an import
/// namespace of its own (//tests/host_imports), with no wasm-bindgen: the JS
/// module (test_host.js) is named in `FrustrateWeb.init(hostImports:)`, and
/// every realm that instantiates the module imports its own copy and is handed
/// its instance by `attach`.
///
/// The module, test_host.js and the bindings are built by Bazel and served by
/// `bazel run //tests/dart_integration:web_test` (the threaded build under
/// `-- --threaded`).
@TestOn('browser')
@Timeout(Duration(minutes: 2))
library;

import 'dart:convert';
import 'dart:js_interop';
import 'dart:typed_data';

import 'package:frustrate/frustrate_web.dart';
import 'package:host_imports_api/host_imports_api.frustrate.dart';
import 'package:test/test.dart';

const _module =
    '../packages/web_test_fixtures/host_imports/host_imports_api.wasm';
const _host = '../packages/web_test_fixtures/host_imports/test_host.js';

/// The namespaces //bazel/wasm_import_check admits as the runtime's own.
const _selfSupplied =
    '../packages/web_test_fixtures/host_imports/self_supplied.json';

@JS('fetch')
external JSPromise<_Response> _fetch(JSString url);

extension type _Response._(JSObject _) implements JSObject {
  external JSPromise<JSString> text();
}

/// What `attach` was given as `actor` for an instance of the `Files` actor.
final _filesActor = jsonEncode({'bridge': 'host_imports_api', 'type': 'Files'});

Future<void> _init() async {
  await FrustrateWeb.initFromUrl(
    frustrateBridge,
    _module,
    hostImports: {'frustrate_test_host': _host},
  );
  checkFrustrateSchema();
}

void main() {
  // These run before anything initializes the page, because a failed init
  // installs nothing and the next one starts over.
  test('a namespace with no URL fails, naming it and hostImports', () async {
    await expectLater(
      FrustrateWeb.initFromUrl(frustrateBridge, _module),
      throwsA(
        predicate(
          (e) =>
              '$e'.contains("'frustrate_test_host'") &&
              '$e'.contains('hostImports'),
          'an error naming the namespace and hostImports',
        ),
      ),
    );
  });

  test('a URL that fails to load fails, naming the namespace', () async {
    await expectLater(
      FrustrateWeb.initFromUrl(
        frustrateBridge,
        _module,
        hostImports: {
          'frustrate_test_host':
              '../packages/web_test_fixtures/host_imports/none.js',
        },
      ),
      throwsA(
        predicate(
          (e) =>
              '$e'.contains("'frustrate_test_host'") &&
              '$e'.contains('none.js') &&
              '$e'.contains('hostImports'),
          'an error naming the namespace, the URL and hostImports',
        ),
      ),
    );
  });

  test('the runtime claims every namespace the build check takes to be its '
      'own', () async {
    final response = await _fetch(_selfSupplied.toJS).toDart;
    final list = (jsonDecode((await response.text().toDart).toDart) as List)
        .cast<String>();
    expect(list, isNotEmpty);
    for (final ns in list) {
      await expectLater(
        FrustrateWeb.initFromUrl(
          frustrateBridge,
          _module,
          hostImports: {ns: _host},
        ),
        throwsA(
          predicate(
            (e) => '$e'.contains("'$ns'") && '$e'.contains('supplies itself'),
            "an error saying the runtime supplies '$ns' itself",
          ),
        ),
        reason: '//bazel/wasm_import_check admits $ns as the runtime\'s own',
      );
    }
  });

  test("hostImports may not name wasm-bindgen's namespace", () async {
    await expectLater(
      FrustrateWeb.initFromUrl(
        frustrateBridge,
        _module,
        hostImports: {'./host_imports_api_bg.js': _host},
      ),
      throwsA(
        predicate(
          (e) =>
              '$e'.contains("wasm-bindgen's") &&
              '$e'.contains('bindgenGlueUrl'),
          'an error naming bindgenGlueUrl',
        ),
      ),
    );
  });

  test('the page instance is attached with actor null', () async {
    await _init();
    expect(pageActor(), isNull);
  });

  test(
    'an async fn sees actor null, on the page or in a pool worker',
    () async {
      await _init();
      expect(await asyncActor(), isNull);
    },
  );

  test('an actor instance is attached with its bridge and Rust type name, and '
      'attach finishes before the actor is constructed', () async {
    await _init();
    final files = await Files.new_();
    expect(await files.actor(), _filesActor);
    expect(await files.attachedAtConstruction(), isTrue);
    await files.dispose();
  });

  test('an actor call reaches the host functions synchronously, through the '
      "instance's memory, on a file its attach opened", () async {
    await _init();
    final files = await Files.new_();
    final data = Uint8List.fromList(List.generate(1000, (i) => i % 251));
    expect(await files.roundTrip(data: data), data);
    // A second actor has its own realm and its own file.
    final other = await Files.new_();
    final short = Uint8List.fromList([7, 8, 9]);
    expect(await other.roundTrip(data: short), short);
    expect(await files.roundTrip(data: data), data);
    await other.dispose();
    await files.dispose();
  });
}
