use std::io::{self, BufRead};
use std::sync::Arc;

use anyhow::Result;
use log::info;
use tokio::sync::mpsc;

use mw75::mw75_client::{Mw75Client, Mw75ClientConfig};
use mw75::protocol::EEG_CHANNEL_NAMES;
use mw75::types::Mw75Event;

/// Delay before attempting to reconnect after a disconnect.
const RECONNECT_DELAY_SECS: u64 = 3;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────────
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // ── Configuration ─────────────────────────────────────────────────────────
    let config = Mw75ClientConfig {
        scan_timeout_secs: 10,
        name_pattern: "MW75".into(),
    };
    let client = Mw75Client::new(config);

    // ── Stdin command channel (lives across reconnects) ───────────────────────
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if line_tx.send(l.trim().to_owned()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // ── Connect / reconnect loop ──────────────────────────────────────────────
    loop {
        match connect_and_run(&client, &mut line_rx).await {
            Ok(quit) if quit => {
                info!("Quit requested — exiting.");
                break;
            }
            Ok(_) => {
                // Disconnected — try to reconnect after a delay
                info!(
                    "Will attempt to reconnect in {RECONNECT_DELAY_SECS} s … \
                     (press 'q' + Enter to quit)"
                );
                tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
            }
            Err(e) => {
                info!(
                    "Connection failed: {e:#} — retrying in {RECONNECT_DELAY_SECS} s …"
                );
                tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
            }
        }
    }

    Ok(())
}

/// Run a single connect → activate → stream → disconnect cycle.
///
/// Returns `Ok(true)` if the user typed 'q' (quit), `Ok(false)` on
/// device disconnect (caller should reconnect), or `Err` on failure.
async fn connect_and_run(
    client: &Mw75Client,
    line_rx: &mut mpsc::UnboundedReceiver<String>,
) -> Result<bool> {
    info!("Connecting to MW75 headphones …");
    let (mut rx, handle) = client.connect().await?;
    let handle = Arc::new(handle);

    // ── Activation ────────────────────────────────────────────────────────────
    handle.start().await?;
    info!("Activation complete.");

    // ── Data transport ──────────────────────────────────────────────────────
    //
    // The device sends 0x88 heartbeats indicating RFCOMM status.
    // Strategy:
    //   default     — keep BLE connected, attempt RFCOMM in parallel
    //   RFCOMM=0    — BLE-only mode (skip RFCOMM entirely)
    //   RFCOMM=ble  — disconnect BLE first, then RFCOMM (old behavior)
    #[cfg(feature = "rfcomm")]
    let _rfcomm_task = {
        let rfcomm_mode = std::env::var("RFCOMM").unwrap_or_default();
        match rfcomm_mode.as_str() {
            "0" => {
                info!("RFCOMM=0: BLE-only mode, skipping RFCOMM");
                None
            }
            "ble" => {
                // Old behavior: disconnect BLE first
                let bt_address = handle.peripheral_id();
                info!("RFCOMM=ble: disconnecting BLE, then RFCOMM …");
                handle.disconnect_ble().await.ok();
                let rfcomm_handle = handle.clone();
                match mw75::rfcomm::start_rfcomm_stream(rfcomm_handle, &bt_address).await {
                    Ok(task) => { info!("RFCOMM reader task started"); Some(task) }
                    Err(e) => { info!("RFCOMM failed ({e})"); None }
                }
            }
            _ => {
                // Default: keep BLE alive, try RFCOMM in parallel
                let bt_address = handle.peripheral_id();
                info!("Attempting RFCOMM (BLE stays connected) …");
                info!("  (set RFCOMM=0 for BLE-only, RFCOMM=ble for old behavior)");
                let rfcomm_handle = handle.clone();
                match mw75::rfcomm::start_rfcomm_stream(rfcomm_handle, &bt_address).await {
                    Ok(task) => { info!("RFCOMM reader task started"); Some(task) }
                    Err(e) => {
                        info!("RFCOMM failed ({e}), continuing with BLE notifications");
                        None
                    }
                }
            }
        }
    };

    #[cfg(not(feature = "rfcomm"))]
    info!("Streaming EEG data via BLE notifications …");

    info!("Press Ctrl-C or type 'q' + Enter to quit.\n");
    info!("Commands: q = quit, s = stats\n");

    // ── Event loop ────────────────────────────────────────────────────────────
    let mut quit = false;

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(Mw75Event::Connected(name)) => {
                        info!("✅  Connected to: {name}");
                    }
                    Some(Mw75Event::Disconnected) => {
                        info!("❌  Disconnected from device.");
                        break;
                    }
                    Some(Mw75Event::Activated(status)) => {
                        info!(
                            "🔋  Activated: EEG={}, Raw={}",
                            status.eeg_enabled, status.raw_mode_enabled
                        );
                    }
                    Some(Mw75Event::Battery(bat)) => {
                        info!("🔋  Battery: {}%", bat.level);
                    }
                    Some(Mw75Event::Eeg(pkt)) => {
                        let ch_summary: String = pkt
                            .channels
                            .iter()
                            .enumerate()
                            .take(4)
                            .map(|(i, &v)| format!("{}={:+.3}", EEG_CHANNEL_NAMES[i], v))
                            .collect::<Vec<_>>()
                            .join(" ");
                        println!(
                            "[EEG] cnt={:3}  ref={:+.4}  drl={:+.4}  {ch_summary}  … µV",
                            pkt.counter, pkt.ref_value, pkt.drl
                        );
                    }
                    Some(Mw75Event::RawData(data)) => {
                        println!("[RAW] {} bytes", data.len());
                    }
                    Some(Mw75Event::OtherEvent { event_id, counter, raw }) => {
                        println!(
                            "[OTHER] event_id={event_id} counter={counter} len={}",
                            raw.len()
                        );
                    }
                    None => {
                        // Channel closed — all senders dropped
                        info!("Event channel closed.");
                        break;
                    }
                }
            }
            line = line_rx.recv() => {
                match line.as_deref() {
                    Some("q") => {
                        info!("Quit requested.");
                        handle.disconnect().await.ok();
                        quit = true;
                        break;
                    }
                    Some("s") => {
                        let stats = handle.get_stats();
                        info!(
                            "Stats: {} total, {} valid, {} invalid ({:.1}% error rate)",
                            stats.total_packets,
                            stats.valid_packets,
                            stats.invalid_packets,
                            stats.error_rate()
                        );
                    }
                    Some(cmd) if !cmd.is_empty() => {
                        info!("Unknown command: '{cmd}'");
                    }
                    _ => {}
                }
            }
        }
    }

    // Print final stats for this session
    let stats = handle.get_stats();
    if stats.total_packets > 0 {
        info!(
            "Session stats: {} packets, {} valid ({:.1}%), {} invalid ({:.1}%)",
            stats.total_packets,
            stats.valid_packets,
            100.0 - stats.error_rate(),
            stats.invalid_packets,
            stats.error_rate()
        );
    }

    Ok(quit)
}
