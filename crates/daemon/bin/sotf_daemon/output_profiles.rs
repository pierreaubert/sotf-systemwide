//! Per-output DSP profiles: one plugin chain per audio interface.
//!
//! Headphones and speakers need very different processing, so the daemon
//! keeps a named chain ("profile") per output device instead of a single
//! global chain. Devices are keyed by their stable CoreAudio persistent UID
//! (`uid:<uid>`) when the ConfigBar reports one, and fall back to
//! (`name:<name>`) otherwise. The `default` profile backs unknown devices
//! and is adopted from the live chain on first use, which also migrates
//! pre-profile daemons without losing their chain.
//!
//! Persistence lives in `output-profiles.json` next to
//! `systemwide-state.json`, using the same atomic-write discipline
//! (0600, temp file + rename). The chain-bearing file has a larger size cap
//! than the device-name state file because a profile holds full plugin
//! parameter blobs.

use serde::{Deserialize, Serialize};
use sotf_audio::PluginConfig;
use sotf_audio::engine::PluginGraphConfig;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const DEFAULT_PROFILE_ID: &str = "default";
pub(super) const DEFAULT_PROFILE_NAME: &str = "Default";

const PROFILES_FILE_NAME: &str = "output-profiles.json";
const PROFILES_VERSION: u32 = 1;
const MAX_PROFILES_FILE_BYTES: u64 = 256 * 1024;
static NEXT_PROFILES_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// A named DSP chain bound to one or more outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct OutputProfile {
    pub id: String,
    pub name: String,
    /// Either `"rack"` (linear [`PluginConfig`] list) or `"graph"`.
    pub topology: String,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    #[serde(default)]
    pub graph: Option<PluginGraphConfig>,
    pub input_channels: usize,
    pub output_channels: usize,
    pub updated_at_unix_ms: u64,
}

impl OutputProfile {
    pub(super) fn is_graph(&self) -> bool {
        self.topology == "graph"
    }

    fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("output profile id must not be empty".into());
        }
        if self.id.len() > 64 {
            return Err("output profile id must not exceed 64 characters".into());
        }
        if self.name.trim().is_empty() {
            return Err("output profile name must not be empty".into());
        }
        if self.name.len() > 80 {
            return Err("output profile name must not exceed 80 characters".into());
        }
        match self.topology.as_str() {
            "rack" => Ok(()),
            "graph" => self
                .graph
                .as_ref()
                .map(|_| ())
                .ok_or_else(|| "graph profiles must carry a graph".to_string()),
            other => Err(format!(
                "output profile topology must be \"rack\" or \"graph\", got \"{other}\""
            )),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedOutputProfiles {
    version: u32,
    profiles: Vec<OutputProfile>,
    assignments: HashMap<String, String>,
    current_device_uid: Option<String>,
    current_device_name: Option<String>,
    current_profile_id: Option<String>,
    next_id: u64,
}

/// Chain selected for cold start: either a rack plugin list or a graph.
#[derive(Debug, Clone)]
pub(super) enum StartupChain {
    Rack(Vec<PluginConfig>),
    Graph(PluginGraphConfig),
}

/// Daemon-owned store of per-output DSP profiles.
#[derive(Debug)]
pub(super) struct OutputProfileStore {
    profiles: HashMap<String, OutputProfile>,
    /// Device key (`uid:<uid>` or `name:<name>`) -> profile id. Written both
    /// by explicit assignment and, as last-used memory, by route switches.
    assignments: HashMap<String, String>,
    current_device_uid: Option<String>,
    current_device_name: Option<String>,
    current_profile_id: Option<String>,
    next_id: u64,
}

impl Default for OutputProfileStore {
    fn default() -> Self {
        Self {
            profiles: HashMap::new(),
            assignments: HashMap::new(),
            current_device_uid: None,
            current_device_name: None,
            current_profile_id: None,
            next_id: 1,
        }
    }
}

impl OutputProfileStore {
    /// Stable key for a device. UIDs survive replugs and disambiguate
    /// same-model interfaces; names are the portable fallback.
    pub(super) fn device_key(device_uid: Option<&str>, device_name: &str) -> String {
        match device_uid.map(str::trim).filter(|uid| !uid.is_empty()) {
            Some(uid) => format!("uid:{uid}"),
            None => format!("name:{}", device_name.trim()),
        }
    }

    pub(super) fn load() -> Self {
        let Some(path) = output_profiles_path() else {
            return Self::default();
        };
        match load_profiles_from_path(&path) {
            Some(persisted) => Self {
                profiles: persisted
                    .profiles
                    .into_iter()
                    .map(|profile| (profile.id.clone(), profile))
                    .collect(),
                assignments: persisted.assignments,
                current_device_uid: persisted.current_device_uid,
                current_device_name: persisted.current_device_name,
                current_profile_id: persisted.current_profile_id,
                next_id: persisted.next_id.max(1),
            },
            None => Self::default(),
        }
    }

    /// Persist the store. Returns true when the state reached disk. Without
    /// a resolvable state directory (no HOME in tests and minimal
    /// environments) the store stays session-only and reports false so
    /// callers can warn instead of failing the mutation.
    fn save(&self) -> Result<bool, String> {
        let Some(path) = output_profiles_path() else {
            return Ok(false);
        };
        let mut profiles: Vec<OutputProfile> = self.profiles.values().cloned().collect();
        profiles.sort_by(|a, b| a.id.cmp(&b.id));
        let persisted = PersistedOutputProfiles {
            version: PROFILES_VERSION,
            profiles,
            assignments: self.profiles_assignment_snapshot(),
            current_device_uid: self.current_device_uid.clone(),
            current_device_name: self.current_device_name.clone(),
            current_profile_id: self.current_profile_id.clone(),
            next_id: self.next_id,
        };
        save_profiles_to_path(&path, &persisted)
            .map_err(|error| format!("failed to persist output profiles: {error}"))?;
        Ok(true)
    }

    fn warn_if_session_only(persisted: bool, operation: &str) {
        if !persisted {
            log::warn!(
                "Output profiles are session-only ({operation}): no state directory available"
            );
        }
    }

    fn profiles_assignment_snapshot(&self) -> HashMap<String, String> {
        self.assignments.clone()
    }

    fn now_unix_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }

    /// Adopt the live chain as the `default` profile when the store is
    /// empty. Returns true when it created (and persisted) the profile.
    pub(super) fn ensure_default(
        &mut self,
        plugins: Vec<PluginConfig>,
        graph: Option<PluginGraphConfig>,
        input_channels: usize,
        output_channels: usize,
    ) -> Result<bool, String> {
        if !self.profiles.is_empty() {
            return Ok(false);
        }
        let (topology, stored_graph) = match graph {
            Some(graph) => ("graph".to_string(), Some(graph)),
            None => ("rack".to_string(), None),
        };
        self.profiles.insert(
            DEFAULT_PROFILE_ID.to_string(),
            OutputProfile {
                id: DEFAULT_PROFILE_ID.to_string(),
                name: DEFAULT_PROFILE_NAME.to_string(),
                topology,
                plugins,
                graph: stored_graph,
                input_channels: input_channels.max(1),
                output_channels: output_channels.max(1),
                updated_at_unix_ms: Self::now_unix_ms(),
            },
        );
        Self::warn_if_session_only(self.save()?, "adopt default");
        Ok(true)
    }

    pub(super) fn get(&self, profile_id: &str) -> Option<&OutputProfile> {
        self.profiles.get(profile_id)
    }

    pub(super) fn profiles_sorted(&self) -> Vec<&OutputProfile> {
        let mut profiles: Vec<&OutputProfile> = self.profiles.values().collect();
        profiles.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        profiles
    }

    pub(super) fn assignments(&self) -> &HashMap<String, String> {
        &self.assignments
    }

    pub(super) fn current(&self) -> (Option<String>, Option<String>, Option<String>) {
        (
            self.current_profile_id.clone(),
            self.current_device_uid.clone(),
            self.current_device_name.clone(),
        )
    }

    /// Resolve the profile id for a device: UID assignment, then name
    /// assignment, then the default profile.
    pub(super) fn resolve_profile_id(
        &self,
        device_uid: Option<&str>,
        device_name: &str,
    ) -> Option<String> {
        if let Some(uid) = device_uid.map(str::trim).filter(|uid| !uid.is_empty())
            && let Some(id) = self.assignments.get(&format!("uid:{uid}"))
            && self.profiles.contains_key(id)
        {
            return Some(id.clone());
        }
        if let Some(id) = self
            .assignments
            .get(&format!("name:{}", device_name.trim()))
            && self.profiles.contains_key(id)
        {
            return Some(id.clone());
        }
        self.profiles
            .contains_key(DEFAULT_PROFILE_ID)
            .then(|| DEFAULT_PROFILE_ID.to_string())
    }

    /// Insert or replace a profile. An empty id allocates `profile-<n>`.
    pub(super) fn upsert(&mut self, mut profile: OutputProfile) -> Result<String, String> {
        if profile.id.trim().is_empty() {
            profile.id = format!("profile-{}", self.next_id);
            self.next_id = self.next_id.saturating_add(1);
        }
        profile.updated_at_unix_ms = Self::now_unix_ms();
        profile.validate()?;
        let id = profile.id.clone();
        self.profiles.insert(id.clone(), profile);
        Self::warn_if_session_only(self.save()?, "upsert profile");
        Ok(id)
    }

    pub(super) fn remove(&mut self, profile_id: &str) -> Result<(), String> {
        if profile_id == DEFAULT_PROFILE_ID {
            return Err("the Default profile cannot be deleted".into());
        }
        if self.profiles.remove(profile_id).is_none() {
            return Err(format!("unknown output profile \"{profile_id}\""));
        }
        self.assignments
            .retain(|_, assigned| assigned != profile_id);
        if self.current_profile_id.as_deref() == Some(profile_id) {
            self.current_profile_id = Some(DEFAULT_PROFILE_ID.to_string());
        }
        Self::warn_if_session_only(self.save()?, "remove profile");
        Ok(())
    }

    pub(super) fn assign(
        &mut self,
        device_uid: Option<&str>,
        device_name: &str,
        profile_id: &str,
    ) -> Result<(), String> {
        if !self.profiles.contains_key(profile_id) {
            return Err(format!("unknown output profile \"{profile_id}\""));
        }
        if device_name.trim().is_empty() {
            return Err("device name must not be empty".into());
        }
        // Write the UID key and, when a UID is known, the name key as well so
        // name-based route resolution (legacy clients, UID-less commands)
        // finds the same profile. UID lookup still wins on read.
        self.assignments.insert(
            Self::device_key(device_uid, device_name),
            profile_id.to_string(),
        );
        if device_uid
            .map(str::trim)
            .filter(|uid| !uid.is_empty())
            .is_some()
        {
            self.assignments.insert(
                format!("name:{}", device_name.trim()),
                profile_id.to_string(),
            );
        }
        Self::warn_if_session_only(self.save()?, "assign profile");
        Ok(())
    }

    /// Set the current route without persisting (cold start re-affirms the
    /// on-disk state it just loaded).
    pub(super) fn set_current_in_memory(
        &mut self,
        device_uid: Option<String>,
        device_name: Option<String>,
        profile_id: Option<String>,
    ) {
        self.current_device_uid = device_uid;
        self.current_device_name = device_name;
        self.current_profile_id = profile_id;
    }

    /// Record a successful route application: current route plus last-used
    /// memory so a replugged device recalls its chain.
    pub(super) fn record_route(
        &mut self,
        device_uid: Option<&str>,
        device_name: &str,
        profile_id: &str,
    ) -> Result<(), String> {
        let uid = device_uid
            .map(str::trim)
            .filter(|uid| !uid.is_empty())
            .map(str::to_string);
        self.current_device_uid = uid.clone();
        self.current_device_name = Some(device_name.to_string());
        self.current_profile_id = Some(profile_id.to_string());
        self.assignments.insert(
            Self::device_key(uid.as_deref(), device_name),
            profile_id.to_string(),
        );
        if uid.is_some() {
            self.assignments.insert(
                format!("name:{}", device_name.trim()),
                profile_id.to_string(),
            );
        }
        Self::warn_if_session_only(self.save()?, "record route");
        Ok(())
    }

    /// Write the live chain through to the current profile so edits made on
    /// the live route are kept when the user switches outputs and back.
    /// Structural callers persist; the per-parameter hot path stays
    /// allocation- and fsync-free and is picked up by the next save.
    pub(super) fn sync_live_chain(
        &mut self,
        plugins: Vec<PluginConfig>,
        graph: Option<PluginGraphConfig>,
        input_channels: usize,
        output_channels: usize,
        persist: bool,
    ) -> Result<(), String> {
        let profile_id = self
            .current_profile_id
            .clone()
            .unwrap_or_else(|| DEFAULT_PROFILE_ID.to_string());
        let (topology, stored_graph) = match graph {
            Some(graph) => ("graph".to_string(), Some(graph)),
            None => ("rack".to_string(), None),
        };
        let name = self
            .profiles
            .get(&profile_id)
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| {
                if profile_id == DEFAULT_PROFILE_ID {
                    DEFAULT_PROFILE_NAME.to_string()
                } else {
                    profile_id.clone()
                }
            });
        self.profiles.insert(
            profile_id,
            OutputProfile {
                id: self
                    .current_profile_id
                    .clone()
                    .unwrap_or_else(|| DEFAULT_PROFILE_ID.to_string()),
                name,
                topology,
                plugins,
                graph: stored_graph,
                input_channels: input_channels.max(1),
                output_channels: output_channels.max(1),
                updated_at_unix_ms: Self::now_unix_ms(),
            },
        );
        if persist {
            Self::warn_if_session_only(self.save()?, "sync live chain");
        }
        Ok(())
    }
}

fn output_profiles_path() -> Option<PathBuf> {
    super::configured::systemwide_state_dir().map(|dir| dir.join(PROFILES_FILE_NAME))
}

fn load_profiles_from_path(path: &Path) -> Option<PersistedOutputProfiles> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_PROFILES_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PROFILES_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_PROFILES_FILE_BYTES {
        return None;
    }
    let persisted: PersistedOutputProfiles = serde_json::from_slice(&bytes).ok()?;
    if persisted.version != PROFILES_VERSION {
        return None;
    }
    if persisted
        .profiles
        .iter()
        .any(|profile| profile.validate().is_err())
    {
        return None;
    }
    Some(persisted)
}

fn save_profiles_to_path(path: &Path, persisted: &PersistedOutputProfiles) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "output profiles path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(persisted)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if bytes.len() as u64 > MAX_PROFILES_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "output profiles exceed the on-disk size limit",
        ));
    }
    let temp_path = parent.join(format!(
        ".output-profiles-{}-{}.tmp",
        std::process::id(),
        NEXT_PROFILES_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rack_profile(id: &str) -> OutputProfile {
        OutputProfile {
            id: id.to_string(),
            name: format!("name-{id}"),
            topology: "rack".to_string(),
            plugins: vec![PluginConfig::new(
                "gain",
                serde_json::json!({"gain_db": -3.0}),
            )],
            graph: None,
            input_channels: 2,
            output_channels: 2,
            updated_at_unix_ms: 0,
        }
    }

    #[test]
    fn device_key_prefers_uid_over_name() {
        assert_eq!(
            OutputProfileStore::device_key(Some("  abc "), "Speakers"),
            "uid:abc"
        );
        assert_eq!(
            OutputProfileStore::device_key(Some(""), "Speakers"),
            "name:Speakers"
        );
        assert_eq!(
            OutputProfileStore::device_key(None, "Speakers"),
            "name:Speakers"
        );
    }

    #[test]
    fn resolve_prefers_uid_then_name_then_default() {
        let mut store = OutputProfileStore::default();
        store.profiles.insert(
            DEFAULT_PROFILE_ID.to_string(),
            rack_profile(DEFAULT_PROFILE_ID),
        );
        store
            .profiles
            .insert("p-head".to_string(), rack_profile("p-head"));
        store
            .profiles
            .insert("p-name".to_string(), rack_profile("p-name"));
        store
            .assignments
            .insert("uid:AAA".to_string(), "p-head".to_string());
        store
            .assignments
            .insert("name:Speakers".to_string(), "p-name".to_string());
        store
            .assignments
            .insert("uid:STALE".to_string(), "p-deleted".to_string());

        assert_eq!(
            store.resolve_profile_id(Some("AAA"), "Whatever"),
            Some("p-head".to_string())
        );
        assert_eq!(
            store.resolve_profile_id(None, "Speakers"),
            Some("p-name".to_string())
        );
        assert_eq!(
            store.resolve_profile_id(Some("STALE"), "Unknown Device"),
            Some(DEFAULT_PROFILE_ID.to_string())
        );
        assert_eq!(
            store.resolve_profile_id(None, "Unknown Device"),
            Some(DEFAULT_PROFILE_ID.to_string())
        );
    }

    #[test]
    fn upsert_allocates_ids_and_rejects_bad_topology() {
        let mut store = OutputProfileStore::default();
        let mut profile = rack_profile("");
        profile.name = "Headphones".to_string();
        let id = store.upsert(profile).unwrap();
        assert_eq!(id, "profile-1");
        assert!(store.get(&id).is_some());

        let mut bad = rack_profile("bad");
        bad.topology = "mesh".to_string();
        assert!(store.upsert(bad).is_err());

        let mut graphless = rack_profile("graphless");
        graphless.topology = "graph".to_string();
        assert!(store.upsert(graphless).is_err());
    }

    #[test]
    fn remove_reassigns_and_protects_default() {
        let mut store = OutputProfileStore::default();
        store.profiles.insert(
            DEFAULT_PROFILE_ID.to_string(),
            rack_profile(DEFAULT_PROFILE_ID),
        );
        store
            .profiles
            .insert("p-x".to_string(), rack_profile("p-x"));
        store
            .assignments
            .insert("uid:1".to_string(), "p-x".to_string());
        store.current_profile_id = Some("p-x".to_string());

        assert!(store.remove(DEFAULT_PROFILE_ID).is_err());
        assert!(store.remove("p-x").is_ok());
        assert!(store.assignments.is_empty());
        assert_eq!(
            store.current_profile_id.as_deref(),
            Some(DEFAULT_PROFILE_ID)
        );
        assert!(store.remove("p-x").is_err());
    }

    #[test]
    fn sync_live_chain_updates_current_profile_only() {
        let mut store = OutputProfileStore::default();
        store.profiles.insert(
            DEFAULT_PROFILE_ID.to_string(),
            rack_profile(DEFAULT_PROFILE_ID),
        );
        store
            .profiles
            .insert("p-head".to_string(), rack_profile("p-head"));
        store.current_profile_id = Some("p-head".to_string());

        let live = vec![PluginConfig::new(
            "gain",
            serde_json::json!({"gain_db": 6.0}),
        )];
        store
            .sync_live_chain(live.clone(), None, 2, 2, false)
            .unwrap();

        let head_plugins = serde_json::to_value(&store.get("p-head").expect("head").plugins)
            .expect("plugins serialize");
        assert_eq!(
            head_plugins,
            serde_json::to_value(&live).expect("live plugins serialize")
        );
        assert_eq!(store.get("p-head").expect("head").topology, "rack");
        // The untouched profile keeps its original plugin, not the live one.
        let default_plugins =
            serde_json::to_value(&store.get(DEFAULT_PROFILE_ID).expect("default").plugins)
                .expect("plugins serialize");
        assert_ne!(default_plugins, head_plugins);
    }

    #[test]
    fn profiles_save_and_load_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PROFILES_FILE_NAME);
        let mut store = OutputProfileStore::default();
        let id = store.upsert(rack_profile("")).unwrap();
        store.assign(Some("UID-1"), "Headphones", &id).unwrap();
        store
            .record_route(Some("UID-1"), "Headphones", &id)
            .unwrap();

        // The store above ran session-only (no state dir in tests); persist
        // explicitly to the temp path and reload.
        let persisted = PersistedOutputProfiles {
            version: PROFILES_VERSION,
            profiles: store.profiles_sorted().into_iter().cloned().collect(),
            assignments: store.assignments.clone(),
            current_device_uid: store.current_device_uid.clone(),
            current_device_name: store.current_device_name.clone(),
            current_profile_id: store.current_profile_id.clone(),
            next_id: store.next_id,
        };
        save_profiles_to_path(&path, &persisted).unwrap();
        let reloaded = load_profiles_from_path(&path).expect("profiles reload");
        assert_eq!(reloaded.profiles.len(), 1);
        assert_eq!(
            reloaded.assignments.get("uid:UID-1").map(String::as_str),
            Some(id.as_str())
        );
        assert_eq!(reloaded.current_device_uid.as_deref(), Some("UID-1"));
        assert_eq!(reloaded.current_profile_id.as_deref(), Some(id.as_str()));

        // Version drift and invalid profiles refuse to load.
        let mut wrong_version = serde_json::to_value(&persisted).unwrap();
        wrong_version["version"] = serde_json::json!(PROFILES_VERSION + 1);
        std::fs::write(&path, serde_json::to_vec(&wrong_version).unwrap()).unwrap();
        assert!(load_profiles_from_path(&path).is_none());
    }

    #[test]
    fn profiles_reject_oversized_or_corrupt_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PROFILES_FILE_NAME);
        std::fs::write(&path, b"not-json").unwrap();
        assert!(load_profiles_from_path(&path).is_none());

        std::fs::write(&path, vec![b'x'; MAX_PROFILES_FILE_BYTES as usize + 1]).unwrap();
        assert!(load_profiles_from_path(&path).is_none());
    }
}
