/// Shared `gcloud` helpers for the relay VM scripts.
///
/// NEVER RUN. Nothing in this directory has been executed. See ../README.md.
///
/// This is a trimmed copy of `rules_flutter/tools/vm/gcloud.dart` — same
/// shapes, same names, minus the Windows and screenshot machinery. It is a
/// copy because there is nowhere for it to be shared from: rules_flutter is a
/// sibling Bazel module, not a Dart package this one can depend on, and a
/// relative import across repository roots would break the moment either
/// moves. The two copies will drift, and nothing will notice.
///
/// Project and zone come from `gcloud config` defaults, exactly as they do
/// there.
library;

import 'dart:io';

/// Runs a gcloud command and returns stdout. Throws on failure.
Future<String> gcloud(List<String> args, {bool quiet = false}) async {
  if (!quiet) {
    stderr.writeln('+ gcloud ${args.join(' ')}');
  }
  final result = await Process.run('gcloud', args);
  if (result.exitCode != 0) {
    throw Exception(
      'gcloud ${args.join(' ')} failed (exit ${result.exitCode}):\n'
      '${result.stderr}',
    );
  }
  return result.stdout.toString().trim();
}

/// Runs a gcloud command and returns the full result without throwing — for
/// commands whose non-zero exit is meaningful (e.g. "does this already exist").
Future<ProcessResult> gcloudTry(List<String> args) {
  stderr.writeln('+ gcloud ${args.join(' ')}');
  return Process.run('gcloud', args);
}

/// The active gcloud project.
Future<String> getProject() async {
  final project = await gcloud(['config', 'get-value', 'project'], quiet: true);
  if (project.isEmpty || project == '(unset)') {
    throw Exception('No default project set. Run: gcloud config set project <id>');
  }
  return project;
}

/// The active gcloud zone.
Future<String> getZone() async {
  final zone = await gcloud(['config', 'get-value', 'compute/zone'], quiet: true);
  if (zone.isEmpty || zone == '(unset)') {
    throw Exception(
      'No default compute zone set. Run: gcloud config set compute/zone <zone>',
    );
  }
  return zone;
}

/// The region containing [zone] — `us-central1-a` -> `us-central1`.
String regionOfZone(String zone) =>
    zone.substring(0, zone.lastIndexOf('-'));

/// Waits for a VM to accept SSH.
Future<void> waitForSsh(
  String vmName, {
  Duration timeout = const Duration(minutes: 5),
}) async {
  final deadline = DateTime.now().add(timeout);
  stderr.writeln('Waiting for $vmName to accept SSH ...');
  while (DateTime.now().isBefore(deadline)) {
    try {
      await gcloud([
        'compute',
        'ssh',
        vmName,
        '--command',
        'echo ready',
        '--ssh-flag=-o',
        '--ssh-flag=ConnectTimeout=5',
        '--ssh-flag=-o',
        '--ssh-flag=StrictHostKeyChecking=no',
      ], quiet: true);
      stderr.writeln('$vmName is ready.');
      return;
    } catch (_) {
      await Future<void>.delayed(const Duration(seconds: 10));
    }
  }
  throw Exception('Timeout waiting for $vmName SSH');
}

/// SCPs a local file or directory to the VM.
Future<void> scpToVm(String vmName, String localPath, String remotePath) async {
  await gcloud([
    'compute',
    'scp',
    '--recurse',
    '--scp-flag=-O',
    '--compress',
    localPath,
    '$vmName:$remotePath',
  ]);
}

/// Runs [command] on the VM via SSH, returning trimmed stdout. Throws on a
/// non-zero exit.
Future<String> sshRun(String vmName, String command) async {
  return gcloud(['compute', 'ssh', vmName, '--command', command]);
}

/// Runs [command] on the VM and streams its output to this terminal. Long
/// builds are unwatchable otherwise. Returns the exit code.
Future<int> sshStream(String vmName, String command) async {
  stderr.writeln('+ ssh $vmName: $command');
  final process = await Process.start(
    'gcloud',
    ['compute', 'ssh', vmName, '--command', command],
    mode: ProcessStartMode.inheritStdio,
  );
  return process.exitCode;
}
