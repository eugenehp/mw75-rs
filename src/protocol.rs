//! GATT UUIDs, BLE command sequences, and wire-format constants for MW75 Neuro.
//!
//! All UUIDs belong to the MW75 vendor namespace
//! `000011XX-d102-11e1-9b23-00025b00a5a5`.

use uuid::Uuid;

// ── BLE Service & Characteristics ────────────────────────────────────────────

/// Primary GATT service UUID advertised by MW75 Neuro devices.
///
/// Used as a scan filter to identify MW75 headphones among nearby BLE peripherals.
pub const MW75_SERVICE_UUID: Uuid =
    Uuid::from_u128(0x00001100_d102_11e1_9b23_00025b00a5a5);

/// Command characteristic — the host writes activation commands here.
///
/// Commands are fixed-length byte sequences (typically 5 bytes) that control
/// EEG mode, raw mode, and battery queries.
pub const MW75_COMMAND_CHAR: Uuid =
    Uuid::from_u128(0x00001101_d102_11e1_9b23_00025b00a5a5);

/// Status characteristic — the device sends activation responses here.
///
/// Subscribe to notifications on this characteristic to receive confirmation
/// of command execution (EEG enabled, raw mode enabled, battery level, etc.).
pub const MW75_STATUS_CHAR: Uuid =
    Uuid::from_u128(0x00001102_d102_11e1_9b23_00025b00a5a5);

// ── BLE Command Sequences ────────────────────────────────────────────────────

/// Enable EEG streaming mode on the MW75.
///
/// Wire format: `[0x09, 0x9A, 0x03, 0x60, 0x01]`
pub const ENABLE_EEG_CMD: [u8; 5] = [0x09, 0x9A, 0x03, 0x60, 0x01];

/// Disable EEG streaming mode on the MW75.
///
/// Wire format: `[0x09, 0x9A, 0x03, 0x60, 0x00]`
pub const DISABLE_EEG_CMD: [u8; 5] = [0x09, 0x9A, 0x03, 0x60, 0x00];

/// Enable raw data mode on the MW75.
///
/// Wire format: `[0x09, 0x9A, 0x03, 0x41, 0x01]`
pub const ENABLE_RAW_MODE_CMD: [u8; 5] = [0x09, 0x9A, 0x03, 0x41, 0x01];

/// Disable raw data mode on the MW75.
///
/// Wire format: `[0x09, 0x9A, 0x03, 0x41, 0x00]`
pub const DISABLE_RAW_MODE_CMD: [u8; 5] = [0x09, 0x9A, 0x03, 0x41, 0x00];

/// Query battery level.
///
/// Wire format: `[0x09, 0x9A, 0x03, 0x14, 0xFF]`
pub const BATTERY_CMD: [u8; 5] = [0x09, 0x9A, 0x03, 0x14, 0xFF];

// ── Protocol Constants ───────────────────────────────────────────────────────

/// EEG event ID found in byte[1] of data packets.
pub const EEG_EVENT_ID: u8 = 239;

/// Total size of one MW75 data packet in bytes.
///
/// ```text
/// [sync(1)] [event_id(1)] [data_len(1)] [counter(1)] [ref(4)] [drl(4)]
/// [ch1..ch12(48)] [feature_status(1)] [checksum(2)] = 63 bytes
/// ```
pub const PACKET_SIZE: usize = 63;

/// Sync byte that marks the start of every MW75 data packet.
pub const SYNC_BYTE: u8 = 0xAA;

/// EEG ADC-to-microvolt scaling factor.
///
/// `µV = raw_adc_float × EEG_SCALING_FACTOR`
pub const EEG_SCALING_FACTOR: f32 = 0.023842;

/// Sentinel value indicating an invalid or saturated ADC reading.
pub const SENTINEL_VALUE: i32 = 8388607;

/// Number of EEG channels per packet.
pub const NUM_EEG_CHANNELS: usize = 12;

/// RFCOMM channel number used for data streaming.
pub const RFCOMM_CHANNEL: u8 = 25;

// ── Timing Constants ─────────────────────────────────────────────────────────

/// Delay after sending ENABLE_EEG command (milliseconds).
pub const BLE_ACTIVATION_DELAY_MS: u64 = 100;

/// Delay between BLE commands (milliseconds).
pub const BLE_COMMAND_DELAY_MS: u64 = 500;

/// BLE discovery scan timeout (seconds).
pub const BLE_DISCOVERY_TIMEOUT_SECS: u64 = 4;

/// Seconds without data before declaring the connection lost.
pub const DATA_PACKET_TIMEOUT_SECS: f64 = 8.0;

// ── BLE Response Codes ───────────────────────────────────────────────────────

/// Success response code from the MW75 status characteristic.
pub const BLE_SUCCESS_CODE: u8 = 0xF1;

/// Command type byte for EEG enable/disable responses.
pub const BLE_EEG_COMMAND: u8 = 0x60;

/// Command type byte for raw mode enable/disable responses.
pub const BLE_RAW_MODE_COMMAND: u8 = 0x41;

/// Command type byte for battery query responses.
pub const BLE_BATTERY_COMMAND: u8 = 0x14;

/// Unknown command byte sometimes seen in responses.
pub const BLE_UNKNOWN_E0_COMMAND: u8 = 0xE0;

// ── Device Discovery ─────────────────────────────────────────────────────────

/// Device name pattern used to identify MW75 headphones during BLE scanning.
///
/// Any device whose advertised name contains this substring (case-insensitive)
/// is considered a candidate MW75 device.
pub const MW75_DEVICE_NAME_PATTERN: &str = "MW75";

// ── Human-readable labels ─────────────────────────────────────────────────────

/// EEG channel labels in packet order (Ch1–Ch12).
pub const EEG_CHANNEL_NAMES: [&str; 12] = [
    "Ch1", "Ch2", "Ch3", "Ch4", "Ch5", "Ch6",
    "Ch7", "Ch8", "Ch9", "Ch10", "Ch11", "Ch12",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_uuid_format() {
        let s = MW75_SERVICE_UUID.to_string();
        assert!(s.contains("00001100"), "UUID should contain service base: {s}");
    }

    #[test]
    fn command_char_uuid_format() {
        let s = MW75_COMMAND_CHAR.to_string();
        assert!(s.contains("00001101"));
    }

    #[test]
    fn status_char_uuid_format() {
        let s = MW75_STATUS_CHAR.to_string();
        assert!(s.contains("00001102"));
    }

    #[test]
    fn uuids_are_distinct() {
        assert_ne!(MW75_SERVICE_UUID, MW75_COMMAND_CHAR);
        assert_ne!(MW75_SERVICE_UUID, MW75_STATUS_CHAR);
        assert_ne!(MW75_COMMAND_CHAR, MW75_STATUS_CHAR);
    }

    #[test]
    fn enable_eeg_cmd_structure() {
        assert_eq!(ENABLE_EEG_CMD.len(), 5);
        assert_eq!(ENABLE_EEG_CMD[0], 0x09);
        assert_eq!(ENABLE_EEG_CMD[3], 0x60); // EEG command type
        assert_eq!(ENABLE_EEG_CMD[4], 0x01); // enable
    }

    #[test]
    fn disable_eeg_cmd_structure() {
        assert_eq!(DISABLE_EEG_CMD.len(), 5);
        assert_eq!(DISABLE_EEG_CMD[3], 0x60);
        assert_eq!(DISABLE_EEG_CMD[4], 0x00); // disable
    }

    #[test]
    fn enable_disable_eeg_differ_only_in_last_byte() {
        assert_eq!(ENABLE_EEG_CMD[..4], DISABLE_EEG_CMD[..4]);
        assert_ne!(ENABLE_EEG_CMD[4], DISABLE_EEG_CMD[4]);
    }

    #[test]
    fn enable_raw_mode_cmd_structure() {
        assert_eq!(ENABLE_RAW_MODE_CMD.len(), 5);
        assert_eq!(ENABLE_RAW_MODE_CMD[3], 0x41); // raw mode command type
        assert_eq!(ENABLE_RAW_MODE_CMD[4], 0x01);
    }

    #[test]
    fn disable_raw_mode_cmd_structure() {
        assert_eq!(DISABLE_RAW_MODE_CMD[3], 0x41);
        assert_eq!(DISABLE_RAW_MODE_CMD[4], 0x00);
    }

    #[test]
    fn battery_cmd_structure() {
        assert_eq!(BATTERY_CMD.len(), 5);
        assert_eq!(BATTERY_CMD[3], 0x14); // battery command type
        assert_eq!(BATTERY_CMD[4], 0xFF);
    }

    #[test]
    fn all_commands_share_prefix() {
        // All commands start with [0x09, 0x9A, 0x03]
        for cmd in [
            &ENABLE_EEG_CMD,
            &DISABLE_EEG_CMD,
            &ENABLE_RAW_MODE_CMD,
            &DISABLE_RAW_MODE_CMD,
            &BATTERY_CMD,
        ] {
            assert_eq!(cmd[0], 0x09, "Wrong prefix byte 0");
            assert_eq!(cmd[1], 0x9A, "Wrong prefix byte 1");
            assert_eq!(cmd[2], 0x03, "Wrong prefix byte 2");
        }
    }

    #[test]
    fn protocol_constants() {
        assert_eq!(EEG_EVENT_ID, 239);
        assert_eq!(PACKET_SIZE, 63);
        assert_eq!(SYNC_BYTE, 0xAA);
        assert_eq!(NUM_EEG_CHANNELS, 12);
        assert_eq!(RFCOMM_CHANNEL, 25);
    }

    #[test]
    fn scaling_factor_reasonable() {
        // 0.023842 * 1000 (raw) ≈ 23.84 µV — reasonable EEG amplitude
        let uv = EEG_SCALING_FACTOR * 1000.0;
        assert!(uv > 20.0 && uv < 30.0, "Unexpected µV: {uv}");
    }

    #[test]
    fn channel_names_count() {
        assert_eq!(EEG_CHANNEL_NAMES.len(), NUM_EEG_CHANNELS);
    }

    #[test]
    fn channel_names_format() {
        for (i, name) in EEG_CHANNEL_NAMES.iter().enumerate() {
            let expected = format!("Ch{}", i + 1);
            assert_eq!(*name, expected.as_str());
        }
    }

    #[test]
    fn response_codes_distinct() {
        let codes = [
            BLE_SUCCESS_CODE,
            BLE_EEG_COMMAND,
            BLE_RAW_MODE_COMMAND,
            BLE_BATTERY_COMMAND,
            BLE_UNKNOWN_E0_COMMAND,
        ];
        for i in 0..codes.len() {
            for j in (i + 1)..codes.len() {
                assert_ne!(
                    codes[i], codes[j],
                    "Response codes {} and {} should be distinct",
                    codes[i], codes[j]
                );
            }
        }
    }

    #[test]
    fn sentinel_value() {
        assert_eq!(SENTINEL_VALUE, 8388607);
        assert_eq!(SENTINEL_VALUE, (1 << 23) - 1); // 2^23 - 1
    }
}
