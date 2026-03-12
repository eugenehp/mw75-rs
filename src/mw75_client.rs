//! BLE scanning, connecting, activation, and the command API for MW75 Neuro.
//!
//! The MW75 uses a two-phase connection model:
//!
//! 1. **BLE activation** — discover the device, connect via BLE, write
//!    activation commands (enable EEG, enable raw mode, query battery),
//!    and verify responses on the status characteristic.
//!
//! 2. **RFCOMM data streaming** — after BLE activation the device starts
//!    broadcasting 63-byte EEG packets on RFCOMM channel 25.
//!
//! With the `rfcomm` Cargo feature enabled, use
//! [`crate::rfcomm::start_rfcomm_stream`] to automatically connect and
//! stream data into the [`Mw75Handle`] after BLE activation.
//!
//! Without the `rfcomm` feature, use [`Mw75Handle::feed_data`] to push
//! raw RFCOMM bytes from any external transport.
//!
//! # Full connection example (with `rfcomm` feature)
//!
//! ```no_run
//! use mw75::prelude::*;
//! use std::sync::Arc;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let client = Mw75Client::new(Mw75ClientConfig::default());
//! let (mut rx, handle) = client.connect().await?;
//! handle.start().await?;
//!
//! // Disconnect BLE before starting RFCOMM
//! let addr = handle.peripheral_id();
//! handle.disconnect_ble().await?;
//!
//! // Start RFCOMM data stream (requires `rfcomm` feature)
//! let handle = Arc::new(handle);
//! # #[cfg(feature = "rfcomm")]
//! let _rfcomm = start_rfcomm_stream(handle.clone(), &addr).await?;
//!
//! while let Some(event) = rx.recv().await {
//!     match event {
//!         Mw75Event::Eeg(pkt) => println!("counter={}", pkt.counter),
//!         Mw75Event::Disconnected => break,
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{anyhow, Result};
use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
#[cfg(target_os = "macos")]
use btleplug::api::CentralState;
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use log::{debug, info, warn};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::parse::PacketProcessor;
use crate::protocol::{
    BATTERY_CMD, BLE_BATTERY_COMMAND, BLE_COMMAND_DELAY_MS, BLE_DISCOVERY_TIMEOUT_SECS,
    BLE_EEG_COMMAND, BLE_RAW_MODE_COMMAND, BLE_SUCCESS_CODE, BLE_UNKNOWN_E0_COMMAND,
    DISABLE_EEG_CMD, DISABLE_RAW_MODE_CMD, ENABLE_EEG_CMD, ENABLE_RAW_MODE_CMD,
    MW75_COMMAND_CHAR, MW75_DEVICE_NAME_PATTERN, MW75_SERVICE_UUID, MW75_STATUS_CHAR,
    BLE_ACTIVATION_DELAY_MS,
};
use crate::types::{ActivationStatus, BatteryInfo, Mw75Event};

// ── Mw75Device ────────────────────────────────────────────────────────────────

/// An MW75 device discovered during a BLE scan.
///
/// Returned by [`Mw75Client::scan_all`]; pass to [`Mw75Client::connect_to`]
/// to establish a connection.
#[derive(Clone, Debug)]
pub struct Mw75Device {
    /// Advertised device name (e.g. `"MW75 Neuro"`).
    pub name: String,
    /// Platform BLE identifier.
    pub id: String,
    pub(crate) peripheral: Peripheral,
    pub(crate) adapter: Adapter,
}

// ── Mw75ClientConfig ──────────────────────────────────────────────────────────

/// Configuration for [`Mw75Client`].
#[derive(Debug, Clone)]
pub struct Mw75ClientConfig {
    /// BLE scan duration in seconds before giving up. Default: `4`.
    pub scan_timeout_secs: u64,
    /// Match devices whose advertised name contains this string
    /// (case-insensitive). Default: `"MW75"`.
    pub name_pattern: String,
}

impl Default for Mw75ClientConfig {
    fn default() -> Self {
        Self {
            scan_timeout_secs: BLE_DISCOVERY_TIMEOUT_SECS,
            name_pattern: MW75_DEVICE_NAME_PATTERN.into(),
        }
    }
}

// ── Mw75Client ────────────────────────────────────────────────────────────────

/// BLE client for MW75 Neuro EEG headphones.
///
/// Handles scanning, connecting, and the BLE activation handshake.
/// After activation, EEG data arrives over RFCOMM; use
/// [`Mw75Handle::feed_data`] to push raw RFCOMM bytes into the built-in
/// packet processor.
///
/// # Architecture
///
/// ```text
///   BLE scan → connect → activate (enable EEG + raw mode)
///                                    ↓
///                        RFCOMM data → feed_data() → Mw75Event::Eeg
/// ```
pub struct Mw75Client {
    config: Mw75ClientConfig,
}

impl Mw75Client {
    /// Create a new client with the given configuration.
    pub fn new(config: Mw75ClientConfig) -> Self {
        Self { config }
    }

    // ── Public: scan ─────────────────────────────────────────────────────────

    /// Scan for **all** nearby MW75 devices and return them.
    ///
    /// The scan runs for `config.scan_timeout_secs` seconds.
    pub async fn scan_all(&self) -> Result<Vec<Mw75Device>> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No Bluetooth adapter found"))?;

        #[cfg(target_os = "macos")]
        wait_for_adapter_ready(&adapter).await;

        info!("scan_all: scanning for {} s …", self.config.scan_timeout_secs);

        // Use service UUID filter — on macOS CoreBluetooth this is required
        // to discover already-paired devices that are not actively advertising.
        let scan_filter = ScanFilter {
            services: vec![MW75_SERVICE_UUID],
        };
        adapter.start_scan(scan_filter).await?;
        tokio::time::sleep(Duration::from_secs(self.config.scan_timeout_secs)).await;
        adapter.stop_scan().await.ok();

        let upper_pattern = self.config.name_pattern.to_uppercase();
        let pattern_bytes = upper_pattern.as_bytes();
        let mut found = vec![];
        for p in adapter.peripherals().await? {
            if let Ok(Some(props)) = p.properties().await {
                let name = props.local_name.clone().unwrap_or_default();
                let id = p.id().to_string();
                debug!("scan_all: saw peripheral: name={name:?}  id={id}");

                let matched = (!name.is_empty()
                    && name.to_uppercase().contains(&upper_pattern))
                    || props.services.contains(&MW75_SERVICE_UUID)
                    || props.manufacturer_data.values().any(|data| {
                        let upper: Vec<u8> =
                            data.iter().map(|b| b.to_ascii_uppercase()).collect();
                        upper
                            .windows(pattern_bytes.len())
                            .any(|w| w == pattern_bytes)
                    });

                if matched {
                    let display_name = if name.is_empty() {
                        "MW75 (matched by UUID/mfg)".to_owned()
                    } else {
                        name
                    };
                    info!("scan_all: found {display_name}  id={id}");
                    found.push(Mw75Device {
                        name: display_name,
                        id,
                        peripheral: p,
                        adapter: adapter.clone(),
                    });
                }
            }
        }
        info!("scan_all: {} device(s) found", found.len());
        Ok(found)
    }

    // ── Public: connect_to ────────────────────────────────────────────────────

    /// Connect to a specific device returned by [`scan_all`], run the BLE
    /// activation handshake, and return an event receiver + handle.
    pub async fn connect_to(
        &self,
        device: Mw75Device,
    ) -> Result<(mpsc::Receiver<Mw75Event>, Mw75Handle)> {
        self.setup_peripheral(device.peripheral, device.name, device.adapter)
            .await
    }

    // ── Public: connect (convenience) ────────────────────────────────────────

    /// Scan for the first MW75 device, connect, and return an event channel.
    ///
    /// Uses a two-phase scan strategy:
    /// 1. Scan with the MW75 service UUID filter (fast, works for paired devices on macOS).
    /// 2. If that fails, retry with a generic scan (catches devices advertising without
    ///    the service UUID in the advertisement payload).
    pub async fn connect(&self) -> Result<(mpsc::Receiver<Mw75Event>, Mw75Handle)> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No Bluetooth adapter found"))?;

        #[cfg(target_os = "macos")]
        wait_for_adapter_ready(&adapter).await;

        let timeout = self.config.scan_timeout_secs;
        let pattern = &self.config.name_pattern;

        // Phase 1: scan with service UUID filter
        info!("Scanning for MW75 devices with service UUID filter (timeout: {timeout} s) …");
        let scan_filter = ScanFilter {
            services: vec![MW75_SERVICE_UUID],
        };
        adapter.start_scan(scan_filter).await?;
        let peripheral = match self.find_first(&adapter, pattern, timeout).await {
            Ok(p) => {
                adapter.stop_scan().await.ok();
                p
            }
            Err(_) => {
                adapter.stop_scan().await.ok();

                // Phase 2: generic scan (no service filter)
                info!("Service-UUID scan found nothing — retrying with generic scan ({timeout} s) …");
                adapter.start_scan(ScanFilter::default()).await?;
                let p = self.find_first(&adapter, pattern, timeout).await?;
                adapter.stop_scan().await.ok();
                p
            }
        };

        let props = peripheral.properties().await?.unwrap_or_default();
        let device_name = props
            .local_name
            .unwrap_or_else(|| format!("MW75 ({})", peripheral.id()));
        info!("Found device: {device_name}");

        self.setup_peripheral(peripheral, device_name, adapter)
            .await
    }

    // ── Private: setup_peripheral ─────────────────────────────────────────────

    async fn setup_peripheral(
        &self,
        peripheral: Peripheral,
        device_name: String,
        adapter: Adapter,
    ) -> Result<(mpsc::Receiver<Mw75Event>, Mw75Handle)> {
        // Connect with timeout
        tokio::time::timeout(Duration::from_secs(10), peripheral.connect())
            .await
            .map_err(|_| anyhow!("BLE connect() timed out after 10 s"))??;

        #[cfg(target_os = "linux")]
        tokio::time::sleep(Duration::from_millis(600)).await;

        tokio::time::timeout(Duration::from_secs(15), peripheral.discover_services())
            .await
            .map_err(|_| anyhow!("discover_services() timed out after 15 s"))??;
        info!("Connected and services discovered: {device_name}");

        let chars: BTreeSet<Characteristic> = peripheral.characteristics();

        let find_char = |uuid: Uuid| -> Result<Characteristic> {
            chars
                .iter()
                .find(|c| c.uuid == uuid)
                .cloned()
                .ok_or_else(|| anyhow!("Characteristic {uuid} not found"))
        };

        // Find required characteristics
        let command_char = find_char(MW75_COMMAND_CHAR)?;
        let status_char = find_char(MW75_STATUS_CHAR)?;

        // Subscribe to status notifications
        peripheral.subscribe(&status_char).await?;
        info!("BLE notifications enabled on status characteristic");

        // Event channel
        let (tx, rx) = mpsc::channel::<Mw75Event>(256);
        let _ = tx.send(Mw75Event::Connected(device_name.clone())).await;

        // Disconnect watcher
        let disconnect_tx = tx.clone();
        let peripheral_id = peripheral.id();
        tokio::spawn(async move {
            match adapter.events().await {
                Ok(mut events) => {
                    while let Some(event) = events.next().await {
                        if let CentralEvent::DeviceDisconnected(id) = event {
                            if id == peripheral_id {
                                info!("Disconnect watcher: MW75 disconnected");
                                let _ = disconnect_tx.send(Mw75Event::Disconnected).await;
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Disconnect watcher: could not subscribe to adapter events: {e}");
                }
            }
        });

        // Spawn notification handler for BLE status responses
        let notification_tx = tx.clone();
        let peripheral_clone = peripheral.clone();
        tokio::spawn(async move {
            let mut notifications = match peripheral_clone.notifications().await {
                Ok(n) => n,
                Err(e) => {
                    warn!("Could not get BLE notifications stream: {e}");
                    return;
                }
            };
            info!("BLE notification stream active, waiting for status responses…");

            while let Some(notif) = notifications.next().await {
                let data = &notif.value;

                if data.len() >= 5 {
                    let cmd_type = data[3];
                    let status = data[4];
                    debug!(
                        "BLE response: cmd=0x{:02x} status=0x{:02x}",
                        cmd_type, status
                    );

                    if cmd_type == BLE_EEG_COMMAND && status == BLE_SUCCESS_CODE {
                        info!("EEG mode confirmed enabled");
                    } else if cmd_type == BLE_RAW_MODE_COMMAND && status == BLE_SUCCESS_CODE {
                        info!("Raw mode confirmed enabled");
                    } else if cmd_type == BLE_BATTERY_COMMAND && status == BLE_SUCCESS_CODE {
                        if data.len() >= 6 {
                            let level = data[5];
                            info!("Battery level: {level}%");
                            let _ = notification_tx
                                .send(Mw75Event::Battery(BatteryInfo { level }))
                                .await;
                        }
                    } else if cmd_type == BLE_SUCCESS_CODE {
                        // Alternative battery response format
                        if data[0] == 0x09 && data[1] == 0x9A && data[2] == 0x03 {
                            let level = status;
                            info!("Battery level: {level}%");
                            let _ = notification_tx
                                .send(Mw75Event::Battery(BatteryInfo { level }))
                                .await;
                        }
                    } else if cmd_type == BLE_UNKNOWN_E0_COMMAND {
                        debug!("Unknown E0 command response: status=0x{status:02x}");
                    } else {
                        warn!(
                            "Unexpected BLE response: cmd=0x{:02x} status=0x{:02x}",
                            cmd_type, status
                        );
                    }
                }
            }

            info!("BLE notification stream ended");
            let _ = notification_tx.send(Mw75Event::Disconnected).await;
        });

        let handle = Mw75Handle {
            peripheral,
            command_char,
            processor: std::sync::Mutex::new(PacketProcessor::new(false)),
            tx,
            device_name: device_name.clone(),
        };

        Ok((rx, handle))
    }

    // ── Private: find_first ───────────────────────────────────────────────────

    /// Discover the first MW75 peripheral using multiple heuristics:
    ///
    /// 1. **Name match** — `local_name` contains the pattern (e.g. "MW75").
    /// 2. **Service UUID match** — advertised services include `MW75_SERVICE_UUID`.
    /// 3. **Manufacturer data match** — mfg data bytes contain the pattern as ASCII.
    ///
    /// On macOS CoreBluetooth, `local_name` is often `None` for already-paired
    /// devices, so (2) and (3) are critical fallbacks.
    async fn find_first(
        &self,
        adapter: &Adapter,
        pattern: &str,
        timeout_secs: u64,
    ) -> Result<Peripheral> {
        let upper_pattern = pattern.to_uppercase();
        let pattern_bytes = upper_pattern.as_bytes();
        let mut logged_peripherals = std::collections::HashSet::new();

        let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            loop {
                let peripherals = adapter.peripherals().await.unwrap_or_default();
                for p in peripherals {
                    if let Ok(Some(props)) = p.properties().await {
                        let id = p.id().to_string();
                        let name = props.local_name.clone().unwrap_or_default();
                        let services = &props.services;
                        let mfg_data = &props.manufacturer_data;

                        // Log each peripheral once for debugging
                        if logged_peripherals.insert(id.clone()) {
                            let svc_summary: String = services
                                .iter()
                                .map(|u| u.to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            let mfg_summary: String = mfg_data
                                .iter()
                                .map(|(k, v)| format!("0x{k:04X}[{}B]", v.len()))
                                .collect::<Vec<_>>()
                                .join(", ");
                            debug!(
                                "scan: id={id}  name={:?}  services=[{svc_summary}]  mfg=[{mfg_summary}]",
                                if name.is_empty() { "<none>" } else { &name }
                            );
                        }

                        // Match 1: name contains pattern
                        if !name.is_empty()
                            && name.to_uppercase().contains(&upper_pattern)
                        {
                            info!("Matched by name: {name}  id={id}");
                            return p;
                        }

                        // Match 2: advertised services include MW75_SERVICE_UUID
                        if services.contains(&MW75_SERVICE_UUID) {
                            info!(
                                "Matched by service UUID: name={:?}  id={id}",
                                if name.is_empty() { "<none>" } else { &name }
                            );
                            return p;
                        }

                        // Match 3: manufacturer data contains pattern as ASCII
                        for (_company_id, data) in mfg_data {
                            let upper_data: Vec<u8> =
                                data.iter().map(|b| b.to_ascii_uppercase()).collect();
                            if upper_data
                                .windows(pattern_bytes.len())
                                .any(|w| w == pattern_bytes)
                            {
                                info!(
                                    "Matched by manufacturer data: name={:?}  id={id}",
                                    if name.is_empty() { "<none>" } else { &name }
                                );
                                return p;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await;

        if result.is_err() {
            warn!(
                "Scan saw {} peripheral(s) total but none matched '{pattern}' \
                 by name, service UUID, or manufacturer data",
                logged_peripherals.len()
            );
        }

        result.map_err(|_| {
            anyhow!("Timed out scanning for an MW75 device after {timeout_secs} s")
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Wait for the macOS CoreBluetooth adapter to reach `PoweredOn` state.
#[cfg(target_os = "macos")]
async fn wait_for_adapter_ready(adapter: &Adapter) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match adapter.adapter_state().await {
            Ok(CentralState::PoweredOn) => {
                info!("macOS: adapter is PoweredOn");
                break;
            }
            Ok(state) => {
                if tokio::time::Instant::now() >= deadline {
                    warn!("macOS: adapter still in state {state:?} after 3 s — proceeding");
                    break;
                }
                debug!("macOS: adapter state = {state:?}, waiting…");
            }
            Err(e) => {
                warn!("macOS: adapter_state() error: {e}");
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
}

// ── Mw75Handle ────────────────────────────────────────────────────────────────

/// A handle to an active MW75 connection for sending commands and feeding data.
pub struct Mw75Handle {
    peripheral: Peripheral,
    command_char: Characteristic,
    processor: std::sync::Mutex<PacketProcessor>,
    tx: mpsc::Sender<Mw75Event>,
    device_name: String,
}

impl Mw75Handle {
    /// Write a raw command to the MW75 command characteristic.
    pub async fn write_command(&self, cmd: &[u8]) -> Result<()> {
        self.peripheral
            .write(&self.command_char, cmd, WriteType::WithResponse)
            .await?;
        Ok(())
    }

    /// Run the full BLE activation sequence: enable EEG → enable raw mode → query battery.
    ///
    /// This mirrors the Python `BLEManager._send_activation_sequence()`.
    ///
    /// After this returns, the MW75 will start streaming EEG packets over
    /// RFCOMM channel 25.
    pub async fn start(&self) -> Result<()> {
        info!("Sending ENABLE_EEG…");
        self.write_command(&ENABLE_EEG_CMD).await?;
        tokio::time::sleep(Duration::from_millis(BLE_ACTIVATION_DELAY_MS)).await;

        info!("Sending ENABLE_RAW_MODE…");
        self.write_command(&ENABLE_RAW_MODE_CMD).await?;
        tokio::time::sleep(Duration::from_millis(BLE_COMMAND_DELAY_MS)).await;

        info!("Getting battery level…");
        self.write_command(&BATTERY_CMD).await?;
        tokio::time::sleep(Duration::from_millis(BLE_COMMAND_DELAY_MS)).await;

        let _ = self
            .tx
            .send(Mw75Event::Activated(ActivationStatus {
                eeg_enabled: true,
                raw_mode_enabled: true,
            }))
            .await;

        info!("BLE activation sequence complete");
        Ok(())
    }

    /// Send the disable sequence to stop EEG streaming.
    ///
    /// Mirrors `BLEManager._send_disable_sequence()`.
    pub async fn stop(&self) -> Result<()> {
        info!("Sending DISABLE_RAW_MODE…");
        self.write_command(&DISABLE_RAW_MODE_CMD).await?;
        tokio::time::sleep(Duration::from_millis(BLE_ACTIVATION_DELAY_MS)).await;

        info!("Sending DISABLE_EEG…");
        self.write_command(&DISABLE_EEG_CMD).await?;
        tokio::time::sleep(Duration::from_millis(BLE_COMMAND_DELAY_MS)).await;

        info!("EEG streaming disabled");
        Ok(())
    }

    /// Feed raw data from the transport (e.g. RFCOMM) into the packet processor.
    ///
    /// Any complete packets found are parsed and dispatched as events on the
    /// `mpsc::Receiver` returned by [`Mw75Client::connect`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn example(handle: &mw75::mw75_client::Mw75Handle) {
    /// // In your RFCOMM read loop:
    /// let data: Vec<u8> = vec![/* raw bytes from socket */];
    /// handle.feed_data(&data).await;
    /// # }
    /// ```
    pub async fn feed_data(&self, data: &[u8]) {
        let events = {
            let mut proc = self.processor.lock().unwrap();
            proc.process_data(data)
        };
        for event in events {
            let _ = self.tx.send(event).await;
        }
    }

    /// Get a snapshot of the current packet processing statistics.
    pub fn get_stats(&self) -> crate::types::ChecksumStats {
        self.processor.lock().unwrap().get_stats()
    }

    /// Check if the BLE peripheral is still connected.
    pub async fn is_connected(&self) -> bool {
        self.peripheral.is_connected().await.unwrap_or(false)
    }

    /// Gracefully disconnect from the MW75.
    ///
    /// Sends the disable command sequence first, then disconnects BLE.
    pub async fn disconnect(&self) -> Result<()> {
        self.stop().await.ok();
        self.peripheral.disconnect().await?;
        Ok(())
    }

    /// Send a [`Mw75Event::Disconnected`] event on the channel.
    ///
    /// Used internally by the RFCOMM reader loop when the transport closes.
    pub async fn send_disconnected(&self) {
        let _ = self.tx.send(Mw75Event::Disconnected).await;
    }

    /// Get the Bluetooth address of the connected peripheral.
    ///
    /// Returns the platform-specific peripheral ID string.
    /// On Linux (BlueZ), this is the MAC address in `"AA:BB:CC:DD:EE:FF"` format.
    /// On macOS, this is a UUID string.
    pub fn peripheral_id(&self) -> String {
        self.peripheral.id().to_string()
    }

    /// Get the advertised device name (e.g. `"MW75 Neuro"`).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Disconnect the BLE link only (keeping the handle alive for RFCOMM).
    ///
    /// On macOS, the BLE connection must be dropped before RFCOMM can connect.
    /// This disconnects BLE without sending disable commands.
    pub async fn disconnect_ble(&self) -> Result<()> {
        info!("Disconnecting BLE (pre-RFCOMM)…");
        self.peripheral.disconnect().await?;
        info!("BLE disconnected");
        Ok(())
    }
}
