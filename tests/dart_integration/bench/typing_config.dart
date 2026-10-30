/// The typing-load scenario's settings (`typing_load.dart`), apart from the
/// scenario so that `tool/web_bench.dart`, which imports no bindings, can read
/// them off its command line and hand them to the page.
library;

class TypingLoadConfig {
  const TypingLoadConfig({
    this.noteChars = 200000,
    this.keyHz = 15,
    this.remoteHz = 2,
    this.jobUs = 50000,
    this.sliceUs = 4000,
    this.idleSeconds = 90,
    this.loadSeconds = 150,
  });

  /// Reads `--typing-idle=S`, `--typing-load=S`, `--typing-slice-us=N` and
  /// `--typing-chars=N`; everything else keeps its default.
  factory TypingLoadConfig.fromArgs(Iterable<String> args) {
    int? opt(String name) {
      for (final a in args) {
        if (a.startsWith('--$name=')) return int.parse(a.split('=').last);
      }
      return null;
    }

    const d = TypingLoadConfig();
    return TypingLoadConfig(
      noteChars: opt('typing-chars') ?? d.noteChars,
      sliceUs: opt('typing-slice-us') ?? d.sliceUs,
      idleSeconds: opt('typing-idle') ?? d.idleSeconds,
      loadSeconds: opt('typing-load') ?? d.loadSeconds,
    );
  }

  factory TypingLoadConfig.fromJson(Map<String, Object?> j) => TypingLoadConfig(
    noteChars: j['note_chars'] as int,
    keyHz: j['key_hz'] as int,
    remoteHz: j['remote_hz'] as int,
    jobUs: j['job_us'] as int,
    sliceUs: j['slice_us'] as int,
    idleSeconds: j['idle_s'] as int,
    loadSeconds: j['load_s'] as int,
  );

  final int noteChars;
  final int keyHz;
  final int remoteHz;
  final int jobUs;
  final int sliceUs;
  final int idleSeconds;
  final int loadSeconds;

  Map<String, Object?> toJson() => {
    'note_chars': noteChars,
    'key_hz': keyHz,
    'remote_hz': remoteHz,
    'job_us': jobUs,
    'slice_us': sliceUs,
    'idle_s': idleSeconds,
    'load_s': loadSeconds,
  };
}
