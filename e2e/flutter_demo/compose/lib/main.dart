// Two bridges in one Rust library: each button calls into its own bridge, and
// each bridge counts its taps in a static of its own.

import 'package:compose_ping/compose_ping.frustrate.dart' as ping;
import 'package:compose_pong/compose_pong.frustrate.dart' as pong;
import 'package:material_ui/material_ui.dart';
import 'package:frustrate/frustrate.dart';

void main() {
  FrustrateNative.init(ping.composePingLibrary);
  ping.checkFrustrateSchema();
  FrustrateNative.init(pong.composePongLibrary);
  pong.checkFrustrateSchema();
  runApp(const MaterialApp(title: 'frustrate compose', home: ComposePage()));
}

class ComposePage extends StatefulWidget {
  const ComposePage({super.key});

  @override
  State<ComposePage> createState() => _ComposePageState();
}

class _ComposePageState extends State<ComposePage> {
  String _ping = ping.pingLabel(taps: 0);
  String _pong = pong.pongLabel(taps: 0);

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          spacing: 16,
          children: [
            Text(_ping, key: const Key('ping')),
            FilledButton(
              key: const Key('ping-tap'),
              onPressed: () =>
                  setState(() => _ping = ping.pingLabel(taps: ping.pingTap())),
              child: const Text('ping'),
            ),
            Text(_pong, key: const Key('pong')),
            FilledButton(
              key: const Key('pong-tap'),
              onPressed: () =>
                  setState(() => _pong = pong.pongLabel(taps: pong.pongTap())),
              child: const Text('pong'),
            ),
          ],
        ),
      ),
    );
  }
}
