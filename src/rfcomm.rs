//! RFCOMM transport for MW75 Neuro EEG data streaming.
//!
//! After BLE activation, the MW75 headphones stream 63-byte EEG packets at
//! 500 Hz over Bluetooth Classic RFCOMM channel 25.  This module provides
//! the platform-specific RFCOMM socket connection and an async reader loop
//! that feeds data into the packet processor.
//!
//! # Platform support
//!
//! | Platform | Backend | Feature gate |
//! |----------|---------|--------------|
//! | Linux    | BlueZ `AF_BLUETOOTH` RFCOMM socket via [`bluer`] | `rfcomm` |
//! | macOS    | IOBluetooth framework via [`objc2-io-bluetooth`] | `rfcomm` |
//! | Windows  | `Windows.Devices.Bluetooth.Rfcomm` via [`windows`] crate | `rfcomm` |
//!
//! # Architecture
//!
//! ```text
//! BLE activation ──► start_rfcomm_stream(handle, address)
//!                                ↓
//!                    RFCOMM socket connect (channel 25)
//!                                ↓
//!                    async read loop ──► handle.feed_data()
//!                                ↓
//!                    PacketProcessor ──► Mw75Event::Eeg
//! ```
//!
//! # Example
//!
//! ```no_run
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! use mw75::mw75_client::{Mw75Client, Mw75ClientConfig};
//! use mw75::rfcomm::start_rfcomm_stream;
//! use mw75::types::Mw75Event;
//! use std::sync::Arc;
//!
//! let client = Mw75Client::new(Mw75ClientConfig::default());
//! let (mut rx, handle) = client.connect().await?;
//! handle.start().await?;
//!
//! // After BLE activation, start RFCOMM data stream
//! let handle = Arc::new(handle);
//! let rfcomm_task = start_rfcomm_stream(handle.clone(), "AA:BB:CC:DD:EE:FF").await?;
//!
//! while let Some(event) = rx.recv().await {
//!     match event {
//!         Mw75Event::Eeg(pkt) => println!("EEG: counter={}", pkt.counter),
//!         Mw75Event::Disconnected => break,
//!         _ => {}
//!     }
//! }
//!
//! rfcomm_task.abort();
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use log::{debug, error, info};
use tokio::task::JoinHandle;

use crate::mw75_client::Mw75Handle;
use crate::protocol::RFCOMM_CHANNEL;

/// RFCOMM connection timeout in seconds.
#[cfg(target_os = "linux")]
const RFCOMM_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Read buffer size. MW75 packets are 63 bytes; RFCOMM may deliver
/// arbitrary-sized chunks (commonly 64, 128, or up to MTU).
#[cfg(target_os = "linux")]
const READ_BUF_SIZE: usize = 1024;

/// Post-BLE-disconnect settle time in milliseconds.
/// Required on some platforms (especially macOS) for the Bluetooth stack
/// to release the BLE connection before RFCOMM can connect.
const BLE_SETTLE_MS: u64 = 1000;

/// Maximum number of RFCOMM connection attempts on macOS.
/// Each attempt waits progressively longer before retrying.
#[cfg(target_os = "macos")]
const MACOS_RFCOMM_MAX_RETRIES: u32 = 8;

/// Base delay between RFCOMM connection retries on macOS (milliseconds).
/// Multiplied by the attempt number: 500, 1000, 1500, 2000, 2500 ms.
#[cfg(target_os = "macos")]
const MACOS_RFCOMM_RETRY_BASE_MS: u64 = 500;

// ── Public API ────────────────────────────────────────────────────────────────

/// Connect to the MW75 device over RFCOMM and spawn an async reader task
/// that feeds data into the given [`Mw75Handle`].
///
/// The `address` parameter is the Bluetooth MAC address of the MW75 device,
/// formatted as `"AA:BB:CC:DD:EE:FF"`.
///
/// # BLE disconnect requirement
///
/// On macOS (and recommended on Linux), the BLE connection should be
/// disconnected **before** calling this function. The MW75 uses the same
/// Bluetooth radio for BLE and RFCOMM, and keeping BLE open can block
/// RFCOMM delegate callbacks (especially on macOS 26+ "Taho").
///
/// This function includes a short settle delay before connecting.
///
/// # Returns
///
/// A [`JoinHandle`] for the reader task. Abort it to stop streaming.
/// The reader task will also terminate naturally if the RFCOMM connection
/// drops (device powered off, out of range, etc.), in which case it sends
/// [`Mw75Event::Disconnected`](crate::types::Mw75Event::Disconnected).
pub async fn start_rfcomm_stream(
    handle: Arc<Mw75Handle>,
    address: &str,
) -> Result<JoinHandle<()>> {
    let address = address.to_string();
    let device_name = handle.device_name().to_string();

    info!("Starting RFCOMM stream to {address} on channel {RFCOMM_CHANNEL}");

    // Brief settle time for BLE disconnect to complete
    tokio::time::sleep(std::time::Duration::from_millis(BLE_SETTLE_MS)).await;

    let task = tokio::spawn(async move {
        match rfcomm_reader_loop(&handle, &address, &device_name).await {
            Ok(()) => {
                info!("RFCOMM stream ended normally");
            }
            Err(e) => {
                error!("RFCOMM stream error: {e}");
            }
        }
        // Signal disconnection
        handle.send_disconnected().await;
    });

    Ok(task)
}

/// Parse a Bluetooth MAC address string into a 6-byte array.
///
/// Accepts `"AA:BB:CC:DD:EE:FF"` format.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return Err(anyhow!("Invalid MAC address format: {s} (expected AA:BB:CC:DD:EE:FF)"));
    }
    let mut bytes = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        bytes[i] = u8::from_str_radix(part, 16)
            .with_context(|| format!("Invalid hex byte '{part}' in MAC address"))?;
    }
    Ok(bytes)
}

// ═══════════════════════════════════════════════════════════════════════════════
// ── Linux implementation (BlueZ RFCOMM socket via bluer) ──────────────────────
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "linux")]
async fn rfcomm_reader_loop(handle: &Mw75Handle, address: &str, _device_name: &str) -> Result<()> {
    use bluer::rfcomm::{SocketAddr, Stream};
    use bluer::Address;
    use tokio::io::AsyncReadExt;

    let addr: Address = address.parse()
        .with_context(|| format!("Invalid Bluetooth address: {address}"))?;

    let sa = SocketAddr::new(addr, RFCOMM_CHANNEL);

    info!("Linux RFCOMM: connecting to {sa}…");

    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(RFCOMM_CONNECT_TIMEOUT_SECS),
        Stream::connect(sa),
    )
    .await
    .map_err(|_| anyhow!("RFCOMM connect timed out after {RFCOMM_CONNECT_TIMEOUT_SECS} s"))?
    .context("RFCOMM connect failed")?;

    info!("Linux RFCOMM: connected to {address} on channel {RFCOMM_CHANNEL}");

    let mut buf = [0u8; READ_BUF_SIZE];
    let mut total_bytes: u64 = 0;

    loop {
        match stream.read(&mut buf).await {
            Ok(0) => {
                info!("RFCOMM: connection closed by remote (EOF)");
                break;
            }
            Ok(n) => {
                total_bytes += n as u64;
                debug!("RFCOMM: read {n} bytes (total: {total_bytes})");
                handle.feed_data(&buf[..n]).await;
            }
            Err(e) => {
                // Check for expected disconnection errors
                let kind = e.kind();
                if kind == std::io::ErrorKind::ConnectionReset
                    || kind == std::io::ErrorKind::BrokenPipe
                    || kind == std::io::ErrorKind::NotConnected
                {
                    info!("RFCOMM: connection lost ({kind})");
                } else {
                    error!("RFCOMM: read error: {e}");
                }
                break;
            }
        }
    }

    info!("RFCOMM reader loop ended (total bytes: {total_bytes})");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// ── macOS implementation (IOBluetooth RFCOMM via objc2) ────────────────────────
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
async fn rfcomm_reader_loop(handle: &Mw75Handle, address: &str, device_name: &str) -> Result<()> {
    use std::sync::mpsc as std_mpsc;

    // On macOS, CoreBluetooth (btleplug) returns a UUID, not a MAC address.
    // We look up the IOBluetooth device by name instead.
    let name_owned = device_name.to_string();

    // IOBluetooth must run on a thread with an NSRunLoop.
    // We spawn a dedicated thread and communicate via a channel.
    let (data_tx, data_rx) = std_mpsc::channel::<Vec<u8>>();
    let (status_tx, mut status_rx) = tokio::sync::mpsc::channel::<Result<()>>(1);

    let address_owned = address.to_string();

    std::thread::spawn(move || {
        macos_rfcomm_thread_by_name(&name_owned, data_tx, status_tx, address_owned);
    });

    // Wait for connection status
    match status_rx.recv().await {
        Some(Ok(())) => {
            info!("macOS RFCOMM: connected to {address} on channel {RFCOMM_CHANNEL}");
        }
        Some(Err(e)) => {
            return Err(e);
        }
        None => {
            return Err(anyhow!("macOS RFCOMM: connection thread exited unexpectedly"));
        }
    }

    // Read data from the std channel and feed to handle
    let mut total_bytes: u64 = 0;
    loop {
        // Use try_recv in a loop with async sleep to avoid blocking the tokio runtime
        match data_rx.try_recv() {
            Ok(data) => {
                if data.is_empty() {
                    info!("macOS RFCOMM: connection closed signal");
                    break;
                }
                total_bytes += data.len() as u64;
                debug!("macOS RFCOMM: received {} bytes (total: {total_bytes})", data.len());
                handle.feed_data(&data).await;
            }
            Err(std_mpsc::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_micros(100)).await;
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                info!("macOS RFCOMM: data channel closed");
                break;
            }
        }
    }

    info!("macOS RFCOMM reader ended (total bytes: {total_bytes})");
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_rfcomm_thread_by_name(
    device_name: &str,
    data_tx: std::sync::mpsc::Sender<Vec<u8>>,
    status_tx: tokio::sync::mpsc::Sender<Result<()>>,
    address: String,
) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, ClassType};
    use objc2_foundation::{NSArray, NSDate, NSRunLoop, NSString};
    use objc2_io_bluetooth::IOBluetoothDevice;

    // ── Step 1: Find the IOBluetoothDevice by name ────────────────────────────
    //
    // CoreBluetooth (btleplug) gives us a UUID, not a MAC address.
    // IOBluetooth is a separate Classic-BT framework, so we search
    // pairedDevices / recentDevices by name to bridge the two worlds.

    info!("macOS RFCOMM: looking up IOBluetooth device by name '{device_name}' …");

    let device: Option<Retained<IOBluetoothDevice>> = unsafe {
        let mut found: Option<Retained<IOBluetoothDevice>> = None;

        // Search paired devices
        let paired: Option<Retained<NSArray<IOBluetoothDevice>>> =
            msg_send![IOBluetoothDevice::class(), pairedDevices];
        if let Some(ref devices) = paired {
            let count: usize = msg_send![devices, count];
            for i in 0..count {
                let dev: Retained<IOBluetoothDevice> = msg_send![devices, objectAtIndex: i];
                let name_ptr: *const NSString = msg_send![&*dev, name];
                if !name_ptr.is_null() {
                    let name_str = (*name_ptr).to_string();
                    debug!("macOS RFCOMM: paired device: {name_str}");
                    if name_str == device_name {
                        found = Some(dev);
                        break;
                    }
                }
            }
        }

        // Fallback: search recent devices
        if found.is_none() {
            let recent: Option<Retained<NSArray<IOBluetoothDevice>>> =
                msg_send![IOBluetoothDevice::class(), recentDevices: 10usize];
            if let Some(ref devices) = recent {
                let count: usize = msg_send![devices, count];
                for i in 0..count {
                    let dev: Retained<IOBluetoothDevice> = msg_send![devices, objectAtIndex: i];
                    let name_ptr: *const NSString = msg_send![&*dev, name];
                    if !name_ptr.is_null() {
                        let name_str = (*name_ptr).to_string();
                        debug!("macOS RFCOMM: recent device: {name_str}");
                        if name_str == device_name {
                            found = Some(dev);
                            break;
                        }
                    }
                }
            }
        }

        found
    };

    let device = match device {
        Some(d) => d,
        None => {
            // Last resort: try parsing as MAC address
            if let Ok(mac_bytes) = parse_mac(&address) {
                let addr_str = format!(
                    "{:02X}-{:02X}-{:02X}-{:02X}-{:02X}-{:02X}",
                    mac_bytes[0], mac_bytes[1], mac_bytes[2],
                    mac_bytes[3], mac_bytes[4], mac_bytes[5]
                );
                let ns_addr = NSString::from_str(&addr_str);
                let dev: Option<Retained<IOBluetoothDevice>> = unsafe {
                    msg_send![IOBluetoothDevice::class(), deviceWithAddressString: &*ns_addr]
                };
                match dev {
                    Some(d) => d,
                    None => {
                        let _ = status_tx.blocking_send(Err(anyhow!(
                            "macOS: IOBluetoothDevice not found by name '{device_name}' \
                             or address '{address}'"
                        )));
                        return;
                    }
                }
            } else {
                let _ = status_tx.blocking_send(Err(anyhow!(
                    "macOS: IOBluetoothDevice not found by name '{device_name}' \
                     (peripheral ID '{address}' is a CoreBluetooth UUID, not a MAC address)"
                )));
                return;
            }
        }
    };

    // Log device address for diagnostics
    unsafe {
        let addr_ptr: *const NSString = msg_send![&*device, addressString];
        if !addr_ptr.is_null() {
            let addr = (*addr_ptr).to_string();
            info!("macOS RFCOMM: found device '{device_name}' at address {addr}");
        } else {
            info!("macOS RFCOMM: found device '{device_name}' (address unavailable)");
        }
    }

    // ── Step 2: Connection state and SDP ──────────────────────────────────────

    let runloop = NSRunLoop::currentRunLoop();

    let is_connected: bool = unsafe { msg_send![&*device, isConnected] };
    info!("macOS: device isConnected = {is_connected}");

    // Check if Classic BT ACL link exists (vs just BLE)
    // Try explicit openConnection regardless — if already connected it's a no-op
    if !is_connected {
        info!("macOS: opening baseband connection …");
        let r: i32 = unsafe { msg_send![&*device, openConnection] };
        info!("macOS: openConnection returned 0x{r:08x}");
        // Wait with thread::sleep (run loop may have no sources)
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let c: bool = unsafe { msg_send![&*device, isConnected] };
            if c { info!("macOS: baseband connected"); break; }
        }
    }

    // SDP query (results are likely cached for paired devices)
    info!("macOS: performing SDP query …");
    let _: i32 = unsafe { msg_send![&*device, performSDPQuery: std::ptr::null::<AnyObject>()] };
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Enumerate SDP service records to find available channels
    let mut rfcomm_channels: Vec<u8> = Vec::new();
    let mut l2cap_psms: Vec<u16> = Vec::new();
    unsafe {
        let services: *const AnyObject = msg_send![&*device, services];
        if !services.is_null() {
            let count: usize = msg_send![services, count];
            info!("macOS: {count} SDP service record(s):");
            for i in 0..count {
                let rec: *const AnyObject = msg_send![services, objectAtIndex: i];
                let svc_name_ptr: *const NSString = msg_send![rec, getServiceName];
                let svc_name = if !svc_name_ptr.is_null() { (*svc_name_ptr).to_string() } else { "<unnamed>".into() };

                let mut ch: u8 = 0;
                let ch_r: i32 = msg_send![rec, getRFCOMMChannelID: &mut ch as *mut u8];
                let mut psm: u16 = 0;
                let psm_r: i32 = msg_send![rec, getL2CAPPSM: &mut psm as *mut u16];

                let rfcomm_s = if ch_r == 0 { rfcomm_channels.push(ch); format!("RFCOMM={ch}") } else { String::new() };
                let l2cap_s = if psm_r == 0 { l2cap_psms.push(psm); format!("L2CAP={psm}") } else { String::new() };
                info!("macOS:   [{i}] {svc_name:35} {rfcomm_s:12} {l2cap_s}");
            }
        }
    }

    // ── Step 3: Try all transport strategies ──────────────────────────────────
    //
    // Strategy A: RFCOMM on channel 25 (primary)
    // Strategy B: L2CAP PSM 25 (alternative transport for same service)
    // Strategy C: RFCOMM on other discovered channels
    // Strategy D: RFCOMM on channel 2 (GAIA)

    let mut channel_ptr: *mut AnyObject = std::ptr::null_mut();
    let mut transport_type = "";

    // ── Strategy A: RFCOMM channel 25 ─────────────────────────────────────────
    info!("macOS: ── Strategy A: RFCOMM channel {RFCOMM_CHANNEL} ──");
    for attempt in 1..=3u32 {
        channel_ptr = std::ptr::null_mut();
        let r: i32 = unsafe {
            msg_send![
                &*device,
                openRFCOMMChannelSync: &mut channel_ptr,
                withChannelID: RFCOMM_CHANNEL as u8,
                delegate: std::ptr::null::<AnyObject>()
            ]
        };
        if r == 0 && !channel_ptr.is_null() {
            info!("macOS: ✅ RFCOMM channel {RFCOMM_CHANNEL} opened!");
            transport_type = "RFCOMM";
            break;
        }
        let err_name = if r as u32 == 0xe00002bc { "kIOReturnNotPermitted" } else { "?" };
        info!("macOS: RFCOMM ch {RFCOMM_CHANNEL} attempt {attempt}/3: 0x{r:08x} ({err_name})");
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    // ── Strategy B: L2CAP PSM 25 ──────────────────────────────────────────────
    if transport_type.is_empty() {
        info!("macOS: ── Strategy B: L2CAP PSM 25 ──");
        let mut l2cap_ch: *mut AnyObject = std::ptr::null_mut();
        let r: i32 = unsafe {
            msg_send![
                &*device,
                openL2CAPChannelSync: &mut l2cap_ch,
                withPSM: 25u16,
                delegate: std::ptr::null::<AnyObject>()
            ]
        };
        if r == 0 && !l2cap_ch.is_null() {
            info!("macOS: ✅ L2CAP PSM 25 opened!");
            channel_ptr = l2cap_ch;
            transport_type = "L2CAP";
        } else {
            let err_name = if r as u32 == 0xe00002bc { "kIOReturnNotPermitted" } else { "?" };
            info!("macOS: L2CAP PSM 25 failed: 0x{r:08x} ({err_name})");
        }
    }

    // ── Strategy C: Try all other discovered RFCOMM channels ──────────────────
    if transport_type.is_empty() {
        for &ch in &rfcomm_channels {
            if ch == RFCOMM_CHANNEL { continue; }
            info!("macOS: ── Strategy C: RFCOMM channel {ch} ──");
            channel_ptr = std::ptr::null_mut();
            let r: i32 = unsafe {
                msg_send![
                    &*device,
                    openRFCOMMChannelSync: &mut channel_ptr,
                    withChannelID: ch,
                    delegate: std::ptr::null::<AnyObject>()
                ]
            };
            if r == 0 && !channel_ptr.is_null() {
                info!("macOS: ✅ RFCOMM channel {ch} opened!");
                transport_type = "RFCOMM";
                break;
            }
            let err_name = if r as u32 == 0xe00002bc { "kIOReturnNotPermitted" } else { "?" };
            info!("macOS: RFCOMM ch {ch} failed: 0x{r:08x} ({err_name})");
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // ── Strategy D: Try all discovered L2CAP PSMs ─────────────────────────────
    if transport_type.is_empty() {
        for &psm in &l2cap_psms {
            if psm == 25 { continue; }
            info!("macOS: ── Strategy D: L2CAP PSM {psm} ──");
            let mut l2cap_ch: *mut AnyObject = std::ptr::null_mut();
            let r: i32 = unsafe {
                msg_send![
                    &*device,
                    openL2CAPChannelSync: &mut l2cap_ch,
                    withPSM: psm,
                    delegate: std::ptr::null::<AnyObject>()
                ]
            };
            if r == 0 && !l2cap_ch.is_null() {
                info!("macOS: ✅ L2CAP PSM {psm} opened!");
                channel_ptr = l2cap_ch;
                transport_type = "L2CAP";
                break;
            }
            let err_name = if r as u32 == 0xe00002bc { "kIOReturnNotPermitted" } else { "?" };
            info!("macOS: L2CAP PSM {psm} failed: 0x{r:08x} ({err_name})");
        }
    }

    if transport_type.is_empty() {
        let _ = status_tx.blocking_send(Err(anyhow!(
            "macOS: all RFCOMM/L2CAP channel open strategies failed (last=0xe00002bc). \
             RFCOMM channels tried: {rfcomm_channels:?}, L2CAP PSMs tried: {l2cap_psms:?}"
        )));
        return;
    }

    // ── Step 4: Connected — notify caller and pump NSRunLoop ──────────────────

    info!("macOS: {transport_type} channel connected, starting data loop");
    let _ = status_tx.blocking_send(Ok(()));

    // Pump NSRunLoop to receive delegate callbacks.
    // TODO: implement IOBluetoothRFCOMMChannelDelegate / IOBluetoothL2CAPChannelDelegate
    // to actually receive data via `data_tx`.
    loop {
        let date = NSDate::dateWithTimeIntervalSinceNow(0.1);
        runloop.runUntilDate(&date);

        let is_open: bool = unsafe { msg_send![channel_ptr, isOpen] };
        if !is_open {
            info!("macOS: {transport_type} channel closed");
            let _ = data_tx.send(Vec::new());
            break;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ── Windows implementation (Windows.Devices.Bluetooth.Rfcomm) ─────────────────
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
async fn rfcomm_reader_loop(handle: &Mw75Handle, address: &str, _device_name: &str) -> Result<()> {
    use windows::Devices::Bluetooth::Rfcomm::RfcommDeviceService;
    use windows::Devices::Bluetooth::BluetoothDevice;
    use windows::Networking::Sockets::StreamSocket;
    use windows::Storage::Streams::{DataReader, InputStreamOptions};

    let mac_bytes = parse_mac(address)?;

    // Convert MAC to u64 for Windows API (big-endian 6 bytes in low 48 bits)
    let bt_addr: u64 = (mac_bytes[0] as u64) << 40
        | (mac_bytes[1] as u64) << 32
        | (mac_bytes[2] as u64) << 24
        | (mac_bytes[3] as u64) << 16
        | (mac_bytes[4] as u64) << 8
        | (mac_bytes[5] as u64);

    info!("Windows RFCOMM: connecting to {address} (0x{bt_addr:012x})…");

    // Get Bluetooth device
    let device = tokio::task::spawn_blocking(move || -> Result<BluetoothDevice> {
        let op = BluetoothDevice::FromBluetoothAddressAsync(bt_addr)?;
        let device = op.get()?;
        Ok(device)
    })
    .await
    .context("Bluetooth device lookup panicked")?
    .context("Failed to find Bluetooth device")?;

    info!("Windows: found Bluetooth device");

    // Get RFCOMM services
    let rfcomm_services = tokio::task::spawn_blocking(move || -> Result<_> {
        let op = device.GetRfcommServicesAsync()?;
        let result = op.get()?;
        Ok(result)
    })
    .await
    .context("RFCOMM service lookup panicked")?
    .context("Failed to get RFCOMM services")?;

    let services = rfcomm_services.Services()?;
    if services.Size()? == 0 {
        return Err(anyhow!("No RFCOMM services found on device {address}"));
    }

    // Find the Serial Port Profile service or use the first one
    let service = services.GetAt(0)?;
    let host = service.ConnectionHostName()?;
    let service_name = service.ConnectionServiceName()?;

    info!(
        "Windows RFCOMM: connecting to service '{}'",
        service_name.to_string()
    );

    // Connect StreamSocket
    let socket = StreamSocket::new()?;

    tokio::task::spawn_blocking(move || -> Result<()> {
        let op = socket.ConnectAsync(&host, &service_name)?;
        op.get()?;
        Ok(())
    })
    .await
    .context("RFCOMM socket connect panicked")?
    .context("RFCOMM socket connect failed")?;

    info!("Windows RFCOMM: connected to {address}");

    // Read loop
    let input_stream = socket.InputStream()?;
    let reader = DataReader::CreateDataReader(&input_stream)?;
    reader.SetInputStreamOptions(InputStreamOptions::Partial)?;

    let mut total_bytes: u64 = 0;

    loop {
        // Read data
        let result = tokio::task::spawn_blocking({
            let reader = reader.clone();
            move || -> Result<Vec<u8>> {
                let op = reader.LoadAsync(READ_BUF_SIZE as u32)?;
                let bytes_read = op.get()?;
                if bytes_read == 0 {
                    return Ok(Vec::new());
                }
                let mut buf = vec![0u8; bytes_read as usize];
                reader.ReadBytes(&mut buf)?;
                Ok(buf)
            }
        })
        .await;

        match result {
            Ok(Ok(data)) if data.is_empty() => {
                info!("Windows RFCOMM: connection closed (EOF)");
                break;
            }
            Ok(Ok(data)) => {
                total_bytes += data.len() as u64;
                debug!("Windows RFCOMM: read {} bytes (total: {total_bytes})", data.len());
                handle.feed_data(&data).await;
            }
            Ok(Err(e)) => {
                error!("Windows RFCOMM: read error: {e}");
                break;
            }
            Err(e) => {
                error!("Windows RFCOMM: read task panicked: {e}");
                break;
            }
        }
    }

    info!("Windows RFCOMM reader ended (total bytes: {total_bytes})");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// ── Unsupported platforms ─────────────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn rfcomm_reader_loop(_handle: &Mw75Handle, address: &str, _device_name: &str) -> Result<()> {
    Err(anyhow!(
        "RFCOMM is not supported on this platform. \
         Use Mw75Handle::feed_data() to push raw bytes from an external transport."
    ))
}

// ═══════════════════════════════════════════════════════════════════════════════
// ── Tests ─────────────────────────────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mac_valid() {
        let mac = parse_mac("AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(mac, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn parse_mac_lowercase() {
        let mac = parse_mac("aa:bb:cc:dd:ee:ff").unwrap();
        assert_eq!(mac, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn parse_mac_mixed_case() {
        let mac = parse_mac("Aa:Bb:Cc:Dd:Ee:Ff").unwrap();
        assert_eq!(mac, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn parse_mac_zeros() {
        let mac = parse_mac("00:00:00:00:00:00").unwrap();
        assert_eq!(mac, [0; 6]);
    }

    #[test]
    fn parse_mac_invalid_format() {
        assert!(parse_mac("AA:BB:CC:DD:EE").is_err());
        assert!(parse_mac("AA-BB-CC-DD-EE-FF").is_err());
        assert!(parse_mac("AABBCCDDEEFF").is_err());
        assert!(parse_mac("").is_err());
    }

    #[test]
    fn parse_mac_invalid_hex() {
        assert!(parse_mac("GG:BB:CC:DD:EE:FF").is_err());
        assert!(parse_mac("AA:XX:CC:DD:EE:FF").is_err());
    }

    #[test]
    fn rfcomm_channel_is_25() {
        assert_eq!(RFCOMM_CHANNEL, 25);
    }

    #[test]
    fn read_buf_size_adequate() {
        // Must be larger than one MW75 packet (63 bytes)
        assert!(READ_BUF_SIZE >= 63);
        // Should be a reasonable power-of-2-ish size
        assert!(READ_BUF_SIZE >= 512);
    }
}
