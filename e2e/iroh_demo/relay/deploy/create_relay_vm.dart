/// Creates the GCP VM that hosts the Tin Can relay.
///
/// NEVER RUN. No VM, address, or firewall rule has ever been created by this
/// script. See ../README.md before running it, and read every gcloud command
/// below first — each one costs money or opens a port.
///
/// Usage:
///   dart run relay/deploy/create_relay_vm.dart [vm-name] [--machine-type=...]
///
/// What it creates, in this order:
///   1. A *reserved static* external IP. This is the one resource that must
///      outlive the VM: the DNS A record points at it, and a spot preemption
///      would otherwise hand back a different ephemeral address and silently
///      break the relay's certificate renewal.
///   2. Firewall rules on the `iroh-relay` network tag — tcp:80, tcp:443,
///      udp:7842. Ports from iroh-relay/src/defaults.rs; the crate README's
///      "7824" is a typo. Metrics (9090) is deliberately NOT opened; the
///      config binds it to loopback.
///   3. The VM itself: spot, so it is cheap and interruptible.
///
/// It does NOT create the DNS record and it does NOT install the relay. It
/// prints the address and stops, because the A record has to exist and
/// propagate before the relay first starts — Let's Encrypt's HTTP-01 challenge
/// resolves the hostname and connects back on port 80.
///
/// Modelled on rules_flutter/tools/vm/create_linux_vm.dart.
library;

import 'dart:io';

import 'gcloud.dart';

const _defaultName = 'iroh-relay';
const _addressName = 'iroh-relay-ip';
const _networkTag = 'iroh-relay';

// e2-medium (2 vCPU / 4 GB) is comfortable for
// *running* a demo relay and marginal for *building* 350 Rust crates, which is
// why the startup script adds swap. Override with --machine-type if the build
// still thrashes; the relay does not care.
const _defaultMachineType = 'e2-medium';
const _imageFamily = 'ubuntu-2404-lts-amd64';
const _imageProject = 'ubuntu-os-cloud';

// 10 GB (the image default) does not hold a Bazel output base for this graph.
const _bootDiskGb = 50;

const _startupScript = r'''#!/bin/bash
set -ex

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential curl git unzip

# Bazel, via bazelisk — same mechanism as rules_flutter/tools/vm.
if ! command -v bazel &>/dev/null; then
  curl -fsSL https://github.com/bazelbuild/bazelisk/releases/latest/download/bazelisk-linux-amd64 -o /usr/local/bin/bazel
  chmod +x /usr/local/bin/bazel
fi

# 4 GB of swap. rustc's peak RSS on the larger crates in this graph will not
# fit alongside a parallel build in 4 GB of RAM, and a linker OOM-killed at
# 95% is a miserable way to find that out.
if [ ! -f /swapfile ]; then
  fallocate -l 4G /swapfile
  chmod 600 /swapfile
  mkswap /swapfile
  swapon /swapfile
  echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi

echo "STARTUP_COMPLETE" > /tmp/startup_complete
''';

Future<void> main(List<String> args) async {
  final positional = args.where((a) => !a.startsWith('--')).toList();
  final vmName = positional.isNotEmpty ? positional[0] : _defaultName;
  final machineTypeFlag = args.firstWhere(
    (a) => a.startsWith('--machine-type='),
    orElse: () => '--machine-type=$_defaultMachineType',
  );
  final machineType = machineTypeFlag.substring('--machine-type='.length);

  final project = await getProject();
  final zone = await getZone();
  final region = regionOfZone(zone);

  print('Creating relay VM: $vmName');
  print('  Project:      $project');
  print('  Zone:         $zone (region $region)');
  print('  Machine type: $machineType');
  print('  Image:        $_imageFamily ($_imageProject)');
  print('  Boot disk:    ${_bootDiskGb}GB');
  print('  Provisioning: SPOT, terminate-action STOP');
  print('');

  // 1. Static IP. Reserved separately from the VM so that deleting and
  //    recreating the VM does not invalidate the DNS record.
  final existing = await gcloudTry([
    'compute',
    'addresses',
    'describe',
    _addressName,
    '--region=$region',
    '--format=value(address)',
  ]);
  final String ip;
  if (existing.exitCode == 0) {
    ip = existing.stdout.toString().trim();
    print('Reusing reserved address $_addressName: $ip');
  } else {
    await gcloud(['compute', 'addresses', 'create', _addressName, '--region=$region']);
    ip = await gcloud([
      'compute',
      'addresses',
      'describe',
      _addressName,
      '--region=$region',
      '--format=value(address)',
    ], quiet: true);
    print('Reserved address $_addressName: $ip');
  }

  // 2. Firewall. Scoped to the network tag, not to the whole network, so this
  //    does not open ports on anything else in the project.
  await _ensureFirewall(
    name: 'iroh-relay-allow-http',
    allow: 'tcp:80,tcp:443',
    description: 'iroh relay HTTP/HTTPS (and the ACME HTTP-01 challenge)',
  );
  await _ensureFirewall(
    name: 'iroh-relay-allow-quic',
    allow: 'udp:7842',
    description: 'iroh relay QUIC address discovery',
  );

  // 3. The VM. SPOT keeps it cheap; STOP rather than DELETE on preemption so
  //    the disk (and the cached Let's Encrypt certificate on it) survives, and
  //    `gcloud compute instances start` brings the whole thing back.
  await gcloud([
    'compute',
    'instances',
    'create',
    vmName,
    '--machine-type=$machineType',
    '--image-family=$_imageFamily',
    '--image-project=$_imageProject',
    '--boot-disk-size=${_bootDiskGb}GB',
    '--boot-disk-type=pd-balanced',
    '--provisioning-model=SPOT',
    '--instance-termination-action=STOP',
    '--address=$ip',
    '--tags=$_networkTag',
    '--metadata=startup-script=$_startupScript',
    '--scopes=default',
  ]);

  print('');
  print('VM created. Waiting for SSH ...');
  await waitForSsh(vmName);

  print('Waiting for startup script (apt, bazelisk, swap) ...');
  final deadline = DateTime.now().add(const Duration(minutes: 10));
  var ready = false;
  while (DateTime.now().isBefore(deadline)) {
    try {
      final result = await sshRun(vmName, 'cat /tmp/startup_complete 2>/dev/null');
      if (result.contains('STARTUP_COMPLETE')) {
        ready = true;
        break;
      }
    } catch (_) {}
    await Future<void>.delayed(const Duration(seconds: 10));
  }
  if (!ready) {
    stderr.writeln('Startup script did not finish in 10 minutes. Check:');
    stderr.writeln('  gcloud compute ssh $vmName --command '
        '"sudo journalctl -u google-startup-scripts"');
    exit(1);
  }

  print('');
  print('=' * 72);
  print('VM ready: $vmName at $ip');
  print('');
  print('A HUMAN MUST DO THIS NEXT, and nothing automates it:');
  print('');
  print('  1. Create a DNS A record:');
  print('       relay.<your-domain>.   A   $ip');
  print('     and wait for it to resolve. Check with:');
  print('       dig +short relay.<your-domain>');
  print('     Let\'s Encrypt resolves this name and connects back on port 80.');
  print('     If it does not resolve when the relay first starts, the');
  print('     certificate order fails and you burn a rate-limit slot.');
  print('');
  print('  2. Put that same name in relay/relay.toml as tls.hostname, and a');
  print('     real mailbox in tls.contact.');
  print('');
  print('  3. Then deploy:');
  print('       dart run relay/deploy/deploy_relay.dart $vmName');
  print('=' * 72);
}

Future<void> _ensureFirewall({
  required String name,
  required String allow,
  required String description,
}) async {
  final existing = await gcloudTry([
    'compute',
    'firewall-rules',
    'describe',
    name,
    '--format=value(name)',
  ]);
  if (existing.exitCode == 0) {
    print('Firewall rule $name already exists.');
    return;
  }
  await gcloud([
    'compute',
    'firewall-rules',
    'create',
    name,
    '--allow=$allow',
    '--target-tags=$_networkTag',
    '--source-ranges=0.0.0.0/0',
    '--description=$description',
  ]);
}
