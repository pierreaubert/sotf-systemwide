// Encryption.swift - ChaCha20-Poly1305 encryption for audio data
//
// Provides encryption for audio data exchanged between the Swift HAL driver
// and Rust daemon via shared memory. Uses CryptoKit for native performance.
//
// Security Model:
// - ChaCha20-Poly1305 AEAD cipher
// - Key is loaded from the HAL-readable copy at /tmp/sotf-{uid}/session.key
//   (the daemon-private key under ~/.config is unreadable to sandboxed
//   coreaudiod); lab overrides via SOTF_HAL_SESSION_KEY_PATH or
//   SOTF_SYSTEMWIDE_RUNTIME_DIR
// - Frame counter provides unique nonces
// - Poly1305 authentication detects tampering
//
// Must match the Rust implementation in driver-hal/src/encryption.rs

import Foundation
import CryptoKit
import CommonCrypto
import SystemConfiguration

/// Size of the authentication tag (16 bytes for Poly1305)
let kAuthTagSize = 16

private func getSessionKeyPath() -> String {
    let environment = ProcessInfo.processInfo.environment
    if let overridePath = environment["SOTF_HAL_SESSION_KEY_PATH"], !overridePath.isEmpty {
        return overridePath
    }
    if let runtimeDir = environment["SOTF_SYSTEMWIDE_RUNTIME_DIR"], !runtimeDir.isEmpty {
        return URL(fileURLWithPath: runtimeDir).appendingPathComponent("session.key").path
    }

    var uid: uid_t = 0
    var gid: gid_t = 0

    if SCDynamicStoreCopyConsoleUser(nil, &uid, &gid) != nil,
       uid != 0 {
        return "/tmp/sotf-\(uid)/session.key"
    }

    // No console user: coreaudiod cannot reach any home directory, so fall
    // back to a path that fails to open instead of probing an unreachable
    // daemon-private key. Explicit lab overrides are still honored above.
    halLog("ERROR: No console user found, cannot determine session key path")
    return "/tmp/sotf-unknown/session.key"
}

/// Audio encryption cipher using ChaCha20-Poly1305.
///
/// Two implementations share the wire format (see RealtimeChaChaPoly):
/// - `encrypt`/`decrypt` (CryptoKit, allocating): cold paths, tests, and
///   cross-validation of the realtime core.
/// - `encryptToBuffer`/`decryptFromBuffer` (allocation-free, caller buffers):
///   the realtime IO path. The key schedule is precomputed once here at
///   construction so the hot path touches only stack temporaries.
final class AudioCipher {
    private let key: SymmetricKey
    private let fingerprint: [UInt8]
    private let keyWords: (UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32, UInt32)

    /// Create a new AudioCipher from a 256-bit key
    /// - Parameter keyBytes: 32-byte (256-bit) encryption key
    init(keyBytes: [UInt8]) {
        precondition(keyBytes.count == 32, "Key must be exactly 32 bytes")
        self.key = SymmetricKey(data: keyBytes)
        self.keyWords = keyBytes.withUnsafeBufferPointer { ptr in
            let b = ptr.baseAddress!
            func w(_ i: Int) -> UInt32 {
                UInt32(b[i]) | (UInt32(b[i + 1]) << 8) |
                    (UInt32(b[i + 2]) << 16) | (UInt32(b[i + 3]) << 24)
            }
            return (w(0), w(4), w(8), w(12), w(16), w(20), w(24), w(28))
        }

        // Compute fingerprint as first 8 bytes of SHA256(key)
        var hash = [UInt8](repeating: 0, count: Int(CC_SHA256_DIGEST_LENGTH))
        keyBytes.withUnsafeBytes { keyPtr in
            _ = CC_SHA256(keyPtr.baseAddress, CC_LONG(keyBytes.count), &hash)
        }
        self.fingerprint = Array(hash[0..<8])
    }

    /// Get the key fingerprint (first 8 bytes of SHA256 of key)
    func getFingerprint() -> [UInt8] {
        return fingerprint
    }

    /// Get the fingerprint as a hex string
    func getFingerprintHex() -> String {
        return fingerprint.map { String(format: "%02x", $0) }.joined()
    }

    /// Encrypt audio samples
    /// - Parameters:
    ///   - samples: Audio samples as Float array
    ///   - frameCounter: Unique counter for nonce generation
    /// - Returns: Encrypted ciphertext with authentication tag
    func encrypt(samples: [Float], frameCounter: UInt64) -> [UInt8]? {
        // Convert samples to bytes (native endian)
        let plaintext = samplesToBytes(samples)

        // Create nonce from frame counter (12 bytes)
        // Format: [4 bytes zero padding] [8 bytes frame_counter as big-endian]
        var nonceBytes = [UInt8](repeating: 0, count: 12)
        withUnsafeBytes(of: frameCounter.bigEndian) { fcBytes in
            for (i, byte) in fcBytes.enumerated() {
                nonceBytes[4 + i] = byte
            }
        }

        do {
            let nonce = try ChaChaPoly.Nonce(data: nonceBytes)
            let sealedBox = try ChaChaPoly.seal(plaintext, using: key, nonce: nonce)
            // CryptoKit combines ciphertext and tag - return as single array
            return Array(sealedBox.ciphertext) + Array(sealedBox.tag)
        } catch {
            halLog("Encryption failed: \(error)")
            return nil
        }
    }

    /// Encrypt samples in-place to a buffer
    /// - Parameters:
    ///   - samples: Pointer to audio samples
    ///   - sampleCount: Number of samples
    ///   - frameCounter: Unique counter for nonce generation
    ///   - output: Output buffer (must be at least sampleCount * 4 + 16 bytes)
    /// - Returns: Number of bytes written, or 0 on failure
    func encryptToBuffer(_ samples: UnsafePointer<Float>, sampleCount: Int, frameCounter: UInt64, output: UnsafeMutablePointer<UInt8>, plaintextScratch: inout [UInt8]) -> Int {
        // Allocation-free realtime path (see RealtimeChaChaPoly). The scratch
        // argument is retained for signature compatibility and unused.
        _ = plaintextScratch
        let byteCount = sampleCount * MemoryLayout<Float>.size
        return withUnsafePointer(to: keyWords) { kw in
            kw.withMemoryRebound(to: UInt8.self, capacity: 32) { kb in
                samples.withMemoryRebound(to: UInt8.self, capacity: byteCount) { pt in
                    let ptRaw = UnsafeRawPointer(pt)
                    let outRaw = UnsafeMutableRawPointer(output)
                    let tagRaw = UnsafeMutableRawPointer(outRaw.advanced(by: byteCount))
                    // Copy plaintext into the output region, then seal in
                    // place (input and output may alias exactly).
                    if outRaw != ptRaw {
                        outRaw.copyMemory(from: ptRaw, byteCount: byteCount)
                    }
                    RealtimeChaChaPoly.sealInPlace(
                        keyBytes: kb,
                        bytes: outRaw,
                        byteCount: byteCount,
                        frameCounter: frameCounter,
                        tagOut: tagRaw
                    )
                    return byteCount + kAuthTagSize
                }
            }
        }
    }

    /// Decrypt audio samples
    /// - Parameters:
    ///   - ciphertext: Encrypted data with authentication tag
    ///   - frameCounter: Same counter used during encryption
    /// - Returns: Decrypted samples if authentication succeeds, nil if tampered
    func decrypt(ciphertext: [UInt8], frameCounter: UInt64) -> [Float]? {
        guard ciphertext.count >= kAuthTagSize else {
            halLog("Ciphertext too short")
            return nil
        }

        // Create nonce from frame counter
        var nonceBytes = [UInt8](repeating: 0, count: 12)
        withUnsafeBytes(of: frameCounter.bigEndian) { fcBytes in
            for (i, byte) in fcBytes.enumerated() {
                nonceBytes[4 + i] = byte
            }
        }

        do {
            let nonce = try ChaChaPoly.Nonce(data: nonceBytes)

            // Split ciphertext and tag
            let ctLen = ciphertext.count - kAuthTagSize
            let ct = Array(ciphertext[0..<ctLen])
            let tag = Array(ciphertext[ctLen...])

            let sealedBox = try ChaChaPoly.SealedBox(nonce: nonce, ciphertext: ct, tag: tag)
            let plaintext = try ChaChaPoly.open(sealedBox, using: key)

            // Convert bytes back to samples
            return bytesToSamples(Array(plaintext))
        } catch {
            halLog("Decryption failed: \(error)")
            return nil
        }
    }

    /// Decrypt from buffer in-place
    /// - Parameters:
    ///   - ciphertext: Pointer to encrypted data with tag
    ///   - ciphertextLen: Length of ciphertext including tag
    ///   - frameCounter: Same counter used during encryption
    ///   - output: Output buffer for decrypted samples
    /// - Returns: Number of samples written, or 0 on failure
    func decryptFromBuffer(_ ciphertext: UnsafePointer<UInt8>, ciphertextLen: Int, frameCounter: UInt64, output: UnsafeMutablePointer<Float>) -> Int {
        // Allocation-free realtime path (see RealtimeChaChaPoly).
        guard ciphertextLen >= kAuthTagSize else { return 0 }
        let ctLen = ciphertextLen - kAuthTagSize
        let opened = withUnsafePointer(to: keyWords) { kw in
            kw.withMemoryRebound(to: UInt8.self, capacity: 32) { kb in
                output.withMemoryRebound(to: UInt8.self, capacity: ctLen) { outBytes in
                    RealtimeChaChaPoly.open(
                        keyBytes: kb,
                        ciphertext: UnsafeRawPointer(ciphertext),
                        ciphertextLen: ctLen,
                        frameCounter: frameCounter,
                        tag: UnsafeRawPointer(ciphertext.advanced(by: ctLen)),
                        plaintextOut: UnsafeMutableRawPointer(outBytes)
                    )
                }
            }
        }
        guard opened else { return 0 }
        return ctLen / MemoryLayout<Float>.size
    }

    /// Convert Float samples to bytes (native endian)
    private func samplesToBytes(_ samples: [Float]) -> [UInt8] {
        var bytes = [UInt8](repeating: 0, count: samples.count * MemoryLayout<Float>.size)
        for (i, sample) in samples.enumerated() {
            withUnsafeBytes(of: sample) { sampleBytes in
                for (j, byte) in sampleBytes.enumerated() {
                    bytes[i * 4 + j] = byte
                }
            }
        }
        return bytes
    }

    /// Convert bytes to Float samples (native endian)
    private func bytesToSamples(_ bytes: [UInt8]) -> [Float] {
        let sampleCount = bytes.count / MemoryLayout<Float>.size
        var samples = [Float](repeating: 0, count: sampleCount)
        for i in 0..<sampleCount {
            var value: Float = 0
            withUnsafeMutableBytes(of: &value) { valueBytes in
                for j in 0..<4 {
                    valueBytes[j] = bytes[i * 4 + j]
                }
            }
            samples[i] = value
        }
        return samples
    }
}

/// Encryption key manager
final class EncryptionKeyManager {
    // One maintenance writer publishes into the inactive slot. Audio readers
    // pin a slot before inspecting its reference; reclamation stays here.
    private let controlLock = NSLock()
    private var cipher0: AudioCipher?
    private var cipher1: AudioCipher?
    private var activeSlot: UInt32 = 0
    private var readers0: UInt64 = 0
    private var readers1: UInt64 = 0
    private var pendingCipher: AudioCipher?
    private var hasPendingCipher = false
    private var cipher: AudioCipher? {
        get { sotf_atomic_load_u32(&activeSlot) == 0 ? cipher0 : cipher1 }
        set {
            pendingCipher = newValue
            hasPendingCipher = true
            publishPendingCipher()
        }
    }
    private var keyPath: String
    private var lastMtime: Date?
    private var enabled: Bool = false

    /// Shared instance
    static let shared = EncryptionKeyManager()

    private init() {
        keyPath = getSessionKeyPath()
        loadKey()
    }

    private func publishPendingCipher() {
        guard hasPendingCipher else { return }
        let next: UInt32 = sotf_atomic_load_u32(&activeSlot) == 0 ? 1 : 0
        // Exclude a delayed reader that loaded this slot before publication.
        // Merely observing a zero reader count does not close that race.
        let acquired = next == 0 ? sotf_cipher_try_write(&readers0) : sotf_cipher_try_write(&readers1)
        guard acquired else { return }
        if next == 0 { cipher0 = pendingCipher } else { cipher1 = pendingCipher }
        sotf_atomic_store_u32(&activeSlot, next)
        if next == 0 { sotf_cipher_end_write(&readers0) }
        else { sotf_cipher_end_write(&readers1) }
        pendingCipher = nil
        hasPendingCipher = false
    }

    // Keep reference temporaries and the nonescaping body in a separate stack
    // frame. All ARC releases finish before the caller drops the reader pin.
    @inline(never)
    private func invokeCipher(slot: UInt32, body: (AudioCipher?) -> Int) -> Int {
        body(slot == 0 ? cipher0 : cipher1)
    }

    /// Access a cipher without blocking or releasing its final reference on IO.
    func withCipher(_ body: (AudioCipher?) -> Int) -> Int {
        // Publication can race acquisition. Bounded retries keep IO wait-free
        // with respect to the maintenance writer; a missed key drops this block.
        for _ in 0..<3 {
            let slot = sotf_atomic_load_u32(&activeSlot)
            let pinned = slot == 0 ? sotf_cipher_try_pin(&readers0) : sotf_cipher_try_pin(&readers1)
            guard pinned else { continue }
            if sotf_atomic_load_u32(&activeSlot) == slot {
                let result = invokeCipher(slot: slot, body: body)
                if slot == 0 { sotf_atomic_fetch_sub_u64(&readers0, 1) }
                else { sotf_atomic_fetch_sub_u64(&readers1, 1) }
                return result
            }
            if slot == 0 { sotf_atomic_fetch_sub_u64(&readers0, 1) }
            else { sotf_atomic_fetch_sub_u64(&readers1, 1) }
        }
        return body(nil)
    }

    /// Load the encryption key from file
    private func loadKey() {
        guard FileManager.default.fileExists(atPath: keyPath) else {
            halLog("Key file not found at \(keyPath)")
            cipher = nil
            return
        }

        do {
            // The daemon publishes the session key mode 0600. Refuse anything
            // else — group-readable key files leak the audio session key, so
            // warn-and-continue is the wrong direction here.
            let attrs = try FileManager.default.attributesOfItem(atPath: keyPath)
            if let permissions = attrs[.posixPermissions] as? Int {
                let mode = permissions & 0o777
                guard mode & 0o077 == 0, mode & 0o600 == 0o600 else {
                    halLog("Key file has insecure permissions \(String(mode, radix: 8)) (expected 600); refusing to load")
                    cipher = nil
                    return
                }
            }

            let keyData = try Data(contentsOf: URL(fileURLWithPath: keyPath))
            guard keyData.count == 32 else {
                halLog("Invalid key file size: \(keyData.count) bytes (expected 32)")
                cipher = nil
                return
            }

            let keyBytes = [UInt8](keyData)
            cipher = AudioCipher(keyBytes: keyBytes)
            lastMtime = attrs[.modificationDate] as? Date
            halLog("Loaded encryption key, fingerprint: \(cipher?.getFingerprintHex() ?? "nil")")
        } catch {
            halLog("Failed to load encryption key: \(error)")
            cipher = nil
        }
    }

    /// Check if the key file has changed and reload if necessary
    func checkAndReload() -> Bool {
        controlLock.lock()
        defer { controlLock.unlock() }
        publishPendingCipher()
        let currentPath = getSessionKeyPath()
        if currentPath != keyPath {
            keyPath = currentPath
            lastMtime = nil
            loadKey()
            return true
        }
        guard FileManager.default.fileExists(atPath: keyPath) else {
            if cipher != nil {
                halLog("Key file deleted")
                cipher = nil
                return true
            }
            return false
        }

        if let attrs = try? FileManager.default.attributesOfItem(atPath: keyPath),
           let mtime = attrs[.modificationDate] as? Date {
            if lastMtime == nil || mtime != lastMtime {
                loadKey()
                return true
            }
        }

        return false
    }

    /// Reload the key location after test or lab tooling changes the environment.
    func reloadFromCurrentEnvironment() {
        controlLock.lock()
        defer { controlLock.unlock() }
        keyPath = getSessionKeyPath()
        lastMtime = nil
        loadKey()
    }

    /// Get the current cipher (may be nil if key not loaded)
    func getCipher() -> AudioCipher? {
        // Cold-path API only. Realtime users must hold a withCipher pin.
        controlLock.lock()
        defer { controlLock.unlock() }
        return cipher
    }

    /// Get the key fingerprint
    func getFingerprint() -> [UInt8]? {
        return getCipher()?.getFingerprint()
    }

    /// Check if encryption is enabled AND cipher is available
    ///
    /// Returns true only if both `enabled` flag is set AND a valid cipher
    /// has been loaded. This prevents the case where encryption is "enabled"
    /// but no key is available.
    var isEnabled: Bool {
        get {
            controlLock.lock()
            defer { controlLock.unlock() }
            return enabled && cipher != nil
        }
        set {
            controlLock.lock()
            defer { controlLock.unlock() }
            enabled = newValue
            if newValue && cipher == nil {
                halLog("Warning: Encryption enabled but no cipher available (key may not be loaded)")
            } else {
                halLog("Encryption \(enabled ? "enabled" : "disabled")")
            }
        }
    }

    /// Check if encryption is available (cipher loaded, regardless of enabled state)
    var isAvailable: Bool {
        return getCipher() != nil
    }
}
