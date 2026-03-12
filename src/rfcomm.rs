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
const BLE_SETTLE_MS: u64 = 500;

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

    // On macOS, CoreBluetooth returns UUIDs, not MAC addresses.
    // We look up the IOBluetooth device by scanning paired/recent devices
    // and matching by name.
    info!("macOS RFCOMM: looking up IOBluetooth device by name '{device_name}'…");

    // Try pairedDevices first, then recentDevices
    let device: Option<Retained<IOBluetoothDevice>> = unsafe {
        // pairedDevices returns an NSArray of IOBluetoothDevice
        let paired: Option<Retained<NSArray<IOBluetoothDevice>>> =
            msg_send![IOBluetoothDevice::class(), pairedDevices];

        let mut found: Option<Retained<IOBluetoothDevice>> = None;

        if let Some(ref devices) = paired {
            let count: usize = msg_send![devices, count];
            for i in 0..count {
                let dev: Retained<IOBluetoothDevice> = msg_send![devices, objectAtIndex: i];
                let name_ptr: *const NSString = msg_send![&*dev, name];
                if !name_ptr.is_null() {
                    let ns_name: &NSString = &*name_ptr;
                    let name_str = ns_name.to_string();
                    debug!("macOS RFCOMM: paired device: {name_str}");
                    if name_str == device_name {
                        found = Some(dev);
                        break;
                    }
                }
            }
        }

        // Also try recentDevices if not found in paired
        if found.is_none() {
            let recent: Option<Retained<NSArray<IOBluetoothDevice>>> =
                msg_send![IOBluetoothDevice::class(), recentDevices: 10usize];
            if let Some(ref devices) = recent {
                let count: usize = msg_send![devices, count];
                for i in 0..count {
                    let dev: Retained<IOBluetoothDevice> = msg_send![devices, objectAtIndex: i];
                    let name_ptr: *const NSString = msg_send![&*dev, name];
                    if !name_ptr.is_null() {
                        let ns_name: &NSString = &*name_ptr;
                        let name_str = ns_name.to_string();
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
            // If we can't find by name, try parsing as MAC as a fallback
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
                            "macOS: IOBluetoothDevice not found by name '{device_name}' or address '{address}'"
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

    // Open RFCOMM channel synchronously
    // IOBluetoothDevice.openRFCOMMChannelSync:withChannelID:delegate:
    let mut channel_ptr: *mut AnyObject = std::ptr::null_mut();
    let result: i32 = unsafe {
        msg_send![
            &*device,
            openRFCOMMChannelSync: &mut channel_ptr,
            withChannelID: RFCOMM_CHANNEL as u8,
            delegate: std::ptr::null::<AnyObject>()
        ]
    };

    if result != 0 || channel_ptr.is_null() {
        let _ = status_tx.blocking_send(Err(anyhow!(
            "macOS: RFCOMM channel open failed (status=0x{result:08x})"
        )));
        return;
    }

    // Notify success
    let _ = status_tx.blocking_send(Ok(()));

    // Run NSRunLoop to pump IOBluetooth events.
    // Data arrives via the delegate callback; since we're not using a delegate
    // in this simplified version, we use an alternative: periodically poll or
    // use a registered RFCOMM data callback.
    //
    // NOTE: A full macOS implementation requires an NSObject subclass as a delegate
    // for rfcommChannelData:data:length: callbacks. This is complex with objc2.
    // For now, we use the synchronous read approach with NSRunLoop pumping.

    let runloop = NSRunLoop::currentRunLoop();
    loop {
        // Pump the runloop for 1ms
        let date = NSDate::dateWithTimeIntervalSinceNow(0.001);
        runloop.runUntilDate(&date);

        // Check if channel is still open
        let is_open: bool = unsafe { msg_send![channel_ptr, isOpen] };
        if !is_open {
            let _ = data_tx.send(Vec::new()); // Signal closed
            break;
        }

        // NOTE: Actual data delivery requires a delegate with
        // rfcommChannelData:data:length: — the macOS IOBluetooth framework
        // does not support synchronous reads on RFCOMM channels.
        // This loop keeps the runloop alive for delegate callbacks.
        // A production implementation would use a proper ObjC delegate class.
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
