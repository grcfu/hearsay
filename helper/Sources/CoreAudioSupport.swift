import CoreAudio
import Foundation

/// The Core Audio system object. Every hardware-level query starts here.
let systemObject = AudioObjectID(kAudioObjectSystemObject)

/// Errors raised by the helper. Every one of these is a hard failure that ends in a
/// non-zero exit — the helper never degrades into producing silence.
enum HelperError: Error, CustomStringConvertible {
    case coreAudio(op: String, status: OSStatus)
    case noSuchProcess(String)
    case usage(String)

    var description: String {
        switch self {
        case let .coreAudio(op, status):
            return "\(op) failed: \(fourCharCode(status))"
        case let .noSuchProcess(what):
            return "no audio process matched \(what)"
        case let .usage(message):
            return message
        }
    }

    /// Exit code. Distinct codes let Rust tell "you denied permission" apart from
    /// "that process isn't running" without parsing English.
    var exitCode: Int32 {
        switch self {
        case .usage: return 64  // EX_USAGE
        case .noSuchProcess: return 69  // EX_UNAVAILABLE
        case .coreAudio: return 70  // EX_SOFTWARE
        }
    }
}

/// Renders an OSStatus as a four-char code when it is printable, else as a number.
/// Core Audio reports almost everything as a FourCC and the numeric form is useless.
func fourCharCode(_ status: OSStatus) -> String {
    let n = UInt32(bitPattern: status)
    let bytes: [UInt8] = [
        UInt8((n >> 24) & 0xFF), UInt8((n >> 16) & 0xFF),
        UInt8((n >> 8) & 0xFF), UInt8(n & 0xFF),
    ]
    if bytes.allSatisfy({ $0 >= 0x20 && $0 < 0x7F }),
        let text = String(bytes: bytes, encoding: .ascii)
    {
        return "'\(text)' (\(status))"
    }
    return "\(status)"
}

func address(
    _ selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
    element: AudioObjectPropertyElement = kAudioObjectPropertyElementMain
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: element)
}

/// Byte size of a property, or throws if the object does not carry it.
func propertySize(
    _ object: AudioObjectID, _ selector: AudioObjectPropertySelector, op: String
) throws -> UInt32 {
    var addr = address(selector)
    var size: UInt32 = 0
    let status = AudioObjectGetPropertyDataSize(object, &addr, 0, nil, &size)
    guard status == noErr else { throw HelperError.coreAudio(op: op, status: status) }
    return size
}

/// Reads a fixed-layout property (integers, structs, booleans-as-UInt32).
func readProperty<T>(
    _ object: AudioObjectID, _ selector: AudioObjectPropertySelector,
    initial: T, op: String
) throws -> T {
    var addr = address(selector)
    var value = initial
    var size = UInt32(MemoryLayout<T>.size)
    let status = withUnsafeMutablePointer(to: &value) { ptr in
        AudioObjectGetPropertyData(object, &addr, 0, nil, &size, ptr)
    }
    guard status == noErr else { throw HelperError.coreAudio(op: op, status: status) }
    return value
}

/// Reads a variable-length array property, such as the process object list.
func readArrayProperty<T>(
    _ object: AudioObjectID, _ selector: AudioObjectPropertySelector,
    of _: T.Type, op: String
) throws -> [T] {
    let size = try propertySize(object, selector, op: op)
    let count = Int(size) / MemoryLayout<T>.size
    guard count > 0 else { return [] }

    var addr = address(selector)
    var byteSize = size
    let raw = UnsafeMutableRawPointer.allocate(
        byteCount: Int(size), alignment: MemoryLayout<T>.alignment)
    defer { raw.deallocate() }

    let status = AudioObjectGetPropertyData(object, &addr, 0, nil, &byteSize, raw)
    guard status == noErr else { throw HelperError.coreAudio(op: op, status: status) }

    let typed = raw.bindMemory(to: T.self, capacity: count)
    let actual = Int(byteSize) / MemoryLayout<T>.size
    return Array(UnsafeBufferPointer(start: typed, count: min(actual, count)))
}

/// Reads a CFString-valued property, returning nil when the object has no value for it.
/// Several process properties are legitimately absent (a daemon with no bundle ID),
/// so absence is not an error here.
func readStringProperty(
    _ object: AudioObjectID, _ selector: AudioObjectPropertySelector
) -> String? {
    var addr = address(selector)
    var value: CFString?
    var size = UInt32(MemoryLayout<CFString?>.size)
    let status = withUnsafeMutablePointer(to: &value) { ptr in
        AudioObjectGetPropertyData(object, &addr, 0, nil, &size, ptr)
    }
    guard status == noErr, let value else { return nil }
    let string = value as String
    return string.isEmpty ? nil : string
}

/// Writes a line to stderr. All human-facing output goes here; stdout is reserved
/// for machine-readable output (JSON for `--list`, raw PCM when capturing).
func log(_ message: String) {
    FileHandle.standardError.write(Data((message + "\n").utf8))
}
