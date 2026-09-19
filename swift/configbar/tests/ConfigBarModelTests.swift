import XCTest
@testable import ConfigBarModels

final class ConfigBarModelTests: XCTestCase {
    func testMenuBarPillConventionSeparatesPlayingRecordingAndIdle() {
        XCTAssertEqual(
            ConfigBarMenuBarPillAppearance.resolve(
                isPlaying: true,
                isRecording: false,
                hasIssue: false
            ),
            .playing
        )
        XCTAssertEqual(
            ConfigBarMenuBarPillAppearance.resolve(
                isPlaying: false,
                isRecording: true,
                hasIssue: false
            ),
            .recording
        )
        XCTAssertEqual(
            ConfigBarMenuBarPillAppearance.resolve(
                isPlaying: false,
                isRecording: false,
                hasIssue: false
            ),
            .transparent
        )
        XCTAssertEqual(
            ConfigBarMenuBarPillAppearance.resolve(
                isPlaying: true,
                isRecording: false,
                hasIssue: true
            ),
            .transparent
        )
    }


    // MARK: - Helpers

    private func availablePlugin(
        type: String,
        name: String = "",
        description: String = "",
        category: String,
        maturity: String = ""
    ) -> AvailablePlugin {
        AvailablePlugin(
            type_: type,
            name: name,
            description: description,
            category: category,
            maturity: maturity,
            defaultParameters: [:],
            parameters: []
        )
    }

    // MARK: - Category grouping

    func testGroupPluginsByCategoryPreservesOrderAndSortsUnknownAlphabetically() {
        let plugins = [
            availablePlugin(type: "delay", category: "Effects"),
            availablePlugin(type: "eq", category: "EQ & Tone"),
            availablePlugin(type: "upmixer", category: "Spatial & Routing"),
            availablePlugin(type: "zebra", category: "Zebra"),
            availablePlugin(type: "compressor", category: "Dynamics"),
            availablePlugin(type: "alpha", category: "Alpha"),
            availablePlugin(type: "denoiser", category: "Restoration"),
        ]

        let grouped = groupPluginsByCategory(plugins)
        let names = grouped.map { $0.name }

        XCTAssertEqual(names, [
            "EQ & Tone",
            "Dynamics",
            "Spatial & Routing",
            "Effects",
            "Restoration",
            "Alpha",
            "Zebra",
        ])

        XCTAssertEqual(grouped.first { $0.name == "EQ & Tone" }?.plugins.map(\.type_), ["eq"])
        XCTAssertEqual(grouped.first { $0.name == "Effects" }?.plugins.map(\.type_), ["delay"])
        XCTAssertEqual(grouped.first { $0.name == "Alpha" }?.plugins.map(\.type_), ["alpha"])
        XCTAssertEqual(grouped.first { $0.name == "Zebra" }?.plugins.map(\.type_), ["zebra"])
    }

    func testGroupPluginsByCategoryReturnsEmptyForEmptyInput() {
        XCTAssertTrue(groupPluginsByCategory([]).isEmpty)
    }

    // MARK: - Display names

    func testPluginDisplayNameKnownTypes() {
        XCTAssertEqual(pluginDisplayName("eq"), "EQ")
        XCTAssertEqual(pluginDisplayName("compressor"), "Compressor")
        XCTAssertEqual(pluginDisplayName("upmixer"), "Upmixer")
        XCTAssertEqual(pluginDisplayName("multiband_compressor"), "Multiband Compressor")
        XCTAssertEqual(pluginDisplayName("loudness_compensation"), "Loudness Compensation")
        XCTAssertEqual(pluginDisplayName("ab_compare"), "A/B Compare")
    }

    func testPluginDisplayNameUnknownTypeReturnsInput() {
        let unknown = "unknown_fancy_plugin"
        XCTAssertEqual(pluginDisplayName(unknown), unknown)
    }

    // MARK: - PluginParameterDescriptor defaults

    func testParameterDescriptorDefaults() {
        let descriptor = PluginParameterDescriptor(
            key: "gain",
            name: "Gain",
            type: "double"
        )

        XCTAssertEqual(descriptor.unit, "")
        XCTAssertEqual(descriptor.group, "General")
        XCTAssertEqual(descriptor.updateMode, "realtime")
        XCTAssertNil(descriptor.min)
        XCTAssertNil(descriptor.max)
        XCTAssertNil(descriptor.step)
        XCTAssertNil(descriptor.defaultDouble)
        XCTAssertNil(descriptor.defaultBool)
        XCTAssertNil(descriptor.choices)
        XCTAssertNil(descriptor.trueLabel)
        XCTAssertNil(descriptor.falseLabel)
    }

    // MARK: - PluginParameterDescriptor round-trip

    func testParameterDescriptorCustomValuesRoundTrip() {
        let descriptor = PluginParameterDescriptor(
            key: "freq",
            name: "Frequency",
            type: "double",
            unit: "Hz",
            group: "Filter",
            doc: "Cutoff frequency",
            updateMode: "defer",
            min: 20.0,
            max: 20000.0,
            step: 1.0,
            defaultDouble: 1000.0,
            defaultBool: nil,
            choices: ["A", "B"],
            trueLabel: "On",
            falseLabel: "Off"
        )

        XCTAssertEqual(descriptor.key, "freq")
        XCTAssertEqual(descriptor.name, "Frequency")
        XCTAssertEqual(descriptor.type, "double")
        XCTAssertEqual(descriptor.unit, "Hz")
        XCTAssertEqual(descriptor.group, "Filter")
        XCTAssertEqual(descriptor.doc, "Cutoff frequency")
        XCTAssertEqual(descriptor.updateMode, "defer")
        XCTAssertEqual(descriptor.min, 20.0)
        XCTAssertEqual(descriptor.max, 20000.0)
        XCTAssertEqual(descriptor.step, 1.0)
        XCTAssertEqual(descriptor.defaultDouble, 1000.0)
        XCTAssertNil(descriptor.defaultBool)
        XCTAssertEqual(descriptor.choices, ["A", "B"])
        XCTAssertEqual(descriptor.trueLabel, "On")
        XCTAssertEqual(descriptor.falseLabel, "Off")
    }

    // MARK: - Identifiable conformance

    func testAvailablePluginIdEqualsType() {
        let plugin = availablePlugin(type: "eq", category: "EQ & Tone")
        XCTAssertEqual(plugin.id, plugin.type_)
        XCTAssertEqual(plugin.id, "eq")
    }

    func testPluginCategoryIdEqualsName() {
        let category = PluginCategory(
            name: "Dynamics",
            plugins: [availablePlugin(type: "compressor", category: "Dynamics")]
        )
        XCTAssertEqual(category.id, category.name)
        XCTAssertEqual(category.id, "Dynamics")
    }

    // MARK: - Graph topology

    private func graphNode(_ id: Int) -> PluginGraphNodeModel {
        PluginGraphNodeModel(
            id: id,
            pluginType: "gain",
            parameters: [:],
            inputChannels: 2,
            bypassed: false
        )
    }

    private func graph(_ nodeIDs: [Int], edges: [(Int, Int)]) -> PluginGraphModel {
        PluginGraphModel(
            nodes: nodeIDs.map(graphNode),
            edges: edges.map { PluginGraphEdgeModel(fromNode: $0.0, toNode: $0.1) }
        )
    }

    func testGraphTopologyAcceptsLinearChainAndReturnsOrder() {
        let model = graph([10, 20, 30], edges: [(10, 20), (20, 30)])

        XCTAssertEqual(model.linearNodeIDs, [10, 20, 30])
        XCTAssertTrue(model.isLinear)
    }

    func testGraphTopologyRejectsCycle() {
        let model = graph([1, 2, 3], edges: [(1, 2), (2, 3), (3, 1)])

        XCTAssertNil(model.linearNodeIDs)
        XCTAssertFalse(model.isLinear)
    }

    func testGraphTopologyRejectsMultipleRoots() {
        let model = graph([1, 2, 3], edges: [(1, 3)])

        XCTAssertNil(model.linearNodeIDs)
        XCTAssertFalse(model.isLinear)
    }

    func testGraphTopologyRejectsMalformedEdges() {
        let unknownNode = graph([1, 2], edges: [(1, 99)])
        let duplicateNodeIDs = PluginGraphModel(
            nodes: [graphNode(1), graphNode(1)],
            edges: [PluginGraphEdgeModel(fromNode: 1, toNode: 1)]
        )

        XCTAssertNil(unknownNode.linearNodeIDs)
        XCTAssertNil(duplicateNodeIDs.linearNodeIDs)
    }

    // MARK: - Configbar pure behavior

    func testVirtualDeviceDetectionRejectsKnownLoopbackDevicesOnly() {
        XCTAssertTrue(isConfigBarVirtualDevice("SotF Virtual Audio"))
        XCTAssertTrue(isConfigBarVirtualDevice("BLACKHOLE 2ch"))
        XCTAssertFalse(isConfigBarVirtualDevice("Built-in Output"))
    }

    func testMeterPeakSanitizationClampsInvalidAndOversizedValues() {
        let values = sanitizeConfigBarPeaks([.nan, -1.0, 0.0, 0.75, 4.0])

        XCTAssertEqual(values, [0.0, 0.0, 0.0, 0.75, 2.0])
    }

    func testMeterPeakHoldsDecayWithoutDroppingBelowCurrentPeak() {
        let values = updateConfigBarPeakHolds(
            previous: [1.0, 0.25],
            current: [0.5, 0.5]
        )

        XCTAssertEqual(values[0], 0.96, accuracy: 0.0001)
        XCTAssertEqual(values[1], 0.5, accuracy: 0.0001)
        XCTAssertEqual(decayConfigBarPeaks([1.0]), [0.85])
    }

    func testEncryptionToggleGuardConsumesOnlyTheProgrammaticRollback() {
        var guardState = EncryptionToggleGuard()
        var daemonRequests = 0

        XCTAssertFalse(guardState.consumeProgrammaticChange())
        daemonRequests += 1 // the user's original failed request

        guardState.markProgrammaticChange()
        XCTAssertTrue(guardState.consumeProgrammaticChange())
        XCTAssertFalse(guardState.consumeProgrammaticChange())
        XCTAssertEqual(daemonRequests, 1)
    }

    func testRejectedOptimisticMutationsRollbackWithoutRetrying() {
        var device = ConfigBarMutationState(confirmed: "Built-in Output")
        var volume = ConfigBarMutationState(confirmed: Float(0.75))
        var channels = ConfigBarMutationState(confirmed: 2)
        var daemonRequests = 0

        let deviceGeneration = device.begin("USB DAC")
        daemonRequests += 1
        XCTAssertEqual(
            device.resolve(
                generation: deviceGeneration,
                requested: "USB DAC",
                succeeded: false
            ),
            .rolledBack("Built-in Output")
        )

        let volumeGeneration = volume.begin(0.25)
        daemonRequests += 1
        XCTAssertEqual(
            volume.resolve(
                generation: volumeGeneration,
                requested: 0.25,
                succeeded: false
            ),
            .rolledBack(0.75)
        )

        let channelGeneration = channels.begin(8)
        daemonRequests += 1
        XCTAssertEqual(
            channels.resolve(
                generation: channelGeneration,
                requested: 8,
                succeeded: false
            ),
            .rolledBack(2)
        )

        // Rollback is a state assignment only; it must not become a second
        // daemon request through a Picker or Slider callback.
        XCTAssertEqual(daemonRequests, 3)
        XCTAssertEqual(device.confirmed, "Built-in Output")
        XCTAssertEqual(volume.confirmed, 0.75, accuracy: 0.0001)
        XCTAssertEqual(channels.confirmed, 2)
    }

    func testStaleMutationCompletionCannotOverwriteNewerConfirmedValue() {
        var state = ConfigBarMutationState(confirmed: 2)
        let firstGeneration = state.begin(4)
        let secondGeneration = state.begin(8)

        XCTAssertNil(
            state.resolve(
                generation: firstGeneration,
                requested: 4,
                succeeded: false
            )
        )
        XCTAssertEqual(
            state.resolve(
                generation: secondGeneration,
                requested: 8,
                succeeded: true
            ),
            .confirmed(8)
        )
        XCTAssertEqual(state.confirmed, 8)
    }

    func testStatusWatermarkRejectsSnapshotStartedBeforeMutation() {
        var watermark = ConfigBarStatusWatermark()
        let firstSnapshot = watermark.generation

        XCTAssertTrue(watermark.accepts(snapshotGeneration: firstSnapshot))
        XCTAssertEqual(watermark.beginMutation(), 1)
        XCTAssertFalse(watermark.accepts(snapshotGeneration: firstSnapshot))

        let secondSnapshot = watermark.generation
        XCTAssertTrue(watermark.accepts(snapshotGeneration: secondSnapshot))
    }

    func testRejectedEncryptionMutationRollsBackOnceAndConsumesGuard() {
        var state = ConfigBarMutationState(confirmed: true)
        var toggleGuard = EncryptionToggleGuard()
        var daemonRequests = 0

        let generation = state.begin(false)
        daemonRequests += 1
        XCTAssertEqual(
            state.resolve(generation: generation, requested: false, succeeded: false),
            .rolledBack(true)
        )

        toggleGuard.markProgrammaticChange()
        XCTAssertTrue(toggleGuard.consumeProgrammaticChange())
        XCTAssertFalse(toggleGuard.consumeProgrammaticChange())
        XCTAssertEqual(daemonRequests, 1)
        XCTAssertTrue(state.confirmed)
    }

    func testRackRefreshGateCoalescesRequestsReceivedWhileBusy() {
        var gate = ConfigBarRefreshGate()

        XCTAssertTrue(gate.request())
        XCTAssertTrue(gate.isRefreshing)
        XCTAssertFalse(gate.request())
        XCTAssertFalse(gate.request())
        XCTAssertTrue(gate.hasPendingRefresh)

        XCTAssertTrue(gate.complete(), "one queued refresh must run next")
        XCTAssertTrue(gate.isRefreshing)
        XCTAssertFalse(gate.hasPendingRefresh)
        XCTAssertFalse(gate.complete(), "the follow-up completes the refresh cycle")
        XCTAssertFalse(gate.isRefreshing)
    }

    func testGenerationConflictUsesActionableRackMessage() {
        XCTAssertTrue(isConfigBarGenerationConflict(
            "Pipeline generation conflict: intent was based on generation 1, current generation is 7."
        ))
        XCTAssertFalse(isConfigBarGenerationConflict("Failed to build plugin graph"))
        let message = configBarMutationErrorMessage(
            daemonError: "Pipeline generation conflict: intent was based on generation 1, current generation is 7.",
            fallback: "Failed to update plugin"
        )

        XCTAssertEqual(
            message,
            "The pipeline changed while this view was open. Refreshed to the current version; please retry."
        )
    }

    func testApplyConfigurationRetriesOnceOnGenerationConflict() {
        let conflict = "Pipeline generation conflict: intent was based on generation 0, current generation is 1."
        XCTAssertTrue(configBarShouldRetryApplyConfiguration(
            success: false, daemonError: conflict, mayRetry: true
        ))
        XCTAssertFalse(
            configBarShouldRetryApplyConfiguration(success: false, daemonError: conflict, mayRetry: false),
            "the retry must not loop"
        )
        XCTAssertFalse(configBarShouldRetryApplyConfiguration(
            success: true, daemonError: nil, mayRetry: true
        ))
        XCTAssertFalse(configBarShouldRetryApplyConfiguration(
            success: false, daemonError: "Output device 'X' not found.", mayRetry: true
        ), "validation failures must surface for rollback, not resend")
        XCTAssertFalse(configBarShouldRetryApplyConfiguration(
            success: false, daemonError: nil, mayRetry: true
        ), "unreachable daemon must surface, not resend")
    }

    // MARK: - Delay editor parameter policy

    func testDelayUniformModeRemovesPerChannelArray() {
        let base: [String: Any] = [
            "delay_ms": 10.0,
            "channel_delays_ms": [1.0, 2.0],
            "feedback": 0.0,
            "mix": 1.0,
        ]

        let next = ConfigBarDelayParameters.applyingUniformDelay(base, delayMs: 25.0)

        XCTAssertEqual(next["delay_ms"] as? Double, 25.0)
        XCTAssertNil(next["channel_delays_ms"] as? [Any])
        XCTAssertFalse(ConfigBarDelayParameters.isPerChannel(next))
    }

    func testDelayPerChannelModeForcesPureRoutingConstraints() {
        let base: [String: Any] = [
            "delay_ms": 100.0,
            "feedback": 0.3,
            "mix": 0.5,
            "lfo_rate_hz": 2.0,
            "lfo_depth_ms": 1.0,
            "allpass_feedback": true,
            "pitch_preserving": true,
            "allpass_coeff": 0.7,
        ]

        let next = ConfigBarDelayParameters.applyingPerChannelDelays(
            base,
            delaysMs: [1.0, 2.5]
        )

        XCTAssertTrue(ConfigBarDelayParameters.isPerChannel(next))
        XCTAssertEqual(next["channel_delays_ms"] as? [Double], [1.0, 2.5])
        XCTAssertEqual(next["delay_ms"] as? Double, 1.0)
        XCTAssertEqual(next["feedback"] as? Double, 0.0)
        XCTAssertEqual(next["mix"] as? Double, 1.0)
        XCTAssertEqual(next["lfo_rate_hz"] as? Double, 0.0)
        XCTAssertEqual(next["lfo_depth_ms"] as? Double, 0.0)
        XCTAssertEqual(next["allpass_feedback"] as? Bool, false)
        XCTAssertEqual(next["pitch_preserving"] as? Bool, false)
    }

    func testDelayPerChannelModeWithEmptyArrayIsANoOp() {
        let base: [String: Any] = ["delay_ms": 10.0]

        let next = ConfigBarDelayParameters.applyingPerChannelDelays(base, delaysMs: [])

        XCTAssertFalse(ConfigBarDelayParameters.isPerChannel(next))
        XCTAssertEqual(next["delay_ms"] as? Double, 10.0)
    }

    func testDelayChannelDelaysResizeToChannelCount() {
        let params: [String: Any] = [
            "delay_ms": 5.0,
            "channel_delays_ms": [1.0, 2.0],
        ]

        XCTAssertEqual(
            ConfigBarDelayParameters.channelDelays(params, channelCount: 4),
            [1.0, 2.0, 5.0, 5.0]
        )
        XCTAssertEqual(
            ConfigBarDelayParameters.channelDelays(params, channelCount: 1),
            [1.0]
        )
        XCTAssertEqual(
            ConfigBarDelayParameters.channelDelays([:], channelCount: 2),
            [0.0, 0.0]
        )
    }

    func testDelayValuesAreClampedToEngineRange() {
        let params: [String: Any] = [
            "delay_ms": -4.0,
            "channel_delays_ms": [-1.0, Double.nan, 99_999.0],
        ]

        XCTAssertEqual(ConfigBarDelayParameters.scalarDelayMs(params), 0.0)
        XCTAssertEqual(
            ConfigBarDelayParameters.channelDelays(params, channelCount: 3),
            [0.0, 0.0, ConfigBarDelayParameters.maxDelayMs]
        )
    }

    // MARK: - EQ editor parameter policy

    func testEQUniformModeRemovesPerChannelArray() {
        let base: [String: Any] = [
            "filters": [["filter_type": "peak"]],
            "channel_filters": [[["filter_type": "peak"]], [["filter_type": "notch"]]],
        ]

        let next = ConfigBarEQParameters.applyingUniformFilters(
            base,
            filters: [["filter_type": "peak"]]
        )

        XCTAssertEqual((next["filters"] as? [[String: Any]])?.count, 1)
        XCTAssertNil(next["channel_filters"] as? [Any])
        XCTAssertFalse(ConfigBarEQParameters.isPerChannel(next))
    }

    func testEQPerChannelModeKeepsChannelZeroMirror() {
        let base: [String: Any] = [
            "filters": [["filter_type": "peak"]],
        ]
        let perChannel: [[[String: Any]]] = [
            [["filter_type": "peak"]],
            [["filter_type": "notch"]],
        ]

        let next = ConfigBarEQParameters.applyingPerChannelFilters(
            base,
            channelFilters: perChannel
        )

        XCTAssertTrue(ConfigBarEQParameters.isPerChannel(next))
        XCTAssertEqual((next["channel_filters"] as? [[[String: Any]]])?.count, 2)
        XCTAssertEqual((next["filters"] as? [[String: Any]])?.count, 1)
    }

    func testEQPerChannelModeWithEmptyArrayIsANoOp() {
        let base: [String: Any] = ["filters": [["filter_type": "peak"]]]

        let next = ConfigBarEQParameters.applyingPerChannelFilters(base, channelFilters: [])

        XCTAssertFalse(ConfigBarEQParameters.isPerChannel(next))
    }

    func testEQChannelFiltersResizeToChannelCount() {
        let peak: [String: Any] = ["filter_type": "peak"]
        let notch: [String: Any] = ["filter_type": "notch"]
        let params: [String: Any] = [
            "filters": [peak],
            "channel_filters": [[peak], [notch]],
        ]

        let grown = ConfigBarEQParameters.channelFilters(params, channelCount: 4)
        XCTAssertEqual(grown.count, 4)
        XCTAssertEqual(grown[0].count, 1)
        XCTAssertEqual(grown[2].count, 1)

        let shrunk = ConfigBarEQParameters.channelFilters(params, channelCount: 1)
        XCTAssertEqual(shrunk.count, 1)

        let fromUniform = ConfigBarEQParameters.channelFilters(
            ["filters": [peak]],
            channelCount: 2
        )
        XCTAssertEqual(fromUniform.count, 2)
        XCTAssertEqual(fromUniform[0].count, 1)
        XCTAssertEqual(fromUniform[1].count, 1)

        XCTAssertEqual(
            ConfigBarEQParameters.channelFilters([:], channelCount: 2).count,
            2
        )
    }

    // MARK: - Output profile key policy

    func testOutputProfileDeviceKeyPrefersUID() {
        XCTAssertEqual(
            ConfigBarOutputProfiles.deviceKey(uid: "  abc ", name: "Speakers"),
            "uid:abc"
        )
        XCTAssertEqual(
            ConfigBarOutputProfiles.deviceKey(uid: nil, name: "Speakers"),
            "name:Speakers"
        )
        XCTAssertEqual(
            ConfigBarOutputProfiles.deviceKey(uid: "   ", name: "Speakers"),
            "name:Speakers"
        )
    }

    func testOutputProfileResolutionPrefersUIDThenNameThenDefault() {
        let assignments = [
            "uid:AAA": "p-head",
            "name:Speakers": "p-name",
            "uid:STALE": "p-deleted",
        ]
        let known: Set<String> = ["default", "p-head", "p-name"]

        XCTAssertEqual(
            ConfigBarOutputProfiles.resolveProfileID(
                assignments: assignments, uid: "AAA", name: "Whatever", knownProfileIDs: known
            ),
            "p-head"
        )
        XCTAssertEqual(
            ConfigBarOutputProfiles.resolveProfileID(
                assignments: assignments, uid: nil, name: "Speakers", knownProfileIDs: known
            ),
            "p-name"
        )
        XCTAssertEqual(
            ConfigBarOutputProfiles.resolveProfileID(
                assignments: assignments, uid: "STALE", name: "Unknown", knownProfileIDs: known
            ),
            "default"
        )
        XCTAssertNil(
            ConfigBarOutputProfiles.resolveProfileID(
                assignments: [:], uid: nil, name: "Unknown", knownProfileIDs: ["p-head"]
            )
        )
    }

    func testOutputProfileModelParsesRackAndRoundTripsWire() {
        let dict: [String: Any] = [
            "id": "p1",
            "name": "Headphones",
            "topology": "rack",
            "plugins": [
                ["plugin_type": "eq", "parameters": ["gain_db": 3.0], "input_channels": 2],
            ],
            "input_channels": 2,
            "output_channels": 2,
            "updated_at_unix_ms": 42,
        ]
        guard let profile = OutputProfileModel.parse(dict) else {
            XCTFail("rack profile should parse")
            return
        }
        XCTAssertEqual(profile.id, "p1")
        XCTAssertFalse(profile.isGraph)
        XCTAssertEqual(profile.rackInstances().count, 1)
        XCTAssertEqual(profile.rackInstances()[0].pluginType, "eq")
        let wire = profile.wire()
        XCTAssertEqual(wire["id"] as? String, "p1")
        XCTAssertEqual(
            (wire["plugins"] as? [[String: Any]])?.first?["plugin_type"] as? String,
            "eq"
        )

        XCTAssertNil(OutputProfileModel.parse(["id": "x"]))
        XCTAssertNil(OutputProfileModel.parse([
            "id": "x", "name": "y", "topology": "graph",
            "input_channels": 2, "output_channels": 2,
        ]))
    }

    func testOutputProfileModelParsesGraph() {
        let dict: [String: Any] = [
            "id": "g1",
            "name": "Speakers",
            "topology": "graph",
            "plugins": [],
            "graph": [
                "nodes": [
                    ["id": 0, "plugin_type": "eq", "parameters": [:], "input_channels": 2],
                ],
                "edges": [],
            ],
            "input_channels": 2,
            "output_channels": 2,
            "updated_at_unix_ms": 0,
        ]
        guard let profile = OutputProfileModel.parse(dict) else {
            XCTFail("graph profile should parse")
            return
        }
        XCTAssertTrue(profile.isGraph)
        XCTAssertEqual(profile.graph?.nodes.count, 1)
        XCTAssertEqual(profile.graph?.nodes.first?.pluginType, "eq")
    }

    // MARK: - Daemon watchdog policy

    func testWatchdogToleratesTransientProbeFailureButRestartsOnRepeatedOnes() {
        XCTAssertFalse(ConfigBarDaemonWatchdog.shouldRestart(consecutiveFailures: 0))
        XCTAssertFalse(ConfigBarDaemonWatchdog.shouldRestart(consecutiveFailures: 1))
        XCTAssertTrue(ConfigBarDaemonWatchdog.shouldRestart(
            consecutiveFailures: ConfigBarDaemonWatchdog.restartThreshold
        ))
        XCTAssertTrue(ConfigBarDaemonWatchdog.shouldRestart(consecutiveFailures: 5))
    }
}
