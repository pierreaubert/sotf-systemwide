//! Listening for physical output-device changes made outside the daemon.
//!
//! On macOS, changing an interface's sample rate in Audio MIDI Setup (or
//! in another host) currently surfaces only when the daemon next polls or
//! reconfigures, so playback can run at the wrong speed until then.
//! CoreAudio exposes property listeners for exactly this: the device's
//! nominal sample rate ('nsrt'), the system device list ('dev#'), and the
//! default output device ('dOut'). This module registers those listeners
//! and forwards coalesced events to the driver config watcher, which
//! treats an external nominal-rate change like a HAL config request
//! (debounced, negotiated against the device, acknowledged back).
//!
//! Other platforms get an inert stub with the same API. Set
//! `SOTF_DEVICE_LISTENER=0` to disable registration on macOS as well.

use driver_common::{DriverConfig, DriverStatus};

/// A coalescable notification that output-device state may have changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(any(target_os = "macos", test)),
    allow(
        dead_code,
        reason = "the portable listener is inert; CoreAudio constructs these notifications"
    )
)]
pub(super) enum DevicePropertyEvent {
    NominalRate,
    DeviceList,
    DefaultOutput,
}

/// Bounded notifications: callbacks publish bits, and the control owner rereads
/// current hardware state. Repeated churn cannot allocate or build a backlog.
#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct PendingDeviceEvents(std::sync::atomic::AtomicU8);

#[cfg(any(target_os = "macos", test))]
impl PendingDeviceEvents {
    fn publish(&self, event: DevicePropertyEvent) {
        self.0
            .fetch_or(1 << event as u8, std::sync::atomic::Ordering::Release);
    }

    fn drain(&self) -> Vec<DevicePropertyEvent> {
        let bits = self.0.swap(0, std::sync::atomic::Ordering::AcqRel);
        [
            DevicePropertyEvent::NominalRate,
            DevicePropertyEvent::DeviceList,
            DevicePropertyEvent::DefaultOutput,
        ]
        .into_iter()
        .filter(|event| bits & (1 << *event as u8) != 0)
        .collect()
    }
}

/// Build the driver config request for an externally observed nominal
/// sample rate, or `None` when the pipeline already runs at that rate.
///
/// The equality case doubles as the feedback guard: opening the device at
/// the negotiated rate sets its nominal rate, which fires the listener
/// again with the rate the engine already runs at. Buffer frames scale
/// with the rate so the block duration stays constant.
pub(super) fn external_rate_request(
    driver_status: &DriverStatus,
    nominal_rate: Option<u32>,
) -> Option<DriverConfig> {
    let nominal_rate = nominal_rate?;
    if nominal_rate == 0 || nominal_rate == driver_status.sample_rate {
        return None;
    }
    let buffer_frames = if driver_status.buffer_frames > 0 {
        driver_status.buffer_frames
    } else {
        512
    };
    let channel_count = if driver_status.channel_count > 0 {
        driver_status.channel_count
    } else {
        2
    };
    let scaled_frames = super::misc::scaled_buffer_frames_for_rate_change(
        driver_status.sample_rate,
        nominal_rate,
        buffer_frames,
    );
    Some(DriverConfig::new(
        nominal_rate,
        scaled_frames,
        channel_count,
    ))
}

#[cfg(not(target_os = "macos"))]
pub(super) struct OutputDeviceListener;

#[cfg(not(target_os = "macos"))]
impl OutputDeviceListener {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn watch_output_device(&mut self, _name: Option<&str>) {}

    pub(super) fn invalidate_watch(&mut self) {}

    pub(super) fn drain_events(&mut self) -> Vec<DevicePropertyEvent> {
        Vec::new()
    }

    pub(super) fn nominal_sample_rate(&self, _name: &str) -> Option<u32> {
        None
    }
}

#[cfg(target_os = "macos")]
pub(super) use macos::OutputDeviceListener;

#[cfg(target_os = "macos")]
mod macos {
    use super::{DevicePropertyEvent, PendingDeviceEvents};
    use std::ffi::{c_char, c_void};

    type AudioObjectID = u32;
    type OSStatus = i32;

    const NO_ERR: OSStatus = 0;
    const SYSTEM_OBJECT: AudioObjectID = 1;
    const SCOPE_GLOBAL: u32 = 0x676C_6F62; // 'glob'
    const SELECTOR_DEVICES: u32 = 0x6465_7623; // 'dev#'
    const SELECTOR_DEFAULT_OUTPUT: u32 = 0x644F_7574; // 'dOut'
    const SELECTOR_NOMINAL_SAMPLE_RATE: u32 = 0x6E73_7274; // 'nsrt'
    const SELECTOR_NAME: u32 = 0x6C6E_616D; // 'lnam'
    const ENCODING_UTF8: u32 = 0x0800_0100;

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct PropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    impl PropertyAddress {
        fn global(selector: u32) -> Self {
            Self {
                selector,
                scope: SCOPE_GLOBAL,
                element: 0,
            }
        }
    }

    type ListenerProc =
        unsafe extern "C" fn(AudioObjectID, u32, *const PropertyAddress, *mut c_void) -> OSStatus;

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectAddPropertyListener(
            object_id: AudioObjectID,
            address: *const PropertyAddress,
            listener: ListenerProc,
            client_data: *mut c_void,
        ) -> OSStatus;
        fn AudioObjectRemovePropertyListener(
            object_id: AudioObjectID,
            address: *const PropertyAddress,
            listener: ListenerProc,
            client_data: *mut c_void,
        ) -> OSStatus;
        fn AudioObjectGetPropertyData(
            object_id: AudioObjectID,
            address: *const PropertyAddress,
            qualifier_data_size: u32,
            qualifier_data: *const c_void,
            data_size: *mut u32,
            data: *mut c_void,
        ) -> OSStatus;
        fn AudioObjectGetPropertyDataSize(
            object_id: AudioObjectID,
            address: *const PropertyAddress,
            qualifier_data_size: u32,
            qualifier_data: *const c_void,
            data_size: *mut u32,
        ) -> OSStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(cf: *const c_void);
    }

    /// HAL property callback. Runs on a CoreAudio thread; it only forwards
    /// a small event value to the watcher thread.
    unsafe extern "C" fn listener_proc(
        _object_id: AudioObjectID,
        address_count: u32,
        addresses: *const PropertyAddress,
        client_data: *mut c_void,
    ) -> OSStatus {
        if client_data.is_null() || addresses.is_null() {
            return NO_ERR;
        }
        // SAFETY: `client_data` is the atomic event set leaked in
        // `OutputDeviceListener::new`; it is never reclaimed, so it
        // outlives every registration that carries it.
        let pending = unsafe { &*(client_data as *const PendingDeviceEvents) };
        for index in 0..address_count {
            // SAFETY: CoreAudio passes `address_count` valid addresses.
            let selector = unsafe { (*addresses.add(index as usize)).selector };
            let event = match selector {
                SELECTOR_NOMINAL_SAMPLE_RATE => DevicePropertyEvent::NominalRate,
                SELECTOR_DEVICES => DevicePropertyEvent::DeviceList,
                SELECTOR_DEFAULT_OUTPUT => DevicePropertyEvent::DefaultOutput,
                _ => continue,
            };
            pending.publish(event);
        }
        NO_ERR
    }

    fn add_listener(object_id: AudioObjectID, selector: u32, client_data: *mut c_void) -> bool {
        let address = PropertyAddress::global(selector);
        // SAFETY: FFI call with a valid address struct and the leaked
        // sender pointer documented in `listener_proc`.
        let status = unsafe {
            AudioObjectAddPropertyListener(object_id, &address, listener_proc, client_data)
        };
        if status != NO_ERR {
            log::warn!(
                "CoreAudio property listener registration failed for object {object_id} selector {selector:#010x}: OSStatus {status}"
            );
            return false;
        }
        true
    }

    fn remove_listener(object_id: AudioObjectID, selector: u32, client_data: *mut c_void) {
        let address = PropertyAddress::global(selector);
        // SAFETY: mirrors a previous successful `add_listener` call.
        let _ = unsafe {
            AudioObjectRemovePropertyListener(object_id, &address, listener_proc, client_data)
        };
    }

    fn get_f64_property(object_id: AudioObjectID, selector: u32) -> Option<f64> {
        let address = PropertyAddress::global(selector);
        let mut value: f64 = 0.0;
        let mut size = size_of::<f64>() as u32;
        // SAFETY: `value` outlives the call and matches the reported size.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object_id,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut value as *mut f64).cast(),
            )
        };
        if status == NO_ERR && size as usize >= size_of::<f64>() {
            Some(value)
        } else {
            None
        }
    }

    fn device_ids() -> Vec<AudioObjectID> {
        let address = PropertyAddress::global(SELECTOR_DEVICES);
        let mut size: u32 = 0;
        // SAFETY: size query with null qualifier per HAL convention.
        let status = unsafe {
            AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &address, 0, std::ptr::null(), &mut size)
        };
        if status != NO_ERR || size == 0 {
            return Vec::new();
        }
        let count = size as usize / size_of::<AudioObjectID>();
        let mut ids = vec![0 as AudioObjectID; count];
        let mut data_size = size;
        // SAFETY: buffer sized from the query above.
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &address,
                0,
                std::ptr::null(),
                &mut data_size,
                ids.as_mut_ptr().cast(),
            )
        };
        if status != NO_ERR {
            return Vec::new();
        }
        ids.truncate(data_size as usize / size_of::<AudioObjectID>());
        ids
    }

    fn device_name(object_id: AudioObjectID) -> Option<String> {
        let address = PropertyAddress::global(SELECTOR_NAME);
        let mut cf_string: *const c_void = std::ptr::null();
        let mut size = size_of::<*const c_void>() as u32;
        // SAFETY: CFStringRef out-parameter; released below after copying.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object_id,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut cf_string as *mut *const c_void).cast(),
            )
        };
        if status != NO_ERR || cf_string.is_null() {
            return None;
        }
        let mut buffer = [0 as c_char; 512];
        // SAFETY: `cf_string` is a valid CFString from the property read.
        let copied = unsafe {
            CFStringGetCString(
                cf_string,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                ENCODING_UTF8,
            )
        };
        // SAFETY: HAL property reads hand the caller a +1 CF object.
        unsafe { CFRelease(cf_string) };
        if copied == 0 {
            return None;
        }
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|&&byte| byte != 0)
            .map(|&byte| byte as u8)
            .collect();
        String::from_utf8(bytes).ok()
    }

    fn device_id_for_name(name: &str) -> Option<AudioObjectID> {
        device_ids()
            .into_iter()
            .find(|&id| device_name(id).as_deref() == Some(name))
    }

    /// CoreAudio property listener for the applied output device.
    ///
    /// Owned by the driver config watcher thread. The atomic event set is
    /// intentionally leaked: CoreAudio may invoke the callback on its own
    /// threads, and a process-lifetime watcher never needs to reclaim it.
    pub(crate) struct OutputDeviceListener {
        pending_ptr: *mut PendingDeviceEvents,
        enabled: bool,
        watched_name: Option<String>,
        watched_id: Option<AudioObjectID>,
    }

    impl OutputDeviceListener {
        pub(crate) fn new() -> Self {
            let pending_ptr = Box::into_raw(Box::new(PendingDeviceEvents::default()));
            let enabled = std::env::var("SOTF_DEVICE_LISTENER")
                .map(|value| value.trim() != "0")
                .unwrap_or(true);
            if enabled {
                let client_data = pending_ptr.cast();
                add_listener(SYSTEM_OBJECT, SELECTOR_DEVICES, client_data);
                add_listener(SYSTEM_OBJECT, SELECTOR_DEFAULT_OUTPUT, client_data);
                log::info!("CoreAudio device property listener registered");
            } else {
                log::info!("CoreAudio device property listener disabled by SOTF_DEVICE_LISTENER=0");
            }
            Self {
                pending_ptr,
                enabled,
                watched_name: None,
                watched_id: None,
            }
        }

        /// Track the nominal sample rate of `name`, re-registering when the
        /// applied device changes. Cheap to call every watcher tick.
        pub(crate) fn watch_output_device(&mut self, name: Option<&str>) {
            if !self.enabled {
                return;
            }
            if name == self.watched_name.as_deref() && self.watched_id.is_some() {
                return;
            }
            if name != self.watched_name.as_deref() {
                self.clear_watch();
                self.watched_name = name.map(str::to_string);
            }
            let Some(name) = name else { return };
            if let Some(id) = device_id_for_name(name)
                && add_listener(id, SELECTOR_NOMINAL_SAMPLE_RATE, self.pending_ptr.cast())
            {
                self.watched_id = Some(id);
            }
        }

        /// Forget the resolved device id (device list changed; ids are not
        /// stable across replugs) so the next watch re-resolves by name.
        pub(crate) fn invalidate_watch(&mut self) {
            self.clear_watch();
        }

        fn clear_watch(&mut self) {
            if let Some(id) = self.watched_id.take() {
                remove_listener(id, SELECTOR_NOMINAL_SAMPLE_RATE, self.pending_ptr.cast());
            }
        }

        pub(crate) fn drain_events(&mut self) -> Vec<DevicePropertyEvent> {
            // SAFETY: this process-lifetime allocation is never reclaimed,
            // and every access to its payload uses atomic operations.
            unsafe { &*self.pending_ptr }.drain()
        }

        pub(crate) fn nominal_sample_rate(&self, name: &str) -> Option<u32> {
            let id = match self.watched_id {
                Some(id) if self.watched_name.as_deref() == Some(name) => Some(id),
                _ => device_id_for_name(name),
            }?;
            let rate = get_f64_property(id, SELECTOR_NOMINAL_SAMPLE_RATE)?;
            if rate.is_finite() && rate > 0.0 {
                Some(rate.round() as u32)
            } else {
                None
            }
        }
    }

    impl Drop for OutputDeviceListener {
        fn drop(&mut self) {
            self.clear_watch();
            if self.enabled {
                remove_listener(SYSTEM_OBJECT, SELECTOR_DEVICES, self.pending_ptr.cast());
                remove_listener(
                    SYSTEM_OBJECT,
                    SELECTOR_DEFAULT_OUTPUT,
                    self.pending_ptr.cast(),
                );
            }
            // `pending_ptr` stays leaked by design (see struct docs).
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DevicePropertyEvent, PendingDeviceEvents, external_rate_request};
    use driver_common::DriverStatus;

    fn status(sample_rate: u32, buffer_frames: u32, channel_count: u32) -> DriverStatus {
        DriverStatus::new(
            true,
            true,
            true,
            sample_rate,
            channel_count,
            buffer_frames,
            "Test Driver",
            true,
            0,
        )
    }

    #[test]
    fn device_event_storm_is_coalesced_and_next_batch_is_preserved() {
        let pending = PendingDeviceEvents::default();
        std::thread::scope(|scope| {
            for event in [
                DevicePropertyEvent::NominalRate,
                DevicePropertyEvent::DeviceList,
                DevicePropertyEvent::DefaultOutput,
            ] {
                let pending = &pending;
                scope.spawn(move || {
                    for _ in 0..100_000 {
                        pending.publish(event);
                    }
                });
            }
        });
        assert_eq!(
            pending.drain(),
            vec![
                DevicePropertyEvent::NominalRate,
                DevicePropertyEvent::DeviceList,
                DevicePropertyEvent::DefaultOutput
            ]
        );
        assert!(pending.drain().is_empty());
        pending.publish(DevicePropertyEvent::DefaultOutput);
        assert_eq!(pending.drain(), vec![DevicePropertyEvent::DefaultOutput]);
    }

    #[test]
    fn external_rate_request_ignores_missing_or_current_rate() {
        let status = status(48_000, 512, 2);
        assert_eq!(external_rate_request(&status, None), None);
        assert_eq!(external_rate_request(&status, Some(0)), None);
        // The feedback guard: our own stream open set this nominal rate.
        assert_eq!(external_rate_request(&status, Some(48_000)), None);
    }

    #[test]
    fn external_rate_request_scales_buffer_to_keep_block_duration() {
        let status = status(48_000, 512, 2);
        let request =
            external_rate_request(&status, Some(96_000)).expect("rate change must reconfigure");
        assert_eq!(request.sample_rate, 96_000);
        assert_eq!(request.buffer_frames, 1024);
        assert_eq!(request.channel_count, 2);
    }

    #[test]
    fn external_rate_request_falls_back_for_unreported_geometry() {
        let status = status(0, 0, 0);
        let request =
            external_rate_request(&status, Some(44_100)).expect("rate change must reconfigure");
        assert_eq!(request.sample_rate, 44_100);
        assert_eq!(request.buffer_frames, 512);
        assert_eq!(request.channel_count, 2);
    }
}
