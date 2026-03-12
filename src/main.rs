use std::io::{self, BufRead};

use anyhow::Result;
use log::info;

use mw75::mw75_client::{Mw75Client, Mw75ClientConfig};
use mw75::protocol::EEG_CHANNEL_NAMES;
use mw75::types::Mw75Event;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────────
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // ── Configuration ─────────────────────────────────────────────────────────
    let config = Mw75ClientConfig {
        scan_timeout_secs: 10,
        name_pattern: "MW75".into(),
    };

    // ── Connect ───────────────────────────────────────────────────────────────
    let client = Mw75Client::new(config);

    info!("Connecting to MW75 headphones …");
    let (mut rx, handle) = client.connect().await?;

    let handle = std::sync::Arc::new(handle);

    // ── Start activation sequence ─────────────────────────────────────────────
    handle.start().await?;
    info!("Activation complete.");

    // ── Start RFCOMM data stream ──────────────────────────────────────────────
    #[cfg(feature = "rfcomm")]
    {
        let bt_address = handle.peripheral_id();
        info!("Starting RFCOMM stream to {bt_address}…");

        // Disconnect BLE first (required on macOS, recommended on Linux)
        handle.disconnect_ble().await.ok();

        let rfcomm_handle = handle.clone();
        match mw75::rfcomm::start_rfcomm_stream(rfcomm_handle, &bt_address).await {
            Ok(_task) => {
                info!("RFCOMM reader task started");
            }
            Err(e) => {
                info!("RFCOMM connect failed ({e}), falling back to feed_data mode");
            }
        }
    }

    #[cfg(not(feature = "rfcomm"))]
    {
        info!("RFCOMM feature not enabled. Waiting for data via feed_data()…");
    }

    info!("Press Ctrl-C or type 'q' + Enter to quit.\n");
    info!("Commands (type + Enter):");
    info!("  q  – quit");
    info!("  s  – show stats\n");

    // ── Stdin command loop ────────────────────────────────────────────────────
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

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

    let handle_cmd = std::sync::Arc::clone(&handle);
    tokio::spawn(async move {
        while let Some(line) = line_rx.recv().await {
            if line.is_empty() {
                continue;
            }
            match line.as_str() {
                "q" => {
                    info!("Quit requested.");
                    handle_cmd.disconnect().await.ok();
                    std::process::exit(0);
                }
                "s" => {
                    let stats = handle_cmd.get_stats();
                    info!(
                        "Stats: {} total, {} valid, {} invalid ({:.1}% error rate)",
                        stats.total_packets,
                        stats.valid_packets,
                        stats.invalid_packets,
                        stats.error_rate()
                    );
                }
                cmd => {
                    info!("Unknown command: '{cmd}'");
                }
            }
        }
    });

    // ── Main event loop ───────────────────────────────────────────────────────
    while let Some(event) = rx.recv().await {
        match event {
            Mw75Event::Connected(name) => {
                info!("✅  Connected to: {name}");
            }
            Mw75Event::Disconnected => {
                info!("❌  Disconnected from device.");
                break;
            }
            Mw75Event::Activated(status) => {
                info!(
                    "🔋  Activated: EEG={}, Raw={}",
                    status.eeg_enabled, status.raw_mode_enabled
                );
            }
            Mw75Event::Battery(bat) => {
                info!("🔋  Battery: {}%", bat.level);
            }
            Mw75Event::Eeg(pkt) => {
                let ch_summary: String = pkt
                    .channels
                    .iter()
                    .enumerate()
                    .take(4) // Show first 4 channels for brevity
                    .map(|(i, &v)| format!("{}={:+.3}", EEG_CHANNEL_NAMES[i], v))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!(
                    "[EEG] cnt={:3}  ref={:+.4}  drl={:+.4}  {ch_summary}  … µV",
                    pkt.counter, pkt.ref_value, pkt.drl
                );
            }
            Mw75Event::RawData(data) => {
                println!("[RAW] {} bytes", data.len());
            }
            Mw75Event::OtherEvent {
                event_id,
                counter,
                raw,
            } => {
                println!(
                    "[OTHER] event_id={event_id} counter={counter} len={}",
                    raw.len()
                );
            }
        }
    }

    // Print final stats
    let stats = handle.get_stats();
    if stats.total_packets > 0 {
        info!(
            "Final Stats: {} packets, {} valid ({:.1}%), {} invalid ({:.1}%)",
            stats.total_packets,
            stats.valid_packets,
            100.0 - stats.error_rate(),
            stats.invalid_packets,
            stats.error_rate()
        );
    }

    info!("Event loop finished – exiting.");
    Ok(())
}
