import SwiftUI
import ConfigBarModels

// MARK: - Plugin Editor Views

/// Router that picks the right editor view for a plugin type
struct PluginEditorView: View {
    let pluginType: String
    let parameters: [String: Any]
    let descriptors: [PluginParameterDescriptor]
    let channelCount: Int
    let onUpdate: ([String: Any]) -> Void

    init(
        pluginType: String,
        parameters: [String: Any],
        descriptors: [PluginParameterDescriptor],
        channelCount: Int = 2,
        onUpdate: @escaping ([String: Any]) -> Void
    ) {
        self.pluginType = pluginType
        self.parameters = parameters
        self.descriptors = descriptors
        self.channelCount = channelCount
        self.onUpdate = onUpdate
    }

    var body: some View {
        if !descriptors.isEmpty && pluginType != "eq" && pluginType != "delay" {
            DescriptorPluginEditor(
                pluginType: pluginType,
                parameters: parameters,
                descriptors: descriptors,
                onUpdate: onUpdate
            )
        } else {
            switch pluginType {
            case "eq":
                EQEditor(
                    parameters: parameters,
                    channelCount: channelCount,
                    onUpdate: onUpdate
                )
            case "delay":
                DelayEditor(
                    parameters: parameters,
                    channelCount: channelCount,
                    onUpdate: onUpdate
                )
            default:
                GenericPluginEditor(pluginType: pluginType, parameters: parameters, onUpdate: onUpdate)
            }
        }
    }
}

// MARK: - Gain Editor

struct GainEditor: View {
    let parameters: [String: Any]
    let onUpdate: ([String: Any]) -> Void

    @State private var gainDb: Double = 0.0

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Gain:")
                    .frame(width: 80, alignment: .trailing)
                Slider(value: $gainDb, in: -60...20, step: 0.1)
                    .onChange(of: gainDb) { newValue in
                        onUpdate(["gain_db": newValue])
                    }
                Text("\(gainDb, specifier: "%.1f") dB")
                    .frame(width: 70, alignment: .trailing)
                    .monospacedDigit()
            }
        }
        .onAppear {
            gainDb = parameters["gain_db"] as? Double ?? 0.0
        }
    }
}

// MARK: - Delay Editor

/// Delay editor with a uniform/per-channel mode switch. Uniform mode delays
/// every channel by one time (echo/ambience). Per-channel mode assigns an
/// independent time per channel for speaker time alignment; the DSP runs it
/// as a pure routing delay, so the effect controls are hidden and their
/// values are reset to the engine-accepted set when per-channel is applied.
struct DelayEditor: View {
    let parameters: [String: Any]
    let channelCount: Int
    let onUpdate: ([String: Any]) -> Void

    @State private var draftParameters: [String: Any] = [:]
    @State private var perChannel = false
    @State private var uniformMs = 0.0
    @State private var channelMs: [Double] = []
    @State private var feedback = 0.3
    @State private var mix = 0.5
    @State private var lfoRateHz = 0.0
    @State private var lfoDepthMs = 0.0
    @State private var allpassCoeff = 0.5
    @State private var allpassFeedback = false
    @State private var pitchPreserving = false

    private var safeChannelCount: Int {
        min(max(channelCount, 1), ConfigBarDelayParameters.maxChannels)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Text("Mode")
                    .frame(width: 150, alignment: .trailing)
                    .font(.caption)
                Picker("", selection: $perChannel) {
                    Text("Same for all channels").tag(false)
                    Text("Per channel").tag(true)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 300)
                .onChange(of: perChannel) { _ in applyModeChange() }
            }

            if perChannel {
                VStack(alignment: .leading, spacing: 6) {
                    Text("CHANNEL DELAYS")
                        .font(.caption.weight(.semibold))
                        .foregroundColor(.secondary)
                    ForEach(channelMs.indices, id: \.self) { index in
                        delaySliderRow(
                            name: "Channel \(index + 1)",
                            value: Binding(
                                get: { channelMs[index] },
                                set: {
                                    channelMs[index] = $0
                                    emitPerChannel()
                                }
                            ),
                            doc: nil
                        )
                    }
                }
                Text("Per-channel mode is a pure time alignment: feedback, mix, modulation and allpass are reset so the engine accepts the node.")
                    .font(.caption2)
                    .foregroundColor(.secondary)
                    .padding(.leading, 158)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    Text("TIME")
                        .font(.caption.weight(.semibold))
                        .foregroundColor(.secondary)
                    delaySliderRow(
                        name: "Delay",
                        value: Binding(
                            get: { uniformMs },
                            set: {
                                uniformMs = $0
                                emitUniform()
                            }
                        ),
                        doc: "Delay time applied to every channel"
                    )
                }
                VStack(alignment: .leading, spacing: 6) {
                    Text("EFFECT")
                        .font(.caption.weight(.semibold))
                        .foregroundColor(.secondary)
                    scalarSliderRow(
                        name: "Feedback",
                        value: $feedback,
                        in: -0.95...0.95,
                        step: 0.01,
                        format: "%.2f",
                        unit: "",
                        doc: "Amount fed back into the delay line"
                    )
                    scalarSliderRow(
                        name: "Mix",
                        value: $mix,
                        in: 0.0...1.0,
                        step: 0.01,
                        format: "%.0f",
                        unit: " %",
                        scale: 100.0,
                        doc: "Dry/wet blend"
                    )
                    scalarSliderRow(
                        name: "LFO Rate",
                        value: $lfoRateHz,
                        in: 0.0...20.0,
                        step: 0.1,
                        format: "%.1f",
                        unit: " Hz",
                        doc: "Modulation oscillator speed"
                    )
                    scalarSliderRow(
                        name: "LFO Depth",
                        value: $lfoDepthMs,
                        in: 0.0...10.0,
                        step: 0.1,
                        format: "%.2f",
                        unit: " ms",
                        doc: "Modulation amount on delay time"
                    )
                    scalarSliderRow(
                        name: "Allpass Coeff",
                        value: $allpassCoeff,
                        in: 0.0...0.99,
                        step: 0.01,
                        format: "%.2f",
                        unit: "",
                        doc: "Allpass filter coefficient"
                    )
                    toggleRow(
                        name: "Allpass Feedback",
                        value: $allpassFeedback,
                        doc: "Use allpass filter in feedback path"
                    )
                    toggleRow(
                        name: "Pitch Preserving",
                        value: $pitchPreserving,
                        doc: "Fixed read heads for pitch-preserving delay changes; requires zero LFO rate and depth"
                    )
                }
            }
        }
        .onAppear {
            draftParameters = parameters
            perChannel = ConfigBarDelayParameters.isPerChannel(parameters)
            uniformMs = ConfigBarDelayParameters.scalarDelayMs(parameters)
            channelMs = ConfigBarDelayParameters.channelDelays(
                parameters,
                channelCount: safeChannelCount
            )
            seedScalars(from: parameters)
        }
        .onChange(of: channelCount) { _ in
            channelMs = ConfigBarDelayParameters.channelDelays(
                draftParameters,
                channelCount: safeChannelCount
            )
            if perChannel {
                emitPerChannel()
            }
        }
    }

    private func seedScalars(from source: [String: Any]) {
        feedback = clampedDouble(source["feedback"], in: -0.95...0.95, fallback: 0.3)
        mix = clampedDouble(source["mix"], in: 0.0...1.0, fallback: 0.5)
        lfoRateHz = clampedDouble(source["lfo_rate_hz"], in: 0.0...20.0, fallback: 0.0)
        lfoDepthMs = clampedDouble(source["lfo_depth_ms"], in: 0.0...10.0, fallback: 0.0)
        allpassCoeff = clampedDouble(source["allpass_coeff"], in: 0.0...0.99, fallback: 0.5)
        allpassFeedback = source["allpass_feedback"] as? Bool ?? false
        pitchPreserving = source["pitch_preserving"] as? Bool ?? false
    }

    private func clampedDouble(
        _ raw: Any?,
        in range: ClosedRange<Double>,
        fallback: Double
    ) -> Double {
        guard let value = ConfigBarDelayParameters.doubleValue(raw), value.isFinite else {
            return fallback
        }
        return min(max(value, range.lowerBound), range.upperBound)
    }

    private func emitUniform() {
        draftParameters[ConfigBarDelayParameters.scalarDelayKey] = uniformMs
        draftParameters.removeValue(forKey: ConfigBarDelayParameters.channelDelaysKey)
        syncScalarKeys()
        onUpdate(draftParameters)
    }

    private func emitScalars() {
        syncScalarKeys()
        onUpdate(draftParameters)
    }

    private func syncScalarKeys() {
        draftParameters["feedback"] = feedback
        draftParameters["mix"] = mix
        draftParameters["lfo_rate_hz"] = lfoRateHz
        draftParameters["lfo_depth_ms"] = lfoDepthMs
        draftParameters["allpass_coeff"] = allpassCoeff
        draftParameters["allpass_feedback"] = allpassFeedback
        draftParameters["pitch_preserving"] = pitchPreserving
    }

    private func emitPerChannel() {
        draftParameters = ConfigBarDelayParameters.applyingPerChannelDelays(
            draftParameters,
            delaysMs: channelMs
        )
        seedScalars(from: draftParameters)
        onUpdate(draftParameters)
    }

    private func applyModeChange() {
        if perChannel {
            channelMs = ConfigBarDelayParameters.channelDelays(
                draftParameters,
                channelCount: safeChannelCount
            )
            emitPerChannel()
        } else {
            uniformMs = channelMs.first ?? uniformMs
            draftParameters = ConfigBarDelayParameters.applyingUniformDelay(
                draftParameters,
                delayMs: uniformMs
            )
            onUpdate(draftParameters)
        }
    }

    @ViewBuilder
    private func delaySliderRow(
        name: String,
        value: Binding<Double>,
        doc: String?
    ) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 8) {
                Text(name)
                    .frame(width: 150, alignment: .trailing)
                    .font(.caption)
                Slider(
                    value: value,
                    in: 0.0...ConfigBarDelayParameters.maxDelayMs,
                    step: 0.1
                )
                Text("\(value.wrappedValue, specifier: "%.2f") ms")
                    .frame(width: 100, alignment: .trailing)
                    .monospacedDigit()
            }
            if let doc {
                Text(doc)
                    .font(.caption2)
                    .foregroundColor(.secondary)
                    .padding(.leading, 158)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .padding(.vertical, doc == nil ? 1 : 3)
    }

    @ViewBuilder
    private func scalarSliderRow(
        name: String,
        value: Binding<Double>,
        in range: ClosedRange<Double>,
        step: Double,
        format: String,
        unit: String,
        scale: Double = 1.0,
        doc: String?
    ) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 8) {
                Text(name)
                    .frame(width: 150, alignment: .trailing)
                    .font(.caption)
                Slider(value: value, in: range, step: step)
                    .onChange(of: value.wrappedValue) { _ in emitScalars() }
                Text("\(value.wrappedValue * scale, specifier: format)\(unit)")
                    .frame(width: 100, alignment: .trailing)
                    .monospacedDigit()
            }
            if let doc {
                Text(doc)
                    .font(.caption2)
                    .foregroundColor(.secondary)
                    .padding(.leading, 158)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .padding(.vertical, 3)
    }

    @ViewBuilder
    private func toggleRow(name: String, value: Binding<Bool>, doc: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 8) {
                Text(name)
                    .frame(width: 150, alignment: .trailing)
                    .font(.caption)
                Toggle("", isOn: value)
                    .labelsHidden()
                    .onChange(of: value.wrappedValue) { _ in emitScalars() }
            }
            Text(doc)
                .font(.caption2)
                .foregroundColor(.secondary)
                .padding(.leading, 158)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 3)
    }
}

// MARK: - EQ Editor

/// EQ editor with a uniform/per-channel mode switch, mirroring the delay
/// editor. Uniform mode applies one filter list to every channel.
/// Per-channel mode keeps an independent filter list per channel; the DSP
/// requires one entry per node channel, so the lists are sized to the node
/// channel count and switching modes reuses the uniform list as the seed.
struct EQEditor: View {
    let parameters: [String: Any]
    let channelCount: Int
    let onUpdate: ([String: Any]) -> Void

    @State private var draftParameters: [String: Any] = [:]
    @State private var perChannel = false
    @State private var uniformFilters: [[String: Any]] = []
    @State private var channelFilters: [[[String: Any]]] = []

    private var safeChannelCount: Int {
        min(max(channelCount, 1), ConfigBarEQParameters.maxChannels)
    }

    private var canAddUniformBand: Bool {
        uniformFilters.count < ConfigBarEQParameters.maxFilters
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Text("Mode")
                    .frame(width: 150, alignment: .trailing)
                    .font(.caption)
                Picker("", selection: $perChannel) {
                    Text("Same for all channels").tag(false)
                    Text("Per channel").tag(true)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 300)
                .onChange(of: perChannel) { _ in applyModeChange() }
            }

            if perChannel {
                VStack(alignment: .leading, spacing: 8) {
                    Text("CHANNEL EQUALIZERS")
                        .font(.caption.weight(.semibold))
                        .foregroundColor(.secondary)
                    ForEach(channelFilters.indices, id: \.self) { channel in
                        GroupBox("Channel \(channel + 1)") {
                            VStack(alignment: .leading, spacing: 6) {
                                if channelFilters[channel].isEmpty {
                                    Text("No bands — audio passes through.")
                                        .font(.caption)
                                        .foregroundColor(.secondary)
                                }
                                filterRows(filters: channelFilters[channel], channel: channel)
                                Button(action: { addBand(to: channel) }) {
                                    Label("Add Band to Channel \(channel + 1)", systemImage: "plus.circle")
                                }
                                .buttonStyle(.borderless)
                                .disabled(channelFilters[channel].count >= ConfigBarEQParameters.maxFilters)
                            }
                        }
                    }
                    Button(action: copyFirstChannelToAll) {
                        Label("Copy Channel 1 to all channels", systemImage: "doc.on.doc")
                    }
                    .buttonStyle(.borderless)
                    .disabled(channelFilters.isEmpty)
                }
                Text("Per-channel mode sends one filter list per channel; the engine requires one entry per channel.")
                    .font(.caption2)
                    .foregroundColor(.secondary)
                    .padding(.leading, 158)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    Text("EQUALIZER (ALL CHANNELS)")
                        .font(.caption.weight(.semibold))
                        .foregroundColor(.secondary)
                    if uniformFilters.isEmpty {
                        Text("No bands — audio passes through. Add a band.")
                            .font(.caption)
                            .foregroundColor(.secondary)
                    }
                    filterRows(filters: uniformFilters, channel: nil)
                    Button(action: addUniformBand) {
                        Label("Add Band", systemImage: "plus.circle")
                    }
                    .buttonStyle(.borderless)
                    .disabled(!canAddUniformBand)
                }
            }
        }
        .onAppear {
            draftParameters = parameters
            perChannel = ConfigBarEQParameters.isPerChannel(parameters)
            uniformFilters = ConfigBarEQParameters.uniformFilters(parameters)
            channelFilters = ConfigBarEQParameters.channelFilters(
                parameters,
                channelCount: safeChannelCount
            )
        }
        .onChange(of: channelCount) { _ in
            channelFilters = ConfigBarEQParameters.channelFilters(
                draftParameters,
                channelCount: safeChannelCount
            )
            if perChannel {
                emitPerChannel()
            }
        }
    }

    @ViewBuilder
    private func filterRows(filters rows: [[String: Any]], channel: Int?) -> some View {
        ForEach(Array(rows.enumerated()), id: \.offset) { index, filter in
            EQBandRow(
                index: index,
                filter: filter,
                onUpdate: { updatedFilter in
                    if let channel {
                        guard channelFilters.indices.contains(channel),
                              channelFilters[channel].indices.contains(index) else { return }
                        channelFilters[channel][index] = updatedFilter
                        emitPerChannel()
                    } else {
                        guard uniformFilters.indices.contains(index) else { return }
                        uniformFilters[index] = updatedFilter
                        emitUniform()
                    }
                },
                onRemove: {
                    if let channel {
                        guard channelFilters.indices.contains(channel),
                              channelFilters[channel].indices.contains(index) else { return }
                        channelFilters[channel].remove(at: index)
                        emitPerChannel()
                    } else {
                        guard uniformFilters.indices.contains(index) else { return }
                        uniformFilters.remove(at: index)
                        emitUniform()
                    }
                }
            )
        }
    }

    private func defaultBand() -> [String: Any] {
        [
            "filter_type": "peak",
            "freq": 1000.0,
            "q": 1.0,
            "db_gain": 0.0
        ]
    }

    private func addUniformBand() {
        guard canAddUniformBand else { return }
        uniformFilters.append(defaultBand())
        emitUniform()
    }

    private func addBand(to channel: Int) {
        guard channelFilters.indices.contains(channel),
              channelFilters[channel].count < ConfigBarEQParameters.maxFilters else { return }
        channelFilters[channel].append(defaultBand())
        emitPerChannel()
    }

    private func copyFirstChannelToAll() {
        guard let first = channelFilters.first else { return }
        channelFilters = Array(repeating: first, count: channelFilters.count)
        emitPerChannel()
    }

    private func emitUniform() {
        draftParameters = ConfigBarEQParameters.applyingUniformFilters(
            draftParameters,
            filters: uniformFilters
        )
        onUpdate(draftParameters)
    }

    private func emitPerChannel() {
        draftParameters = ConfigBarEQParameters.applyingPerChannelFilters(
            draftParameters,
            channelFilters: channelFilters
        )
        onUpdate(draftParameters)
    }

    private func applyModeChange() {
        if perChannel {
            channelFilters = ConfigBarEQParameters.channelFilters(
                draftParameters,
                channelCount: safeChannelCount
            )
            emitPerChannel()
        } else {
            uniformFilters = channelFilters.first ?? uniformFilters
            draftParameters = ConfigBarEQParameters.applyingUniformFilters(
                draftParameters,
                filters: uniformFilters
            )
            onUpdate(draftParameters)
        }
    }
}

struct EQBandRow: View {
    let index: Int
    let filter: [String: Any]
    let onUpdate: ([String: Any]) -> Void
    let onRemove: () -> Void

    @State private var filterType: String = "peak"
    @State private var frequency: Double = 1000.0
    @State private var q: Double = 1.0
    @State private var gainDb: Double = 0.0
    @State private var isLoaded = false
    @State private var usesRoomEQKeys = false

    let filterTypes = ["peak", "lowshelf", "highshelf", "lowpass", "highpass", "notch", "bandpass"]

    var body: some View {
        HStack(spacing: 6) {
            Text("#\(index + 1)")
                .frame(width: 25)
                .foregroundColor(.secondary)

            Picker("", selection: $filterType) {
                ForEach(filterTypes, id: \.self) { type_ in
                    Text(type_.capitalized).tag(type_)
                }
            }
            .frame(width: 90)
            .onChange(of: filterType) { _ in emitUpdate() }

            Text("Hz:")
            TextField("", value: $frequency, format: .number)
                .frame(width: 60)
                .onChange(of: frequency) { _ in emitUpdate() }

            Text("Q:")
            TextField("", value: $q, format: .number)
                .frame(width: 45)
                .onChange(of: q) { _ in emitUpdate() }

            Text("dB:")
            TextField("", value: $gainDb, format: .number)
                .frame(width: 50)
                .onChange(of: gainDb) { _ in emitUpdate() }

            Button(action: onRemove) {
                Image(systemName: "minus.circle")
                    .foregroundColor(.red)
            }
            .buttonStyle(.borderless)
        }
        .onAppear {
            usesRoomEQKeys = filter["freq"] != nil || filter["db_gain"] != nil
            filterType = filter["filter_type"] as? String ?? "peak"
            frequency = filter["freq"] as? Double
                ?? filter["frequency"] as? Double
                ?? 1000.0
            q = filter["q"] as? Double ?? 1.0
            gainDb = filter["db_gain"] as? Double
                ?? filter["gain_db"] as? Double
                ?? 0.0
            DispatchQueue.main.async {
                isLoaded = true
            }
        }
    }

    private func emitUpdate() {
        guard isLoaded else { return }
        if usesRoomEQKeys {
            onUpdate([
                "filter_type": filterType,
                "freq": frequency,
                "q": q,
                "db_gain": gainDb
            ] as [String: Any])
        } else {
            onUpdate([
                "filter_type": filterType,
                "frequency": frequency,
                "q": q,
                "gain_db": gainDb
            ] as [String: Any])
        }
    }
}

// MARK: - Compressor Editor

struct CompressorEditor: View {
    let parameters: [String: Any]
    let onUpdate: ([String: Any]) -> Void

    @State private var threshold: Double = -20.0
    @State private var ratio: Double = 4.0
    @State private var attack: Double = 5.0
    @State private var release: Double = 50.0
    @State private var knee: Double = 6.0
    @State private var makeupGain: Double = 0.0
    @State private var mix: Double = 1.0

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            paramSlider("Threshold:", value: $threshold, range: -60...0, unit: "dB")
            paramSlider("Ratio:", value: $ratio, range: 1...20, unit: ":1")
            paramSlider("Attack:", value: $attack, range: 0.1...100, unit: "ms")
            paramSlider("Release:", value: $release, range: 10...1000, unit: "ms")
            paramSlider("Knee:", value: $knee, range: 0...20, unit: "dB")
            paramSlider("Makeup:", value: $makeupGain, range: -24...24, unit: "dB")
            paramSlider("Mix:", value: $mix, range: 0...1, unit: "")
        }
        .onAppear { loadParams() }
    }

    private func loadParams() {
        threshold = parameters["threshold_db"] as? Double ?? -20.0
        ratio = parameters["ratio"] as? Double ?? 4.0
        attack = parameters["attack_ms"] as? Double ?? 5.0
        release = parameters["release_ms"] as? Double ?? 50.0
        knee = parameters["knee_db"] as? Double ?? 6.0
        makeupGain = parameters["makeup_gain_db"] as? Double ?? 0.0
        mix = parameters["mix"] as? Double ?? 1.0
    }

    private func emitUpdate() {
        onUpdate([
            "threshold_db": threshold,
            "ratio": ratio,
            "attack_ms": attack,
            "release_ms": release,
            "knee_db": knee,
            "makeup_gain_db": makeupGain,
            "mix": mix,
        ] as [String: Any])
    }

    private func paramSlider(_ label: String, value: Binding<Double>, range: ClosedRange<Double>, unit: String) -> some View {
        HStack {
            Text(label)
                .frame(width: 80, alignment: .trailing)
            Slider(value: value, in: range)
                .onChange(of: value.wrappedValue) { _ in emitUpdate() }
            Text("\(value.wrappedValue, specifier: "%.1f") \(unit)")
                .frame(width: 80, alignment: .trailing)
                .monospacedDigit()
        }
    }
}

// MARK: - Limiter Editor

struct LimiterEditor: View {
    let parameters: [String: Any]
    let onUpdate: ([String: Any]) -> Void

    @State private var threshold: Double = -0.1
    @State private var release: Double = 50.0
    @State private var lookahead: Double = 5.0

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            paramSlider("Threshold:", value: $threshold, range: -20...0, unit: "dB")
            paramSlider("Release:", value: $release, range: 10...1000, unit: "ms")
            paramSlider("Lookahead:", value: $lookahead, range: 0...20, unit: "ms")
        }
        .onAppear {
            threshold = parameters["threshold_db"] as? Double ?? -0.1
            release = parameters["release_ms"] as? Double ?? 50.0
            lookahead = parameters["lookahead_ms"] as? Double ?? 5.0
        }
    }

    private func emitUpdate() {
        onUpdate([
            "threshold_db": threshold,
            "release_ms": release,
            "lookahead_ms": lookahead,
        ] as [String: Any])
    }

    private func paramSlider(_ label: String, value: Binding<Double>, range: ClosedRange<Double>, unit: String) -> some View {
        HStack {
            Text(label)
                .frame(width: 80, alignment: .trailing)
            Slider(value: value, in: range)
                .onChange(of: value.wrappedValue) { _ in emitUpdate() }
            Text("\(value.wrappedValue, specifier: "%.1f") \(unit)")
                .frame(width: 80, alignment: .trailing)
                .monospacedDigit()
        }
    }
}

// MARK: - Gate Editor

struct GateEditor: View {
    let parameters: [String: Any]
    let onUpdate: ([String: Any]) -> Void

    @State private var threshold: Double = -40.0
    @State private var ratio: Double = 10.0
    @State private var attack: Double = 1.0
    @State private var hold: Double = 10.0
    @State private var release: Double = 100.0

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            paramSlider("Threshold:", value: $threshold, range: -80...0, unit: "dB")
            paramSlider("Ratio:", value: $ratio, range: 1...100, unit: ":1")
            paramSlider("Attack:", value: $attack, range: 0.1...50, unit: "ms")
            paramSlider("Hold:", value: $hold, range: 0...1000, unit: "ms")
            paramSlider("Release:", value: $release, range: 10...2000, unit: "ms")
        }
        .onAppear {
            threshold = parameters["threshold_db"] as? Double ?? -40.0
            ratio = parameters["ratio"] as? Double ?? 10.0
            attack = parameters["attack_ms"] as? Double ?? 1.0
            hold = parameters["hold_ms"] as? Double ?? 10.0
            release = parameters["release_ms"] as? Double ?? 100.0
        }
    }

    private func emitUpdate() {
        onUpdate([
            "threshold_db": threshold,
            "ratio": ratio,
            "attack_ms": attack,
            "hold_ms": hold,
            "release_ms": release,
        ] as [String: Any])
    }

    private func paramSlider(_ label: String, value: Binding<Double>, range: ClosedRange<Double>, unit: String) -> some View {
        HStack {
            Text(label)
                .frame(width: 80, alignment: .trailing)
            Slider(value: value, in: range)
                .onChange(of: value.wrappedValue) { _ in emitUpdate() }
            Text("\(value.wrappedValue, specifier: "%.1f") \(unit)")
                .frame(width: 80, alignment: .trailing)
                .monospacedDigit()
        }
    }
}

// MARK: - Descriptor-Driven Plugin Editor

struct DescriptorPluginEditor: View {
    let pluginType: String
    let parameters: [String: Any]
    let descriptors: [PluginParameterDescriptor]
    let onUpdate: ([String: Any]) -> Void

    @State private var draftParameters: [String: Any] = [:]

    private var visibleDescriptors: [PluginParameterDescriptor] {
        switch pluginType {
        case "crossfeed":
            return crossfeedVisibleDescriptors
        case "multiband_compressor", "multiband_expander":
            return multibandVisibleDescriptors
        case "upmixer":
            return descriptors.filter { $0.key != "speaker_config" }
        default:
            return descriptors
        }
    }

    private var crossfeedVisibleDescriptors: [PluginParameterDescriptor] {
        let modeIndex = crossfeedAlgorithmModeIndex()
        return descriptors.filter { descriptor in
            if descriptor.key == "mode" || descriptor.key == "crossfeed_mode" {
                return false
            }

            switch descriptor.group.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
            case "bauer":
                return modeIndex == 1
            case "meier":
                return modeIndex == 2
            case "multiband", "multibands":
                return modeIndex == 3
            default:
                return true
            }
        }
    }

    private var multibandVisibleDescriptors: [PluginParameterDescriptor] {
        let activeCrossoverCount = max(multibandBandCount() - 1, 0)
        return descriptors.filter { descriptor in
            guard let crossoverIndex = crossoverFrequencyIndex(for: descriptor.key) else {
                return true
            }
            return crossoverIndex <= activeCrossoverCount
        }
    }

    private var speakerConfigDescriptor: PluginParameterDescriptor? {
        descriptors.first { $0.key == "speaker_config" }
    }

    private var descriptorGroups: [(String, [PluginParameterDescriptor])] {
        var groupOrder: [String] = []
        var groupedDescriptors: [String: [PluginParameterDescriptor]] = [:]

        for descriptor in visibleDescriptors {
            let trimmedGroup = descriptor.group.trimmingCharacters(in: .whitespacesAndNewlines)
            let groupName = trimmedGroup.isEmpty ? "General" : trimmedGroup
            if groupedDescriptors[groupName] == nil {
                groupOrder.append(groupName)
                groupedDescriptors[groupName] = []
            }
            groupedDescriptors[groupName, default: []].append(descriptor)
        }

        return groupOrder.compactMap { groupName in
            guard let descriptors = groupedDescriptors[groupName] else {
                return nil
            }
            return (groupName, descriptors)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if pluginType == "crossfeed" {
                crossfeedModeButtonBar
            }

            if pluginType == "upmixer" {
                upmixerSpeakerConfigMenu
            }

            ForEach(descriptorGroups, id: \.0) { group in
                VStack(alignment: .leading, spacing: 6) {
                    Text(group.0.uppercased())
                        .font(.caption.weight(.semibold))
                        .foregroundColor(.secondary)

                    ForEach(group.1) { descriptor in
                        descriptorRow(descriptor)
                    }
                }
            }
        }
        .onAppear {
            draftParameters = normalizedInitialParameters(parameters)
        }
    }

    @ViewBuilder
    private var upmixerSpeakerConfigMenu: some View {
        if let descriptor = speakerConfigDescriptor {
            VStack(alignment: .leading, spacing: 6) {
                Text("OUTPUT LAYOUT")
                    .font(.caption.weight(.semibold))
                    .foregroundColor(.secondary)

                descriptorRow(descriptor)
            }
        }
    }

    private var crossfeedModeButtonBar: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Text("Mode")
                    .frame(width: 150, alignment: .trailing)
                    .font(.caption)

                Picker("", selection: crossfeedAlgorithmModeBinding) {
                    Text("Bauer").tag(1)
                    Text("Meier").tag(2)
                    Text("Multiband").tag(3)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 260)
            }

            Text("Crossfeed algorithm selection")
                .font(.caption2)
                .foregroundColor(.secondary)
                .padding(.leading, 158)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 3)
    }

    @ViewBuilder
    private func descriptorRow(_ descriptor: PluginParameterDescriptor) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 8) {
                Text(descriptor.name)
                    .frame(width: 150, alignment: .trailing)
                    .font(.caption)

                switch descriptor.type {
                case "bool":
                    Toggle("", isOn: boolBinding(for: descriptor))
                        .labelsHidden()

                case "choice":
                    Picker("", selection: choiceIndexBinding(for: descriptor)) {
                        ForEach(Array((descriptor.choices ?? []).enumerated()), id: \.offset) { index, label in
                            Text(label).tag(index)
                        }
                    }
                    .frame(width: descriptor.key == "speaker_config" ? 220 : 180)

                case "int":
                    Stepper(
                        value: intBinding(for: descriptor),
                        in: Int(descriptor.min ?? 0)...Int(descriptor.max ?? 100),
                        step: max(Int(descriptor.step ?? 1), 1)
                    ) {
                        Text("\(intValue(for: descriptor))\(descriptor.unit.isEmpty ? "" : " \(descriptor.unit)")")
                            .frame(width: 90, alignment: .leading)
                            .monospacedDigit()
                    }

                case "file_path":
                    TextField("", text: stringBinding(for: descriptor))
                        .textFieldStyle(.roundedBorder)
                        .frame(minWidth: 220)

                default:
                    Slider(
                        value: doubleBinding(for: descriptor),
                        in: (descriptor.min ?? -100.0)...(descriptor.max ?? 100.0),
                        step: max(descriptor.step ?? 0.1, 0.0001)
                    )
                    Text("\(doubleValue(for: descriptor), specifier: "%.2f")\(descriptor.unit.isEmpty ? "" : " \(descriptor.unit)")")
                        .frame(width: 100, alignment: .trailing)
                        .monospacedDigit()
                }
            }

            if !descriptor.doc.isEmpty {
                Text(descriptor.doc)
                    .font(.caption2)
                    .foregroundColor(.secondary)
                    .padding(.leading, 158)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .padding(.vertical, descriptor.doc.isEmpty ? 1 : 3)
    }

    private func updateValue(_ value: Any, for descriptor: PluginParameterDescriptor) {
        if pluginType == "crossfeed" {
            switch descriptor.key {
            case "mode", "crossfeed_mode":
                let index = crossfeedModeIndex(from: value) ?? 3
                draftParameters["mode"] = crossfeedModeValue(for: index)
                draftParameters.removeValue(forKey: "crossfeed_mode")
                onUpdate(draftParameters)
                return
            case "preset", "crossfeed_preset":
                let index = crossfeedPresetIndex(from: value) ?? 0
                draftParameters["preset"] = crossfeedPresetValue(for: index)
                draftParameters.removeValue(forKey: "crossfeed_preset")
                onUpdate(draftParameters)
                return
            default:
                break
            }
        }

        draftParameters[descriptor.key] = value
        onUpdate(draftParameters)
    }

    private func doubleBinding(for descriptor: PluginParameterDescriptor) -> Binding<Double> {
        Binding(
            get: { doubleValue(for: descriptor) },
            set: { updateValue($0, for: descriptor) }
        )
    }

    private func intBinding(for descriptor: PluginParameterDescriptor) -> Binding<Int> {
        Binding(
            get: { intValue(for: descriptor) },
            set: { updateValue($0, for: descriptor) }
        )
    }

    private func boolBinding(for descriptor: PluginParameterDescriptor) -> Binding<Bool> {
        Binding(
            get: { boolValue(for: descriptor) },
            set: { updateValue($0, for: descriptor) }
        )
    }

    private func stringBinding(for descriptor: PluginParameterDescriptor) -> Binding<String> {
        Binding(
            get: { stringValue(for: descriptor) },
            set: { updateValue($0, for: descriptor) }
        )
    }

    private func choiceIndexBinding(for descriptor: PluginParameterDescriptor) -> Binding<Int> {
        Binding(
            get: { choiceIndex(for: descriptor) },
            set: { index in
                updateValue(choiceValue(for: descriptor, index: index), for: descriptor)
            }
        )
    }

    private var crossfeedAlgorithmModeBinding: Binding<Int> {
        Binding(
            get: { crossfeedAlgorithmModeIndex() },
            set: { updateCrossfeedMode($0) }
        )
    }

    private func doubleValue(for descriptor: PluginParameterDescriptor) -> Double {
        numberValue(rawValue(for: descriptor)) ?? descriptor.defaultDouble ?? descriptor.min ?? 0.0
    }

    private func intValue(for descriptor: PluginParameterDescriptor) -> Int {
        Int((numberValue(rawValue(for: descriptor)) ?? descriptor.defaultDouble ?? descriptor.min ?? 0.0).rounded())
    }

    private func boolValue(for descriptor: PluginParameterDescriptor) -> Bool {
        let raw = rawValue(for: descriptor)
        if let bool = raw as? Bool {
            return bool
        }
        if let number = numberValue(raw) {
            return number > 0.5
        }
        if let string = raw as? String {
            let trueWords = ["true", "on", "yes", "1", descriptor.trueLabel?.lowercased() ?? ""]
            return trueWords.contains(string.lowercased())
        }
        return descriptor.defaultBool ?? false
    }

    private func stringValue(for descriptor: PluginParameterDescriptor) -> String {
        if let string = rawValue(for: descriptor) as? String {
            return string
        }
        return ""
    }

    private func choiceIndex(for descriptor: PluginParameterDescriptor) -> Int {
        if pluginType == "crossfeed" && (descriptor.key == "mode" || descriptor.key == "crossfeed_mode") {
            return crossfeedModeIndex()
        }
        if pluginType == "crossfeed" && (descriptor.key == "preset" || descriptor.key == "crossfeed_preset") {
            return crossfeedPresetIndex()
        }

        let choices = descriptor.choices ?? []
        let raw = rawValue(for: descriptor)
        if let string = raw as? String,
           let index = choices.firstIndex(of: string) {
            return index
        }
        // Oversampling travels as a factor (1/2/4); map it onto the
        // ["Off", "2x", "4x"] choice positions so the stored value displays
        // correctly instead of shifting by one.
        if descriptor.key == "oversampling", let factor = numberValue(raw) {
            switch Int(factor.rounded()) {
            case 2: return min(1, max(choices.count - 1, 0))
            case 4: return min(2, max(choices.count - 1, 0))
            default: return 0
            }
        }
        return Int((numberValue(raw) ?? descriptor.defaultDouble ?? 0.0).rounded())
            .clamped(to: 0...max(choices.count - 1, 0))
    }

    private func choiceValue(for descriptor: PluginParameterDescriptor, index: Int) -> Any {
        let choices = descriptor.choices ?? []
        if pluginType == "crossfeed" && (descriptor.key == "mode" || descriptor.key == "crossfeed_mode" || descriptor.key == "preset" || descriptor.key == "crossfeed_preset") {
            return index
        }
        // These controls address the plugin by label: the factory maps the
        // spec labels to factors/values, while integer factors stay valid
        // for hand-written configs.
        if descriptor.key == "speaker_config" || descriptor.key == "oversampling",
           let selected = choices[safe: index]
        {
            return selected
        }
        if let current = rawValue(for: descriptor) as? String,
           choices.contains(current),
           let selected = choices[safe: index] {
            return selected
        }
        return index
    }

    private func rawValue(for descriptor: PluginParameterDescriptor) -> Any? {
        if pluginType == "crossfeed" {
            switch descriptor.key {
            case "crossfeed_mode":
                return draftParameters["crossfeed_mode"] ?? draftParameters["mode"]
            case "crossfeed_preset":
                return draftParameters["crossfeed_preset"] ?? draftParameters["preset"]
            default:
                break
            }
        }
        return draftParameters[descriptor.key]
    }

    private func normalizedInitialParameters(_ parameters: [String: Any]) -> [String: Any] {
        guard pluginType == "crossfeed" else {
            return parameters
        }

        var normalized = parameters
        let parsedModeIndex = crossfeedModeIndex(from: normalized["crossfeed_mode"] ?? normalized["mode"]) ?? 3
        let modeIndex = parsedModeIndex == 0 ? 3 : parsedModeIndex
        if parsedModeIndex == 0 {
            normalized["enabled"] = false
        }
        let presetIndex = crossfeedPresetIndex(from: normalized["crossfeed_preset"] ?? normalized["preset"]) ?? 0
        normalized["mode"] = crossfeedModeValue(for: modeIndex)
        normalized.removeValue(forKey: "crossfeed_mode")
        normalized["preset"] = crossfeedPresetValue(for: presetIndex)
        normalized.removeValue(forKey: "crossfeed_preset")
        return normalized
    }

    private func updateCrossfeedMode(_ index: Int) {
        let modeIndex = index.clamped(to: 1...3)
        draftParameters["mode"] = crossfeedModeValue(for: modeIndex)
        draftParameters.removeValue(forKey: "crossfeed_mode")
        onUpdate(draftParameters)
    }

    private func multibandBandCount() -> Int {
        guard let descriptor = descriptors.first(where: { $0.key == "num_bands" }) else {
            return 3
        }

        let rawCount = numberValue(rawValue(for: descriptor)) ?? descriptor.defaultDouble ?? descriptor.min ?? 3.0
        let minBands = max(Int((descriptor.min ?? 2.0).rounded()), 1)
        let maxBands = max(Int((descriptor.max ?? Double(minBands)).rounded()), minBands)
        return Int(rawCount.rounded()).clamped(to: minBands...maxBands)
    }

    private func crossoverFrequencyIndex(for key: String) -> Int? {
        let prefix = "crossover_freq_"
        guard key.hasPrefix(prefix) else {
            return nil
        }
        return Int(String(key.dropFirst(prefix.count)))
    }

    private func crossfeedAlgorithmModeIndex() -> Int {
        let mode = crossfeedModeIndex()
        return mode == 0 ? 3 : mode.clamped(to: 1...3)
    }

    private func crossfeedModeIndex() -> Int {
        crossfeedModeIndex(from: draftParameters["crossfeed_mode"] ?? draftParameters["mode"]) ?? 3
    }

    private func crossfeedModeIndex(from raw: Any?) -> Int? {
        if let number = numberValue(raw) {
            return Int(number.rounded()).clamped(to: 0...3)
        }
        guard let string = raw as? String else {
            return nil
        }

        switch string.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "disable", "disabled", "off":
            return 0
        case "bauer":
            return 1
        case "meier":
            return 2
        case "multiband", "mb":
            return 3
        default:
            return nil
        }
    }

    private func crossfeedModeValue(for index: Int) -> String {
        switch index.clamped(to: 0...3) {
        case 0:
            return "Off"
        case 1:
            return "Bauer"
        case 2:
            return "Meier"
        default:
            return "Mb"
        }
    }

    private func crossfeedPresetIndex() -> Int {
        crossfeedPresetIndex(from: draftParameters["crossfeed_preset"] ?? draftParameters["preset"]) ?? 0
    }

    private func crossfeedPresetIndex(from raw: Any?) -> Int? {
        if let number = numberValue(raw) {
            return Int(number.rounded()).clamped(to: 0...4)
        }
        guard let string = raw as? String else {
            return nil
        }

        switch string.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "default":
            return 0
        case "cmoy":
            return 1
        case "meier":
            return 2
        case "mb", "multiband":
            return 3
        case "off", "disable", "disabled":
            return 4
        default:
            return nil
        }
    }

    private func crossfeedPresetValue(for index: Int) -> String {
        switch index.clamped(to: 0...4) {
        case 1:
            return "Cmoy"
        case 2:
            return "Meier"
        case 3:
            return "Mb"
        case 4:
            return "Off"
        default:
            return "Default"
        }
    }

    private func numberValue(_ raw: Any?) -> Double? {
        if let double = raw as? Double {
            return double
        }
        if let int = raw as? Int {
            return Double(int)
        }
        if let number = raw as? NSNumber {
            return number.doubleValue
        }
        if let string = raw as? String {
            return Double(string)
        }
        return nil
    }
}

private extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}

private extension Comparable {
    func clamped(to range: ClosedRange<Self>) -> Self {
        Swift.min(Swift.max(self, range.lowerBound), range.upperBound)
    }
}

// MARK: - Generic Plugin Editor (fallback)

struct GenericPluginEditor: View {
    let pluginType: String
    let parameters: [String: Any]
    let onUpdate: ([String: Any]) -> Void

    @State private var jsonText: String = ""
    @State private var parseError: String? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Parameters (JSON):")
                .font(.caption)
                .foregroundColor(.secondary)

            TextEditor(text: $jsonText)
                .font(.system(.body, design: .monospaced))
                .frame(minHeight: 80, maxHeight: 200)
                .border(Color.gray.opacity(0.3))

            HStack {
                if let error = parseError {
                    Text(error)
                        .font(.caption)
                        .foregroundColor(.red)
                }
                Spacer()
                Button("Update Draft") {
                    applyJson()
                }
            }
        }
        .onAppear {
            if let data = try? JSONSerialization.data(withJSONObject: parameters, options: .prettyPrinted),
               let str = String(data: data, encoding: .utf8) {
                jsonText = str
            }
        }
    }

    private func applyJson() {
        guard let data = jsonText.data(using: .utf8),
              let parsed = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            parseError = "Invalid JSON"
            return
        }
        parseError = nil
        onUpdate(parsed)
    }
}
