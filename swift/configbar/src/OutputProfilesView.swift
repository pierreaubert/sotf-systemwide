import SwiftUI
import AppKit
import ConfigBarModels

// MARK: - Outputs & DSP Profiles (Option B)
//
// Master/detail split: the left column lists audio interfaces (present ones
// plus remembered-but-unplugged ones whose profile assignment survives), the
// right column edits the selected output's DSP chain. The live route embeds
// the full live chain view; every other output edits its stored profile,
// which the daemon applies atomically the next time that route is selected.

struct OutputProfilesView: View {
    let client: AudioEngineClient
    let entries: [OutputRouteEntry]
    let profiles: [OutputProfileModel]
    let outputChannels: Int
    let availableOutputChannels: Int?
    let refreshTrigger: Int
    let onSwitchRoute: (OutputRouteEntry, String?) -> Void
    let onAssignProfile: (OutputRouteEntry, String) -> Void
    let onSaveProfile: (OutputProfileModel, @escaping (Bool) -> Void) -> Void
    let onCreateProfile: (String) -> Void
    let onRenameProfile: (String, String) -> Void
    let onDeleteProfile: (String) -> Void

    @State private var selectedID: String? = nil
    @State private var showingCreateProfile = false
    @State private var newProfileName = ""

    private var selection: OutputRouteEntry? {
        if let selectedID,
           let match = entries.first(where: { $0.id == selectedID }) {
            return match
        }
        return entries.first(where: { $0.isLiveRoute }) ?? entries.first
    }

    private func profileName(for id: String?) -> String {
        profiles.first { $0.id == id }?.name ?? id ?? "Default"
    }

    var body: some View {
        HStack(alignment: .top, spacing: 0) {
            // MARK: Master — outputs
            VStack(alignment: .leading, spacing: 0) {
                HStack {
                    Text("OUTPUTS")
                        .font(.caption.weight(.semibold))
                        .foregroundColor(.secondary)
                    Spacer()
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 6)

                ScrollView(.vertical, showsIndicators: true) {
                    LazyVStack(alignment: .leading, spacing: 2) {
                        ForEach(entries) { entry in
                            OutputMasterRow(
                                entry: entry,
                                profileName: profileName(for: entry.profileID),
                                isSelected: entry.id == selection?.id,
                                onSelect: { selectedID = entry.id },
                                onSwitch: { onSwitchRoute(entry, entry.profileID) }
                            )
                        }
                    }
                    .padding(.horizontal, 4)
                }
                .frame(minHeight: 220)
            }
            .frame(width: 270)

            Divider()
                .padding(.horizontal, 8)

            // MARK: Detail — selected output's chain
            VStack(alignment: .leading, spacing: 8) {
                if let entry = selection {
                    ProfileDetailHeader(
                        entry: entry,
                        profiles: profiles,
                        onAssignProfile: { onAssignProfile(entry, $0) },
                        onSwitch: { onSwitchRoute(entry, entry.profileID) },
                        onCreateProfile: {
                            newProfileName = "\(entry.name) DSP"
                            showingCreateProfile = true
                        },
                        onRenameProfile: { onRenameProfile($0, $1) },
                        onDeleteProfile: onDeleteProfile
                    )

                    Divider()

                    if entry.isLiveRoute {
                        PluginRackView(
                            client: client,
                            outputChannels: outputChannels,
                            availableOutputChannels: availableOutputChannels,
                            refreshTrigger: refreshTrigger
                        )
                    } else if let profile = profiles.first(where: { $0.id == entry.profileID }) {
                        StoredProfileDetailView(
                            client: client,
                            profile: profile,
                            channelCount: entry.channels ?? outputChannels,
                            onSaveProfile: onSaveProfile
                        )
                        .id("stored-\(profile.id)")
                    } else {
                        Text("No stored chain for this output yet — switch to it or assign a profile.")
                            .foregroundColor(.secondary)
                            .font(.callout)
                    }
                } else {
                    Text("No audio outputs known.")
                        .foregroundColor(.secondary)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .sheet(isPresented: $showingCreateProfile) {
            VStack(alignment: .leading, spacing: 12) {
                Text("New DSP Profile")
                    .font(.headline)
                Text("Starts as an empty chain. Assign plugins in the detail view, then switch outputs to hear it.")
                    .font(.callout)
                    .foregroundColor(.secondary)
                TextField("Profile name", text: $newProfileName)
                    .textFieldStyle(.roundedBorder)
                HStack {
                    Spacer()
                    Button("Cancel") { showingCreateProfile = false }
                        .keyboardShortcut(.cancelAction)
                    Button("Create") {
                        let name = newProfileName.trimmingCharacters(in: .whitespacesAndNewlines)
                        if !name.isEmpty {
                            onCreateProfile(name)
                        }
                        showingCreateProfile = false
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(newProfileName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            .padding()
            .frame(width: 380)
        }
    }
}

// MARK: - Master Row

struct OutputMasterRow: View {
    let entry: OutputRouteEntry
    let profileName: String
    let isSelected: Bool
    let onSelect: () -> Void
    let onSwitch: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(entry.isLiveRoute ? Color.green : (entry.isPresent ? Color.blue : Color.gray))
                .frame(width: 8, height: 8)
                .help(entry.isLiveRoute ? "Live route" : (entry.isPresent ? "Connected" : "Unplugged — profile kept"))

            Button(action: onSelect) {
                VStack(alignment: .leading, spacing: 1) {
                    HStack(spacing: 4) {
                        Text(entry.name)
                            .font(.callout.weight(entry.isLiveRoute ? .semibold : .regular))
                            .lineLimit(1)
                            .truncationMode(.tail)
                        if entry.isDefault {
                            Text("default")
                                .font(.caption2)
                                .foregroundColor(.secondary)
                        }
                        if entry.isLiveRoute {
                            Text("LIVE")
                                .font(.caption2.weight(.bold))
                                .foregroundColor(.green)
                        }
                    }
                    Text("\(profileName)\(entry.channels.map { " • \($0)ch" } ?? "")\(entry.isPresent ? "" : " • unplugged")")
                        .font(.caption)
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if entry.isPresent && !entry.isLiveRoute {
                Button("Switch", action: onSwitch)
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                    .help("Route audio to \(entry.name) and load its DSP profile")
            }
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 5)
        .background(
            RoundedRectangle(cornerRadius: 6)
                .fill(isSelected ? Color.accentColor.opacity(0.15) : Color.clear)
        )
    }
}

// MARK: - Detail Header

struct ProfileDetailHeader: View {
    let entry: OutputRouteEntry
    let profiles: [OutputProfileModel]
    let onAssignProfile: (String) -> Void
    let onSwitch: () -> Void
    let onCreateProfile: () -> Void
    let onRenameProfile: (String, String) -> Void
    let onDeleteProfile: (String) -> Void

    @State private var showingRename = false
    @State private var renameText = ""

    private var currentProfile: OutputProfileModel? {
        profiles.first { $0.id == entry.profileID }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text(entry.name)
                    .font(.headline)
                    .lineLimit(1)
                    .truncationMode(.tail)
                if entry.isLiveRoute {
                    Text("LIVE")
                        .font(.caption.weight(.bold))
                        .foregroundColor(.white)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.green)
                        .cornerRadius(4)
                } else if !entry.isPresent {
                    Text("UNPLUGGED")
                        .font(.caption.weight(.bold))
                        .foregroundColor(.white)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.gray)
                        .cornerRadius(4)
                }
                Spacer()
                if entry.isPresent && !entry.isLiveRoute {
                    Button("Switch to this output", action: onSwitch)
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                }
            }

            HStack(spacing: 8) {
                Text("DSP Profile")
                    .font(.caption)
                    .foregroundColor(.secondary)
                Menu {
                    ForEach(profiles) { profile in
                        Button {
                            onAssignProfile(profile.id)
                        } label: {
                            HStack {
                                Text(profile.name)
                                if profile.id == entry.profileID {
                                    Image(systemName: "checkmark")
                                }
                            }
                        }
                    }
                    Divider()
                    Button("New profile for this output…", action: onCreateProfile)
                } label: {
                    HStack(spacing: 4) {
                        Text(currentProfile?.name ?? entry.profileID ?? "Default")
                        Image(systemName: "chevron.down")
                            .font(.caption)
                    }
                }
                .menuStyle(.borderlessButton)
                .fixedSize()

                if let current = currentProfile {
                    Button("Rename…") {
                        renameText = current.name
                        showingRename = true
                    }
                    .buttonStyle(.link)
                    .font(.caption)
                    if current.id != ConfigBarOutputProfiles.defaultProfileID {
                        Button("Delete") {
                            onDeleteProfile(current.id)
                        }
                        .buttonStyle(.link)
                        .font(.caption)
                        .foregroundColor(.red)
                    }
                }
                Spacer()
            }
            .popover(isPresented: $showingRename, arrowEdge: .bottom) {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Rename profile")
                        .font(.headline)
                    TextField("Profile name", text: $renameText)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 220)
                    HStack {
                        Spacer()
                        Button("Cancel") { showingRename = false }
                        Button("Save") {
                            if let current = currentProfile,
                               !renameText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                                onRenameProfile(current.id, renameText)
                            }
                            showingRename = false
                        }
                        .keyboardShortcut(.defaultAction)
                    }
                }
                .padding()
            }
        }
    }
}

// MARK: - Stored Profile Detail

/// Edits a non-live output's stored chain. Rack profiles use a draft list
/// bound to the shared row/sheet components; graph profiles reuse the graph
/// editors with apply wired to the profile store instead of the live chain.
struct StoredProfileDetailView: View {
    let client: AudioEngineClient
    let profile: OutputProfileModel
    let channelCount: Int
    let onSaveProfile: (OutputProfileModel, @escaping (Bool) -> Void) -> Void

    @State private var availablePlugins: [AvailablePlugin] = []
    @State private var graphError: String? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let graphError {
                Text(graphError)
                    .font(.caption)
                    .foregroundColor(.red)
            }
            if let graph = profile.graph {
                if graph.isLinear {
                    LinearGraphRackEditorView(
                        graph: graph,
                        availablePlugins: availablePlugins,
                        onApply: { candidate in
                            pushGraph(candidate, base: profile)
                        },
                        onReorder: { order in
                            reorderGraph(order, base: profile)
                        }
                    )
                    .id("linear-\(profile.id)")
                } else {
                    PluginGraphEditorView(
                        graph: graph,
                        availablePlugins: availablePlugins,
                        onApply: { candidate in
                            pushGraph(candidate, base: profile)
                        }
                    )
                    .id("graph-\(profile.id)")
                }
            } else {
                ProfileRackEditor(
                    client: client,
                    profile: profile,
                    channelCount: channelCount,
                    availablePlugins: availablePlugins,
                    onSaveProfile: onSaveProfile
                )
                .id("rack-\(profile.id)")
            }
        }
        .onAppear(perform: loadAvailablePlugins)
    }

    private func loadAvailablePlugins() {
        if !availablePlugins.isEmpty { return }
        DispatchQueue.global(qos: .utility).async {
            let loaded = client.getAvailablePlugins() ?? []
            DispatchQueue.main.async {
                availablePlugins = loaded
            }
        }
    }

    private func pushGraph(_ candidate: PluginGraphModel, base: OutputProfileModel) -> Bool {
        graphError = nil
        var updated = base
        updated.graph = candidate
        updated.inputChannels = max(1, channelCount)
        updated.outputChannels = max(1, channelCount)
        // Optimistic like the live editors: keep editing responsive while
        // the store write is in flight; failures surface below.
        onSaveProfile(updated) { ok in
            DispatchQueue.main.async {
                if !ok {
                    graphError = "Profile rejected; the stored graph was not changed."
                }
            }
        }
        return true
    }

    private func reorderGraph(_ order: [Int], base: OutputProfileModel) -> Bool {
        guard var candidate = base.graph else { return false }
        var byID: [Int: PluginGraphNodeModel] = [:]
        for node in candidate.nodes {
            byID[node.id] = node
        }
        let reordered = order.compactMap { byID[$0] }
        guard reordered.count == candidate.nodes.count else { return false }
        candidate.nodes = reordered
        candidate.edges = zip(order, order.dropFirst()).map {
            PluginGraphEdgeModel(fromNode: $0.0, toNode: $0.1)
        }
        return pushGraph(candidate, base: base)
    }
}

// MARK: - Stored Rack Editor

/// Draft-based rack editor for a stored (non-live) profile. Reuses the
/// shared row and sheet components; every committed draft is pushed to the
/// profile store instead of the live chain.
struct ProfileRackEditor: View {
    let client: AudioEngineClient
    let profile: OutputProfileModel
    let channelCount: Int
    let availablePlugins: [AvailablePlugin]
    let onSaveProfile: (OutputProfileModel, @escaping (Bool) -> Void) -> Void

    @State private var draft: [PluginInstance] = []
    @State private var showingAddSheet = false
    @State private var editingPluginID: UUID? = nil
    @State private var errorMessage: String? = nil
    @State private var saving = false

    private var safeChannelCount: Int {
        min(max(channelCount, 1), 32)
    }

    private func descriptors(for pluginType: String) -> [PluginParameterDescriptor] {
        availablePlugins.first { $0.type_ == pluginType }?.parameters ?? []
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if draft.isEmpty {
                Text("Empty chain — audio passes through unprocessed. Add plugins to shape this output.")
                    .font(.callout)
                    .foregroundColor(.secondary)
            } else {
                List {
                    ForEach(draft) { plugin in
                        PluginRowView(
                            plugin: plugin,
                            requiredOutputChannels: safeChannelCount,
                            onEdit: { editingPluginID = plugin.id },
                            onRemove: { removePlugin(plugin.id) },
                            onStateChange: { inputChannels, bypassed in
                                if let inputChannels {
                                    plugin.inputChannels = inputChannels
                                }
                                if let bypassed {
                                    plugin.bypassed = bypassed
                                }
                                touchDraft()
                                commitDraft()
                            }
                        )
                    }
                    .onMove(perform: movePlugins)
                }
                .frame(minHeight: min(CGFloat(64 * draft.count + 16), 420))
            }

            if let errorMessage {
                Text(errorMessage)
                    .font(.caption)
                    .foregroundColor(.red)
            }

            HStack {
                Button {
                    showingAddSheet = true
                } label: {
                    Label("Add Plugin", systemImage: "plus")
                }
                if saving {
                    ProgressView()
                        .controlSize(.small)
                }
                Spacer()
            }
        }
        .onAppear {
            draft = profile.rackInstances()
        }
        .sheet(isPresented: $showingAddSheet) {
            AddPluginSheet(
                availablePlugins: availablePlugins,
                isLoading: false,
                channelCount: safeChannelCount,
                onAdd: { pluginType, parameters in
                    let pluginName = availablePlugins.first { $0.type_ == pluginType }?.name
                        ?? pluginDisplayName(pluginType)
                    draft.append(PluginInstance(
                        index: draft.count,
                        pluginType: pluginType,
                        pluginName: pluginName,
                        parameters: parameters,
                        inputChannels: safeChannelCount
                    ))
                    showingAddSheet = false
                    commitDraft()
                },
                onCancel: { showingAddSheet = false }
            )
            .frame(width: 640, height: 480)
        }
        .sheet(
            isPresented: Binding(
                get: { editingPluginID != nil },
                set: { isPresented in
                    if !isPresented {
                        editingPluginID = nil
                    }
                }
            )
        ) {
            if let pluginID = editingPluginID,
               let plugin = draft.first(where: { $0.id == pluginID }) {
                PluginEditSheet(
                    plugin: plugin,
                    descriptors: descriptors(for: plugin.pluginType),
                    onApply: { _ in
                        touchDraft()
                        commitDraft()
                        return true
                    },
                    onCancel: { editingPluginID = nil },
                    onClose: { editingPluginID = nil }
                )
                .frame(width: 720, height: 520)
            }
        }
    }

    private func touchDraft() {
        draft = draft.map { $0 }
    }

    private func removePlugin(_ id: UUID) {
        draft.removeAll { $0.id == id }
        commitDraft()
    }

    private func movePlugins(from source: IndexSet, to destination: Int) {
        draft.move(fromOffsets: source, toOffset: destination)
        commitDraft()
    }

    private func commitDraft() {
        errorMessage = nil
        var updated = profile
        updated.pluginDicts = draft.map { plugin in
            [
                "plugin_type": plugin.pluginType,
                "parameters": plugin.parameters,
                "input_channels": plugin.inputChannels,
                "bypassed": plugin.bypassed,
            ] as [String: Any]
        }
        updated.inputChannels = safeChannelCount
        updated.outputChannels = safeChannelCount
        saving = true
        onSaveProfile(updated) { ok in
            DispatchQueue.main.async {
                saving = false
                if !ok {
                    errorMessage = "Profile rejected; the stored chain was not changed."
                }
            }
        }
    }
}
