// Compile without SOTF_HAL_TESTING to exercise the production attachment policy.
import Darwin
import Foundation

@main
struct DescriptorProbe {
    static func main() {
        let audio = SharedAudioBuffer()
        let connected = audio.initialize(sampleRate: 48_000, bufferFrames: 64, channelCount: 2)
        audio.closeSharedMemory()
        exit(connected ? 0 : 1)
    }
}
