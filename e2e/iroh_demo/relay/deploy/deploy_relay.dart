/// Builds the relay on the VM and installs it as a systemd service.
///
/// NEVER RUN. Nothing here has been executed against a real host, and the
/// relay it installs has never served traffic. See ../README.md.
///
/// Usage:
///   dart run relay/deploy/deploy_relay.dart <vm-name>
///
/// Why it builds *on* the VM rather than shipping a binary: cross-compiling
/// `x86_64-unknown-linux-gnu` from this macOS workstation needs a Linux
/// cc_toolchain that no module in these three repos registers, and rules_rust's
/// own `allocator_library` needs one before `ring` even gets a chance to fail.
/// Building natively on the target is the boring way out.
///
/// The cost of that choice: Bazel resolves the *whole* module graph before it
/// analyses one target, and the demo `local_path_override`s frustrate, so the
/// whole frustrate worktree goes to the VM to build a relay that uses none of it.
///
/// Steps:
///   1. Refuse to run against an unedited relay.toml.
///   2. Tar and upload the frustrate source tree.
///   3. `bazel build -c opt //relay:iroh_relay` on the VM.
///   4. Install binary, config, and unit; enable and start.
///   5. Print the journal and the smoke-test command.
library;

import 'dart:io';

import 'gcloud.dart';

const _remoteRoot = '/home/deploy_src';
const _placeholderHost = 'relay.example.com';

Future<void> main(List<String> args) async {
  if (args.isEmpty) {
    stderr.writeln('Usage: dart run relay/deploy/deploy_relay.dart <vm-name>');
    exit(1);
  }
  final vmName = args[0];

  // relay/deploy/ -> relay/ -> e2e/iroh_demo/ -> e2e/ -> <frustrate root>
  final deployDir = Directory.fromUri(Platform.script.resolve('.')).path;
  final relayDir = Directory(deployDir).parent.path;
  final moduleDir = Directory(relayDir).parent.path;
  final frustrateRoot = Directory(moduleDir).parent.parent.path;

  // 1. A relay.toml still naming relay.example.com would order a certificate
  //    for a domain nobody controls and burn a Let's Encrypt rate-limit slot.
  //    Fail before touching the VM.
  final config = File('$relayDir/relay.toml').readAsStringSync();
  if (config.contains(_placeholderHost) || config.contains('you@example.com')) {
    stderr.writeln('relay/relay.toml still has placeholder values.');
    stderr.writeln('Set tls.hostname to the DNS name whose A record points at');
    stderr.writeln('this VM, and tls.contact to a real mailbox, then re-run.');
    stderr.writeln('');
    stderr.writeln('The A record must already resolve: Let\'s Encrypt looks the');
    stderr.writeln('name up and connects back on port 80 at first start.');
    exit(1);
  }
  final hostname = RegExp(r'^hostname\s*=\s*"([^"]+)"', multiLine: true)
      .firstMatch(config)
      ?.group(1);
  if (hostname == null) {
    stderr.writeln('Could not read tls.hostname out of relay/relay.toml.');
    exit(1);
  }
  print('Deploying relay for https://$hostname to $vmName');

  // 2. Upload sources. Excludes keep this at tens of megabytes rather than the
  //    gigabytes that bazel-out and cargo target/ dirs add up to.
  final tmp = Directory.systemTemp.createTempSync('relay_deploy_');
  try {
    for (final tree in [frustrateRoot]) {
      final name = tree.split('/').last;
      final tarball = '${tmp.path}/$name.tar.gz';
      print('Packing $tree ...');
      final tar = await Process.run('tar', [
        '-czf', tarball,
        '-C', Directory(tree).parent.path,
        '--exclude=bazel-*',
        '--exclude=target',
        '--exclude=.git',
        '--exclude=.dart_tool',
        '--exclude=node_modules',
        '--exclude=.bazelrc.user',
        name,
      ]);
      if (tar.exitCode != 0) {
        stderr.writeln('tar failed: ${tar.stderr}');
        exit(1);
      }
      final size = File(tarball).lengthSync() ~/ (1024 * 1024);
      print('  ${size}MB');
      await sshRun(vmName, 'sudo mkdir -p $_remoteRoot && sudo chown \$USER $_remoteRoot');
      await scpToVm(vmName, tarball, '$_remoteRoot/$name.tar.gz');
      await sshRun(
        vmName,
        'rm -rf $_remoteRoot/$name && tar -xzf $_remoteRoot/$name.tar.gz -C $_remoteRoot',
      );
    }
  } finally {
    tmp.deleteSync(recursive: true);
  }

  final frustrateName = frustrateRoot.split('/').last;
  final remoteModule = '$_remoteRoot/$frustrateName/e2e/iroh_demo';

  // 3. Build. -c opt because this is the artifact that runs for weeks.
  //    --jobs=2 keeps peak memory survivable on e2-medium; the swapfile the
  //    create script adds is the other half of that bet.
  print('');
  print('Building //relay:iroh_relay on the VM (expect 15-40 minutes) ...');
  final buildExit = await sshStream(
    vmName,
    'cd $remoteModule && bazel build -c opt --jobs=2 //relay:iroh_relay',
  );
  if (buildExit != 0) {
    stderr.writeln('Build failed on the VM (exit $buildExit).');
    stderr.writeln('If it was OOM-killed, resize and retry:');
    stderr.writeln('  gcloud compute instances stop $vmName');
    stderr.writeln('  gcloud compute instances set-machine-type $vmName '
        '--machine-type=e2-standard-4');
    stderr.writeln('  gcloud compute instances start $vmName');
    exit(1);
  }

  // The alias resolves to a crate_universe-mangled path that moves with the
  // rules_rust version, so ask Bazel rather than hardcoding it.
  final binPath = (await sshRun(
    vmName,
    'cd $remoteModule && bazel cquery -c opt --output=files //relay:iroh_relay '
    '2>/dev/null | tail -1',
  ))
      .trim();
  if (binPath.isEmpty) {
    stderr.writeln('cquery did not report an output file for //relay:iroh_relay.');
    exit(1);
  }
  print('Built: $binPath');

  // 4. Install. The binary is replaced with install(1) rather than cp so that
  //    a running relay is not overwritten in place.
  print('');
  print('Installing ...');
  await sshRun(vmName, '''
set -e
cd $remoteModule
sudo install -m 0755 $binPath /usr/local/bin/iroh-relay.new
sudo mv /usr/local/bin/iroh-relay.new /usr/local/bin/iroh-relay
sudo mkdir -p /etc/iroh-relay
sudo install -m 0644 relay/relay.toml /etc/iroh-relay/relay.toml
sudo install -m 0644 relay/iroh-relay.service /etc/systemd/system/iroh-relay.service
sudo systemctl daemon-reload
sudo systemctl enable iroh-relay
sudo systemctl restart iroh-relay
''');

  // 5. Report. The certificate order happens on first start and is the most
  //    likely thing to have gone wrong, so show the journal.
  await Future<void>.delayed(const Duration(seconds: 15));
  print('');
  print('--- systemctl status ---');
  try {
    print(await sshRun(vmName, 'systemctl status iroh-relay --no-pager || true'));
  } catch (e) {
    print('(status unavailable: $e)');
  }
  print('--- journal ---');
  try {
    print(await sshRun(vmName, 'sudo journalctl -u iroh-relay -n 50 --no-pager || true'));
  } catch (e) {
    print('(journal unavailable: $e)');
  }

  print('');
  print('=' * 72);
  print('Installed. Verify from your workstation — this is the acceptance test,');
  print('and it exercises DNS, the certificate, the websocket upgrade, and the');
  print('relaying itself in one command:');
  print('');
  print('  bazel run //relay:relay_smoke -- https://$hostname');
  print('');
  print('Then confirm it survives a restart, which is the other half of M3:');
  print('');
  print('  gcloud compute instances reset $vmName');
  print('  # wait ~60s');
  print('  bazel run //relay:relay_smoke -- https://$hostname');
  print('=' * 72);
}
