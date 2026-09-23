/// The web transport, named unconditionally — for code that is web-only by
/// construction.
///
/// `package:frustrate/frustrate.dart` exports the transport *conditionally*:
/// `runtime_native.dart` under the VM, `runtime_web.dart` under
/// `dart.library.js_interop`. That is exactly right for code which runs on both
/// and must not name either half — and exactly wrong for code which is web-only
/// by construction: a browser bootstrap, or a test tagged `@TestOn('browser')`.
/// Such a file names [FrustrateWeb], and under the analyzer's default platform
/// (the VM) that name does not exist, so `dart analyze` reports errors about
/// code that is correct and will never run there.
///
/// The cost of leaving that alone is not the noise. It is that the package
/// cannot be analysed at all as a gate — a nonzero baseline is not a baseline —
/// so *every* other error in it goes unnoticed too. This library is what lets
/// `tools/analyze.dart` include the integration tests.
///
///     import 'package:frustrate/frustrate_web.dart';
///
///     await FrustrateWeb.initFromUrl(moduleUrl);
///
/// Import this only where the file cannot run natively. In portable code the
/// conditional export is the point, and reaching past it re-introduces the
/// import that will not compile on the other platform.
library;

export 'src/runtime_web.dart';
