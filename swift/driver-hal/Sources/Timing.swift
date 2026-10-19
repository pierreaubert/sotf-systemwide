// Timing.swift - Clock and timing utilities for SotF HAL Driver

import Foundation

/// Fixed-lifetime atomic storage; a control update never writes the active bank.
private final class ClockSnapshot {
    private var sequence: UInt64 = 0
    private var hostTime: UInt64 = 0
    private var sampleBits: UInt64 = 0
    private var rateBits: UInt64 = Float64(48000).bitPattern
    private var seed: UInt64 = 1

    func store(host: UInt64, sample: Float64, rate: Float64, clockSeed: UInt64) {
        let version = sotf_clock_load(&sequence)
        sotf_clock_store(&sequence, version &+ 1)
        sotf_clock_store(&hostTime, host)
        sotf_clock_store(&sampleBits, sample.bitPattern)
        sotf_clock_store(&rateBits, rate.bitPattern)
        sotf_clock_store(&seed, clockSeed)
        sotf_clock_store(&sequence, version &+ 2)
    }

    func read() -> (UInt64, Float64, Float64, UInt64)? {
        let version = sotf_clock_load(&sequence)
        guard version & 1 == 0 else { return nil }
        let host = sotf_clock_load(&hostTime)
        let sample = sotf_clock_load(&sampleBits)
        let rate = sotf_clock_load(&rateBits)
        let clockSeed = sotf_clock_load(&seed)
        guard sotf_clock_load(&sequence) == version else { return nil }
        return (host, Float64(bitPattern: sample), Float64(bitPattern: rate), clockSeed)
    }
}

/// Manages the driver's clock for synchronization with Core Audio
final class DriverClock {
    private let lock = NSLock()
    private var anchorHostTime: UInt64 = 0
    private var anchorSampleTime: Float64 = 0
    private var sampleRate: Float64 = 48000.0
    private var clockSeed: UInt64 = 1
    private var isRunning: Bool = false
    private let snapshot0 = ClockSnapshot()
    private let snapshot1 = ClockSnapshot()
    private var activeSnapshot: UInt32 = 0

    /// Called only by the serialized control writer. Payload is atomic too,
    /// so a failed snapshot attempt never introduces a Swift data race.
    private func publishSnapshot() {
        let next: UInt32 = sotf_atomic_load_u32(&activeSnapshot) == 0 ? 1 : 0
        let snapshot = next == 0 ? snapshot0 : snapshot1
        snapshot.store(host: anchorHostTime, sample: anchorSampleTime, rate: sampleRate, clockSeed: clockSeed)
        sotf_atomic_store_u32(&activeSnapshot, next)
    }

    private func readSnapshot() -> (UInt64, Float64, Float64, UInt64)? {
        for _ in 0..<8 {
            let slot = sotf_atomic_load_u32(&activeSnapshot)
            let snapshot = slot == 0 ? snapshot0 : snapshot1
            if let value = snapshot.read(), sotf_atomic_load_u32(&activeSnapshot) == slot { return value }
        }
        return nil
    }

    private static var timebaseInfo: mach_timebase_info_data_t = {
        var info = mach_timebase_info_data_t()
        mach_timebase_info(&info)
        return info
    }()

    init() {
        // Initialize Swift's lazy static and the Mach timebase off the callback.
        _ = Self.timebaseInfo
    }

    /// Reset the clock anchor when IO starts
    func start(sampleRate: Float64) {
        guard sampleRate.isFinite, sampleRate > 0, sampleRate <= 192000 else { return }
        lock.lock()
        defer { lock.unlock() }

        self.sampleRate = sampleRate
        anchorHostTime = mach_absolute_time()
        anchorSampleTime = 0
        clockSeed += 1
        isRunning = true
        publishSnapshot()
    }

    /// Stop the clock
    func stop() {
        lock.lock()
        defer { lock.unlock() }

        isRunning = false
        publishSnapshot()
    }

    /// Update sample rate (triggers clock seed change)
    func setSampleRate(_ rate: Float64) {
        guard rate.isFinite, rate > 0, rate <= 192000 else { return }
        lock.lock()
        defer { lock.unlock() }

        if rate != sampleRate {
            let currentHostTime = mach_absolute_time()
            anchorSampleTime = getCurrentSampleTimeLocked(at: currentHostTime)
            sampleRate = rate
            anchorHostTime = currentHostTime
            clockSeed += 1
            publishSnapshot()
        }
    }

    /// Convert host time to nanoseconds
    private func hostTimeToNanoseconds(_ hostTime: UInt64) -> UInt64 {
        return hostTime * UInt64(Self.timebaseInfo.numer) / UInt64(Self.timebaseInfo.denom)
    }

    /// Get current sample time for a given host time
    private func getCurrentSampleTimeLocked(at hostTime: UInt64) -> Float64 {
        guard hostTime >= anchorHostTime else { return anchorSampleTime }

        let hostTimeDelta = hostTime - anchorHostTime
        let nanoseconds = hostTimeToNanoseconds(hostTimeDelta)
        let seconds = Double(nanoseconds) / 1_000_000_000.0
        let samplesDelta = seconds * sampleRate

        return anchorSampleTime + samplesDelta
    }

    /// Get the zero timestamp for Core Audio synchronization
    /// Returns (sampleTime, hostTime, seed)
    func getZeroTimeStamp(period: UInt32) -> (Float64, UInt64, UInt64)? {
        guard let (anchorHostTime, anchorSampleTime, sampleRate, clockSeed) = readSnapshot() else {
            // A preempted control writer cannot make CoreAudio wait on a lock.
            return nil
        }

        let currentHostTime = mach_absolute_time()
        let elapsed = currentHostTime >= anchorHostTime ? currentHostTime - anchorHostTime : 0
        let seconds = Double(elapsed) * Double(Self.timebaseInfo.numer)
            / Double(Self.timebaseInfo.denom) / 1_000_000_000.0
        let currentSampleTime = anchorSampleTime + seconds * sampleRate

        // Zero timestamps advance by kAudioDevicePropertyZeroTimeStampPeriod,
        // not by the IO buffer size.
        let periodFrames = Float64(max(period, 1))
        let zeroSampleTime = floor(currentSampleTime / periodFrames) * periodFrames

        // Calculate the host time that corresponds to the zero sample time.
        // Signed math throughout: after setSampleRate re-anchors at an
        // unaligned sample time, the period floor can land *before* the
        // anchor, making the offset negative. Converting that to UInt64
        // trapped (EXC_BREAKPOINT) and crash-looped the driver host until
        // coreaudiod deactivated the device (2026-10-17).
        let sampleOffset = zeroSampleTime - anchorSampleTime
        let secondsOffset = sampleOffset / sampleRate
        let nanosOffset = Int64(secondsOffset * 1_000_000_000.0)
        let hostOffset = nanosOffset * Int64(Self.timebaseInfo.denom) / Int64(Self.timebaseInfo.numer)
        let zeroHostTime = UInt64(max(Int64(0), Int64(bitPattern: anchorHostTime) + hostOffset))

        return (zeroSampleTime, zeroHostTime, clockSeed)
    }

    /// Get current sample time
    func getCurrentSampleTime() -> Float64 {
        lock.lock()
        defer { lock.unlock() }

        return getCurrentSampleTimeLocked(at: mach_absolute_time())
    }

    /// Get the clock seed (changes when timing is reset)
    func getSeed() -> UInt64 {
        lock.lock()
        defer { lock.unlock() }

        return clockSeed
    }

    var running: Bool {
        lock.lock()
        defer { lock.unlock() }

        return isRunning
    }
}
