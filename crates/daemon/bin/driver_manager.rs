//! Platform Audio Driver Manager
//!
//! Manages the lifecycle of the platform-specific audio capture driver.
//! On macOS with the `hal` feature, uses the CoreAudio HAL driver. On Linux
//! and Windows, uses the `cpal`-backed capture driver (PipeWire monitor /
//! WASAPI capture). Falls back to NullDriver when no driver is available.

use crate::cpal_capture::CpalCaptureDriver;
use driver_common::{AudioDriver, ConfigResult, DriverConfig, DriverError, DriverStatus};
use sotf_audio::SinkType;

const DRIVER_OVERRIDE_ENV: &str = "SOTF_SYSTEMWIDE_DRIVER";

/// Driver Manager - handles platform driver lifecycle
pub struct DriverManager {
    driver: Box<dyn AudioDriver>,
    engine_ready: bool,
    lab_backend: bool,
    #[cfg(test)]
    test_lab_output: bool,
    #[cfg(test)]
    test_output_sink: Option<SinkType>,
}

impl DriverManager {
    /// Create a new driver manager with the appropriate platform driver.
    pub fn new() -> Self {
        let choice = std::env::var(DRIVER_OVERRIDE_ENV).ok();
        let lab_backend = choice.as_deref().is_some_and(|choice| {
            matches!(choice.trim().to_ascii_lowercase().as_str(), "fake" | "lab")
        });
        Self {
            driver: create_platform_driver_for_choice(choice.as_deref()),
            engine_ready: false,
            lab_backend,
            #[cfg(test)]
            test_lab_output: false,
            #[cfg(test)]
            test_output_sink: None,
        }
    }

    #[cfg(test)]
    pub fn from_driver(driver: Box<dyn AudioDriver>) -> Self {
        Self {
            driver,
            engine_ready: false,
            lab_backend: false,
            test_lab_output: false,
            test_output_sink: None,
        }
    }

    #[cfg(test)]
    pub fn from_driver_with_lab_output(driver: Box<dyn AudioDriver>) -> Self {
        let mut manager = Self::from_driver(driver);
        manager.test_lab_output = true;
        manager
    }

    pub fn is_lab_backend(&self) -> bool {
        self.lab_backend
    }

    #[cfg(test)]
    pub fn set_test_output(&mut self, output: sotf_audio::lab_pcm::TestOutput) {
        self.test_output_sink = Some(SinkType::TestOutput(output));
    }

    pub fn output_sink_type(&self) -> SinkType {
        #[cfg(test)]
        if let Some(sink) = &self.test_output_sink {
            return sink.clone();
        }
        #[cfg(test)]
        if self.test_lab_output {
            return SinkType::LabNull;
        }
        if self.lab_backend {
            SinkType::LabNull
        } else {
            SinkType::Cpal
        }
    }

    /// Initialize the driver and verify connectivity.
    pub fn initialize(&mut self) -> Result<(), DriverError> {
        self.driver.initialize()
    }

    /// Shut down the driver.
    pub fn shutdown(&mut self) {
        if self.engine_ready {
            self.driver.set_engine_ready(false);
            self.engine_ready = false;
        }
        self.driver.shutdown();
    }

    /// Get current driver status.
    pub fn status(&self) -> DriverStatus {
        self.driver.status()
    }

    /// Share sample transport with the engine without moving device ownership.
    pub fn capture_source(&self) -> Option<driver_common::CaptureSource> {
        self.driver.capture_source()
    }

    /// Get a mutable reference to the underlying driver.
    #[allow(dead_code)]
    pub fn driver_mut(&mut self) -> &mut dyn AudioDriver {
        &mut *self.driver
    }

    /// Get an immutable reference to the underlying driver.
    #[allow(dead_code)]
    pub fn driver(&self) -> &dyn AudioDriver {
        &*self.driver
    }

    /// Set engine_ready flag on the driver.
    pub fn set_engine_ready(&mut self, ready: bool) -> bool {
        if self.engine_ready == ready && self.driver.engine_ready_matches(ready) {
            return true;
        }
        match self.driver.set_engine_ready_checked(ready) {
            Ok(()) => {
                self.engine_ready = ready;
                true
            }
            Err(error) => {
                log::warn!("Driver readiness remains pending: {error}");
                false
            }
        }
    }

    pub fn readiness_is_current(&self) -> bool {
        self.engine_ready && self.driver.engine_ready_matches(true)
    }

    /// Poll for driver-initiated config changes.
    /// Poll without losing a cross-process request identity.
    pub fn poll_config_request(&mut self) -> Option<driver_common::DriverConfigRequest> {
        self.driver.poll_config_request()
    }

    /// Recheck an intent after waiting for debounce and the transition guard.
    pub fn config_request_is_current(&mut self, request_id: u64) -> bool {
        self.driver.config_request_is_current(request_id)
    }

    /// Publish the result only if it still belongs to the current request.
    pub fn acknowledge_config_request(
        &mut self,
        request_id: u64,
        actual: DriverConfig,
        result: ConfigResult,
    ) -> bool {
        self.driver
            .acknowledge_config_request(request_id, actual, result)
    }

    /// Resolve a discarded mailbox request without affecting a newer identity.
    pub fn reject_superseded_config_request(&mut self, request_id: u64) {
        if request_id == 0 {
            return;
        }
        let status = self.driver.status();
        self.driver.acknowledge_config_request(
            request_id,
            DriverConfig::new(
                status.sample_rate,
                status.buffer_frames,
                status.channel_count,
            ),
            ConfigResult::error("Superseded by a newer daemon control intent"),
        );
    }

    /// Request a configuration change.
    pub fn request_config(&mut self, config: DriverConfig) -> ConfigResult {
        self.driver.request_config(config)
    }

    /// Current driver-published output volume (scalar, muted, generation),
    /// when the driver exposes an OS-visible volume control.
    pub fn output_volume(&mut self) -> Option<(f32, bool, u32)> {
        self.driver.output_volume()
    }

    /// Publish a daemon-originated volume change to the driver's control.
    /// Returns the new generation when the driver has a volume control.
    pub fn set_output_volume(&mut self, volume: f32, muted: bool) -> Option<u32> {
        self.driver.set_output_volume(volume, muted)
    }

    /// Patch a user control without replacing a concurrent mute/volume edit.
    pub fn patch_output_volume(
        &mut self,
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Option<(f32, bool, u32)> {
        self.driver.patch_output_volume(volume, muted)
    }
}

/// Create the appropriate platform audio driver.
fn create_platform_driver_for_choice(choice: Option<&str>) -> Box<dyn AudioDriver> {
    match choice.map(|value| value.trim().to_ascii_lowercase()) {
        Some(choice) if choice == "fake" || choice == "lab" => {
            log::info!("[DriverManager] Using systemwide lab fake driver");
            return Box::new(LabDriver::new());
        }
        Some(choice) if choice == "null" => {
            log::info!(
                "[DriverManager] Forcing NullDriver via {}",
                DRIVER_OVERRIDE_ENV
            );
            return Box::new(driver_common::NullDriver::new());
        }
        Some(choice) if choice == "cpal" => {
            log::info!("[DriverManager] Forcing cpal capture driver via {DRIVER_OVERRIDE_ENV}");
            return Box::new(CpalCaptureDriver::new());
        }
        Some(choice) if !choice.is_empty() => {
            log::warn!(
                "[DriverManager] Ignoring unknown {}={}",
                DRIVER_OVERRIDE_ENV,
                choice
            );
        }
        _ => {}
    }

    #[cfg(all(target_os = "macos", feature = "hal"))]
    {
        log::info!("[DriverManager] Creating macOS HAL driver");
        return Box::new(driver_hal::HalDriver::new());
    }

    // Linux/Windows capture through cpal input streams (PipeWire monitor /
    // WASAPI capture). The driver degrades to installed=false when no input
    // device exists, so headless machines keep status-only behavior.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        log::info!("[DriverManager] Creating cpal capture driver");
        return Box::new(CpalCaptureDriver::new());
    }

    #[allow(unreachable_code)]
    {
        log::info!("[DriverManager] No platform driver available, using NullDriver");
        Box::new(driver_common::NullDriver::new())
    }
}

#[derive(Debug)]
struct LabDriver {
    status: DriverStatus,
    phase: std::sync::Arc<parking_lot::Mutex<f32>>,
    capture_config: std::sync::Arc<parking_lot::Mutex<DriverConfig>>,
    capture_initialized: std::sync::Arc<std::sync::atomic::AtomicBool>,
    engine_ready: bool,
    idle_marker: Option<std::path::PathBuf>,
}

impl LabDriver {
    fn new() -> Self {
        Self {
            status: DriverStatus::new(
                true,
                true,
                false,
                48_000,
                2,
                512,
                "Systemwide Lab Driver",
                true,
                0,
            ),
            phase: std::sync::Arc::new(parking_lot::Mutex::new(0.0)),
            capture_config: std::sync::Arc::new(parking_lot::Mutex::new(DriverConfig::new(
                48_000, 512, 2,
            ))),
            capture_initialized: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            engine_ready: false,
            idle_marker: std::env::var_os("SOTF_SYSTEMWIDE_RUNTIME_DIR")
                .map(|dir| std::path::PathBuf::from(dir).join("lab-capture-idle")),
        }
    }

    fn capture_active(&self) -> bool {
        self.status.capture_active
            && !self
                .idle_marker
                .as_ref()
                .is_some_and(|marker| marker.exists())
    }
}

#[derive(Debug)]
struct LabCaptureInput {
    phase: std::sync::Arc<parking_lot::Mutex<f32>>,
    config: std::sync::Arc<parking_lot::Mutex<DriverConfig>>,
    initialized: std::sync::Arc<std::sync::atomic::AtomicBool>,
    idle_marker: Option<std::path::PathBuf>,
}

impl driver_common::CaptureInput for LabCaptureInput {
    fn configuration(&self) -> DriverConfig {
        *self.config.lock()
    }

    fn available_frames(&self) -> usize {
        if self.initialized.load(std::sync::atomic::Ordering::Acquire)
            && !self.idle_marker.as_ref().is_some_and(|path| path.exists())
        {
            4096
        } else {
            0
        }
    }

    fn read_frames(&self, buffer: &mut [f32], expected: DriverConfig) -> usize {
        let config = self.config.lock();
        if *config != expected
            || self.available_frames() == 0
            || config.sample_rate == 0
            || config.channel_count == 0
        {
            return 0;
        }
        let channels = config.channel_count as usize;
        let frames = buffer.len() / channels;
        let step = 440.0 / config.sample_rate as f32;
        let mut phase = self.phase.lock();
        for frame in buffer[..frames * channels].chunks_exact_mut(channels) {
            let sample = (*phase * std::f32::consts::TAU).sin() * 0.1;
            *phase = (*phase + step) % 1.0;
            frame.fill(sample);
        }
        frames
    }
}

impl AudioDriver for LabDriver {
    fn initialize(&mut self) -> Result<(), DriverError> {
        self.status.capture_active = true;
        self.capture_initialized
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn shutdown(&mut self) {
        self.status.capture_active = false;
        self.capture_initialized
            .store(false, std::sync::atomic::Ordering::Release);
    }

    fn status(&self) -> DriverStatus {
        let mut status = self.status.clone();
        status.capture_active = self.capture_active();
        status
    }

    fn read_audio(&mut self, buffer: &mut [f32]) -> usize {
        if !self.capture_active() || !self.engine_ready || self.status.sample_rate == 0 {
            buffer.fill(0.0);
            return 0;
        }

        let channels = self.status.channel_count.max(1) as usize;
        let sample_rate = self.status.sample_rate as f32;
        let step = 440.0 / sample_rate;
        let mut phase = self.phase.lock();

        let complete_samples = buffer.len() - (buffer.len() % channels);
        for frame in buffer[..complete_samples].chunks_exact_mut(channels) {
            let sample = (*phase * std::f32::consts::TAU).sin() * 0.1;
            *phase = (*phase + step) % 1.0;
            for out in frame {
                *out = sample;
            }
        }

        complete_samples / channels
    }

    fn capture_source(&self) -> Option<driver_common::CaptureSource> {
        Some(driver_common::CaptureSource::new(LabCaptureInput {
            phase: std::sync::Arc::clone(&self.phase),
            config: std::sync::Arc::clone(&self.capture_config),
            initialized: std::sync::Arc::clone(&self.capture_initialized),
            idle_marker: self.idle_marker.clone(),
        }))
    }

    fn available_frames(&self) -> usize {
        if self.capture_active() {
            self.status.buffer_frames as usize
        } else {
            0
        }
    }

    fn sample_rate(&self) -> u32 {
        self.status.sample_rate
    }

    fn channel_count(&self) -> u32 {
        self.status.channel_count
    }

    fn request_config(&mut self, config: DriverConfig) -> ConfigResult {
        if config.sample_rate != 0 {
            self.status.sample_rate = config.sample_rate;
        }
        if config.buffer_frames != 0 {
            self.status.buffer_frames = config.buffer_frames;
        }
        if config.channel_count != 0 {
            self.status.channel_count = config.channel_count;
        }
        *self.capture_config.lock() = DriverConfig::new(
            self.status.sample_rate,
            self.status.buffer_frames,
            self.status.channel_count,
        );
        ConfigResult::Accepted
    }

    fn poll_config_change(&mut self) -> Option<DriverConfig> {
        None
    }

    fn acknowledge_config_change(&mut self, _actual: DriverConfig, _result: ConfigResult) {}

    fn set_engine_ready(&mut self, ready: bool) {
        self.engine_ready = ready;
    }
}

/// Get driver status (convenience function matching old API)
pub fn get_driver_status(manager: &DriverManager) -> DriverStatus {
    manager.status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CountingDriver {
        ready_calls: Arc<AtomicUsize>,
        ready: bool,
        ready_failures: usize,
    }

    #[test]
    fn injected_driver_lab_output_is_explicit_and_keeps_driver_identity() {
        let driver = || CountingDriver {
            ready_calls: Arc::new(AtomicUsize::new(0)),
            ready: false,
            ready_failures: 0,
        };
        let ordinary = DriverManager::from_driver(Box::new(driver()));
        assert_eq!(ordinary.output_sink_type(), SinkType::Cpal);
        assert!(!ordinary.is_lab_backend());

        let isolated = DriverManager::from_driver_with_lab_output(Box::new(driver()));
        assert_eq!(isolated.output_sink_type(), SinkType::LabNull);
        assert!(!isolated.is_lab_backend());
        assert_eq!(isolated.status().driver_name, "counting");
    }

    impl AudioDriver for CountingDriver {
        fn initialize(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
        fn shutdown(&mut self) {}
        fn status(&self) -> DriverStatus {
            DriverStatus::new(true, true, false, 48_000, 2, 512, "counting", self.ready, 0)
        }
        fn read_audio(&mut self, buffer: &mut [f32]) -> usize {
            buffer.fill(0.0);
            0
        }
        fn available_frames(&self) -> usize {
            0
        }
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channel_count(&self) -> u32 {
            2
        }
        fn request_config(&mut self, _config: DriverConfig) -> ConfigResult {
            ConfigResult::Accepted
        }
        fn poll_config_change(&mut self) -> Option<DriverConfig> {
            None
        }
        fn acknowledge_config_change(&mut self, _actual: DriverConfig, _result: ConfigResult) {}
        fn set_engine_ready(&mut self, ready: bool) {
            self.ready_calls.fetch_add(1, Ordering::Relaxed);
            self.ready = ready;
        }
        fn set_engine_ready_checked(&mut self, ready: bool) -> Result<(), DriverError> {
            if ready && self.ready_failures > 0 {
                self.ready_failures -= 1;
                return Err(DriverError::not_available("injected unavailable mapping"));
            }
            self.set_engine_ready(ready);
            Ok(())
        }
        fn engine_ready_matches(&self, ready: bool) -> bool {
            self.ready == ready
        }
    }

    #[test]
    fn set_engine_ready_is_idempotent_and_shutdown_clears_state() {
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = CountingDriver {
            ready_calls: Arc::clone(&calls),
            ready: false,
            ready_failures: 0,
        };
        let mut manager = DriverManager::from_driver(Box::new(driver));

        manager.set_engine_ready(true);
        manager.set_engine_ready(true);
        manager.set_engine_ready(false);
        manager.set_engine_ready(false);
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        manager.set_engine_ready(true);
        manager.shutdown();
        assert_eq!(calls.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn failed_or_lost_readiness_publication_is_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut manager = DriverManager::from_driver(Box::new(CountingDriver {
            ready_calls: Arc::clone(&calls),
            ready: false,
            ready_failures: 1,
        }));
        assert!(!manager.set_engine_ready(true));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(manager.set_engine_ready(true));
        assert!(manager.set_engine_ready(true));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        // An externally replaced mapping may lose a previously cached flag.
        manager.driver_mut().set_engine_ready(false);
        assert!(manager.set_engine_ready(true));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_driver_manager_creation() {
        let manager = DriverManager::new();
        let status = manager.status();
        // macOS+hal selects the HAL driver, Linux/Windows select the cpal
        // capture driver; both report platform support. Other configurations
        // fall back to NullDriver.
        #[cfg(any(
            all(target_os = "macos", feature = "hal"),
            target_os = "linux",
            target_os = "windows"
        ))]
        assert!(status.platform_supported);
        #[cfg(not(any(
            all(target_os = "macos", feature = "hal"),
            target_os = "linux",
            target_os = "windows"
        )))]
        assert!(!status.platform_supported);
        let _ = status;
    }

    #[test]
    fn driver_override_can_force_cpal_capture_driver() {
        let driver = create_platform_driver_for_choice(Some("cpal"));
        let status = driver.status();
        assert!(status.platform_supported);
    }

    #[test]
    fn driver_override_can_force_lab_driver_without_installed_hal() {
        let mut driver = create_platform_driver_for_choice(Some("lab"));
        driver.initialize().expect("lab driver initializes");
        driver.set_engine_ready(true);

        let status = driver.status();
        assert_eq!(status.driver_name, "Systemwide Lab Driver");
        assert!(status.platform_supported);
        assert!(status.driver_installed);
        assert!(status.capture_active);

        let mut buffer = vec![0.0; (status.buffer_frames * status.channel_count) as usize];
        let expected_frames = buffer.len() / status.channel_count as usize;
        assert_eq!(driver.available_frames(), status.buffer_frames as usize);
        assert_eq!(driver.read_audio(&mut buffer), expected_frames);
        assert!(buffer.iter().any(|sample| *sample != 0.0));
    }

    #[test]
    fn lab_driver_never_writes_or_reports_a_partial_frame() {
        let mut driver = create_platform_driver_for_choice(Some("lab"));
        driver.initialize().expect("lab driver initializes");
        driver.set_engine_ready(true);

        let mut buffer = vec![f32::NAN; 7];
        assert_eq!(driver.read_audio(&mut buffer), 3);
        assert!(buffer[..6].iter().all(|sample| sample.is_finite()));
        assert!(
            buffer[6].is_nan(),
            "partial-frame tail must remain untouched"
        );
    }

    #[test]
    fn lab_engine_source_produces_audio_before_readiness_and_stops_with_driver() {
        let mut driver = LabDriver::new();
        let source = driver.capture_source().expect("explicit lab source");
        assert_eq!(source.available_frames(), 0);
        driver.initialize().expect("initialize capture");
        let geometry = source.configuration();
        let mut pcm = [f32::NAN; 7];
        assert_eq!(source.read_frames(&mut pcm, geometry), 3);
        assert_eq!(pcm[0], pcm[1]);
        assert_eq!(pcm[2], pcm[3]);
        assert!(pcm[2] > 0.0);
        assert!(pcm[6].is_nan());
        driver.request_config(DriverConfig::new(96_000, 256, 4));
        assert_eq!(source.read_frames(&mut pcm, geometry), 0);
        driver.shutdown();
        assert_eq!(source.available_frames(), 0);
        assert_eq!(source.read_frames(&mut pcm, source.configuration()), 0);
    }

    #[test]
    fn lab_driver_conforms_to_audio_driver_contract() {
        driver_common::test_support::assert_audio_driver_contract(LabDriver::new())
            .expect("LabDriver contract");
    }

    #[test]
    fn driver_override_can_force_null_driver() {
        let driver = create_platform_driver_for_choice(Some("null"));
        let status = driver.status();

        assert_eq!(status.driver_name, "None");
        assert!(!status.platform_supported);
    }
}
