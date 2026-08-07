import AppKit
import CoreAudio
import Foundation

/// One process as Core Audio sees it.
///
/// `objectID` is the handle a tap is built from; PID and bundle ID exist so a human
/// (or Rust) can pick the right one. Object IDs are not stable across launches — always
/// resolve by PID or bundle ID at record time, never cache an object ID.
struct AudioProcess: Codable {
    let objectID: UInt32
    let pid: Int32
    let bundleID: String?
    let name: String?
    let isRunningOutput: Bool
    let isRunningInput: Bool

    enum CodingKeys: String, CodingKey {
        case objectID = "object_id"
        case pid
        case bundleID = "bundle_id"
        case name
        case isRunningOutput = "is_running_output"
        case isRunningInput = "is_running_input"
    }

    /// Written by hand so absent values encode as explicit `null` rather than being
    /// dropped. The synthesised encoder omits nil keys, which would make the shape of
    /// the JSON depend on the data.
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(objectID, forKey: .objectID)
        try c.encode(pid, forKey: .pid)
        try c.encode(bundleID, forKey: .bundleID)
        try c.encode(name, forKey: .name)
        try c.encode(isRunningOutput, forKey: .isRunningOutput)
        try c.encode(isRunningInput, forKey: .isRunningInput)
    }
}

/// Every process Core Audio knows about, whether or not it is currently making sound.
func allAudioProcesses() throws -> [AudioProcess] {
    let objectIDs = try readArrayProperty(
        systemObject, kAudioHardwarePropertyProcessObjectList,
        of: AudioObjectID.self, op: "read process object list")

    return objectIDs.compactMap { objectID -> AudioProcess? in
        // A process can exit between listing and inspection; skip it rather than fail
        // the whole enumeration.
        guard
            let pid = try? readProperty(
                objectID, kAudioProcessPropertyPID, initial: pid_t(0), op: "read process pid")
        else { return nil }

        let bundleID = readStringProperty(objectID, kAudioProcessPropertyBundleID)
        let running = (try? readProperty(
            objectID, kAudioProcessPropertyIsRunningOutput, initial: UInt32(0),
            op: "read is-running-output")) ?? 0
        let capturing = (try? readProperty(
            objectID, kAudioProcessPropertyIsRunningInput, initial: UInt32(0),
            op: "read is-running-input")) ?? 0

        return AudioProcess(
            objectID: UInt32(objectID),
            pid: pid,
            bundleID: bundleID,
            name: displayName(pid: pid, bundleID: bundleID),
            isRunningOutput: running != 0,
            isRunningInput: capturing != 0)
    }
}

/// Resolves the process object to tap, by PID or by bundle ID.
///
/// Bundle ID can match several processes — Chrome and Electron apps run helper
/// processes that each register separately — so this returns every match and the
/// caller taps all of them together.
func findProcesses(pid: Int32?, bundleID: String?) throws -> [AudioProcess] {
    let processes = try allAudioProcesses()

    if let pid {
        let matches = processes.filter { $0.pid == pid }
        guard !matches.isEmpty else { throw HelperError.noSuchProcess("pid \(pid)") }
        return matches
    }

    if let bundleID {
        let wanted = bundleID.lowercased()
        let matches = processes.filter { $0.bundleID?.lowercased() == wanted }
        guard !matches.isEmpty else {
            throw HelperError.noSuchProcess("bundle id \(bundleID)")
        }
        return matches
    }

    return []
}

/// Best available human-readable name. The running-application lookup covers GUI apps;
/// `ps`-style names via `proc_name` would need a private-ish syscall, so daemons simply
/// fall back to their bundle ID and then to nil.
private func displayName(pid: Int32, bundleID: String?) -> String? {
    if let app = NSRunningApplication(processIdentifier: pid), let name = app.localizedName {
        return name
    }
    if let bundleID { return bundleID.split(separator: ".").last.map(String.init) }
    return nil
}

/// Prints the process list as JSON on stdout.
func printProcessList(includeSilent: Bool) throws {
    var processes = try allAudioProcesses()
    if !includeSilent {
        processes = processes.filter { $0.isRunningOutput }
    }
    processes.sort { ($0.name ?? "") .lowercased() < ($1.name ?? "").lowercased() }

    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    let data = try encoder.encode(processes)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
}
