import Foundation

/// hearsay-audio-helper
///
/// A dumb pipe. It opens a Core Audio process tap and writes raw interleaved float32
/// PCM to stdout. It does not buffer, does not write files, does not decide anything.
/// All policy lives in Rust — see CLAUDE.md §3.

let helperVersion = "0.1.0"

let usage = """
    hearsay-audio-helper \(helperVersion)

    Lists and taps audio-producing processes using Core Audio process taps.
    No virtual audio device is created and the system's audio routing is never changed.

    Usage:
      hearsay-audio-helper --list [--all]
      hearsay-audio-helper --help
      hearsay-audio-helper --version

    Options:
      --list       Print audio-producing processes as JSON on stdout.
      --all        With --list, include processes that are not currently making sound.
      --help       Show this message.
      --version    Print the version.

    Output:
      stdout  machine-readable only (JSON for --list)
      stderr  diagnostics, human-readable
    """

func run() throws {
    var arguments = Array(CommandLine.arguments.dropFirst())

    if arguments.isEmpty || arguments.contains("--help") || arguments.contains("-h") {
        log(usage)
        return
    }

    if arguments.contains("--version") {
        log(helperVersion)
        return
    }

    var includeSilent = false
    if let index = arguments.firstIndex(of: "--all") {
        includeSilent = true
        arguments.remove(at: index)
    }

    guard let command = arguments.first else {
        throw HelperError.usage("no command given\n\n\(usage)")
    }

    switch command {
    case "--list":
        try printProcessList(includeSilent: includeSilent)
    default:
        throw HelperError.usage("unrecognised argument: \(command)\n\n\(usage)")
    }
}

do {
    try run()
} catch let error as HelperError {
    log("error: \(error.description)")
    exit(error.exitCode)
} catch {
    log("error: \(error.localizedDescription)")
    exit(70)
}
