//! Command-line frontend for `sennheiser-core`: reads and adjusts the volume
//! of a connected Bluetooth headset via BlueZ's AVRCP absolute volume
//! support, and controls ANC/EQ/etc via the Sennheiser/Sonova GAIA protocol.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sennheiser_core::{battery_ble, bluez, commands, gaia, transport};

#[derive(Parser)]
#[command(
    name = "sennheiser-controls",
    about = "Read/adjust the volume of a connected Bluetooth headset via BlueZ AVRCP"
)]
struct Cli {
    /// Match a connected device by name/alias or MAC address substring (case-insensitive).
    /// If omitted, the first connected device with an active audio transport is used.
    #[arg(short, long, global = true)]
    device: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List every connected Bluetooth audio device and its current volume
    Status,
    /// Print the current volume (0-127) of the target device
    Get,
    /// Set the volume to an exact value (0-127)
    Set { value: u16 },
    /// Increase the volume by --step (default 8)
    Up {
        #[arg(long, default_value_t = 8)]
        step: u16,
    },
    /// Decrease the volume by --step (default 8)
    Down {
        #[arg(long, default_value_t = 8)]
        step: u16,
    },
    /// Print the battery level of a connected device (standard Bluetooth
    /// GATT Battery Service - no GAIA/proprietary protocol involved).
    Battery {
        /// bluez: read org.bluez.Battery1 via D-Bus (Linux-only, what BlueZ
        /// itself already computed). btleplug: read the standard GATT
        /// Battery Level characteristic (0x2A19) directly - the
        /// cross-platform path that would also work on Windows/macOS.
        #[arg(long, value_enum, default_value = "bluez")]
        backend: BatteryBackend,
    },
    /// Control Active Noise Cancellation (Sennheiser GAIA protocol - EXPERIMENTAL,
    /// see the README: opcodes were confirmed on Sennheiser CX400/TW2 firmware
    /// only, and may not work on other hardware).
    Anc {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: AncAction,
    },
    /// Control the transparency / ambient-sound mode (Sennheiser GAIA protocol -
    /// EXPERIMENTAL, same caveats as `anc`).
    Transparency {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: TransparencyAction,
    },
    /// Turn Bass Boost on or off (VERIFIED against real HDB 630 hardware).
    BassBoost {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: BassBoostAction,
    },
    /// Set the 5-band EQ, either via a named preset or a single raw band
    /// (VERIFIED against real HDB 630 hardware, except the "speech-clarity"
    /// preset, which is an unverified/fabricated placeholder - see
    /// gaia::EQ_PRESETS docs).
    Eq {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: EqAction,
    },
    /// Set the "Noise control: Custom" ANC/Transparency crossfade slider
    /// (VERIFIED against real HDB 630 hardware). 0 = full ANC, 100 = full
    /// Transparency, 50 = neutral/off.
    NoiseControlCustom {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        /// Percentage, 0 (full ANC) to 100 (full Transparency)
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
    },
    /// Turn multipoint (connecting to 2 devices at once) on or off, or check
    /// its status (VERIFIED against real HDB 630 hardware). This only
    /// reports how many hosts are currently connected, not their identities -
    /// see `paired-devices` for the headset's own named device list.
    Multipoint {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: MultipointAction,
    },
    /// List every Bluetooth device currently connected to THIS machine
    /// (standard BlueZ Device1.Connected - not a GAIA/headphone-specific
    /// query). See also `paired-devices`, which asks the headset itself.
    BluezDevices,
    /// List devices paired to the headset itself, by name (VERIFIED against
    /// real HDB 630 hardware, but UNVERIFIED opcodes 0x1400/0x1401 - sourced
    /// from a third-party protocol writeup with no decode detail, decoded
    /// here by direct experimentation; see `gaia::CMD_SONOVA_PAIRED_DEVICE_GET`
    /// docs for caveats, particularly around the "connected" flag's meaning).
    PairedDevices {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
    },
    /// Turn Anti-wind (wind noise reduction in ANC) on or off (VERIFIED
    /// against real HDB 630 hardware).
    AntiWind {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: AntiWindAction,
    },
    /// Set the Crossfeed level (VERIFIED against real HDB 630 hardware).
    Crossfeed {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: CrossfeedAction,
    },
    /// Read device info: battery %, codec, model, firmware version, HW
    /// revision, serial (UNVERIFIED opcodes - sourced from a third-party
    /// protocol writeup, not this project's own captures; see
    /// `gaia::CMD_SONOVA_BATTERY_GET` docs and neighbors).
    DeviceInfo {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
    },
    /// Sidetone (hear your own voice during calls) level 0-5 (UNVERIFIED opcode).
    Sidetone {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: LevelAction,
    },
    /// Smart Pause: auto-pause playback when the headset is removed (UNVERIFIED opcode).
    SmartPause {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: ToggleAction,
    },
    /// On-Head Detection (UNVERIFIED opcode).
    OnHeadDetection {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: ToggleAction,
    },
    /// Auto-Answer incoming calls (UNVERIFIED opcode).
    AutoAnswer {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: ToggleAction,
    },
    /// "Comfort Call" (UNVERIFIED opcode, no further detail known about what it changes).
    ComfortCall {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: ToggleAction,
    },
    /// Low Latency (gaming) mode (UNVERIFIED opcode).
    LowLatency {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: ToggleAction,
    },
    /// "BT Compatibility" mode (UNVERIFIED opcode, no further detail known).
    BtCompatibility {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: ToggleAction,
    },
    /// Audio/Podcast mode (UNVERIFIED opcode; known value: 2 = podcast).
    AudioMode {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: AudioModeAction,
    },
    /// Voice/tone prompts (UNVERIFIED opcode).
    VoicePrompt {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: ToggleAction,
    },
    /// Voice prompt language, by index (UNVERIFIED opcode, SET only - see
    /// `gaia::CMD_SONOVA_PROMPT_LANGUAGE_SET` docs).
    PromptLanguage {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        index: u8,
    },
    /// Auto Power-Off timer, in seconds (UNVERIFIED opcode AND unit - see
    /// `gaia::CMD_SONOVA_AUTO_POWER_OFF_GET` docs).
    AutoPowerOff {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: AutoPowerOffAction,
    },
    /// Connects, registers for live-push notifications (including the
    /// UNTESTED `deviceManagement` category - see
    /// `gaia::CATEGORY_SONOVA_DEVICE_MANAGEMENT`), then prints every raw
    /// frame the headset sends for a window of time. For reverse-engineering
    /// what pushes unprompted (e.g. do something to the device - like
    /// disconnecting a paired phone - while this runs).
    Listen {
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        /// How long to listen before exiting
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
    /// Send an arbitrary raw GAIA command over the control channel
    /// (advanced / for experimenting with opcodes this tool doesn't know about).
    Gaia {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        /// GAIA vendor ID, e.g. 0x0494 for Sennheiser
        #[arg(long, default_value = "0x0494")]
        vendor: String,
        /// GAIA command ID, e.g. 0x0708
        #[arg(long)]
        command: String,
        /// Payload bytes as hex, e.g. "0001" (default: no payload)
        #[arg(long, default_value = "")]
        payload: String,
        /// GAIA transport protocol version (1 for GAIA, 3 for GAIA3); ignored for --transport ble
        #[arg(long, default_value_t = 1)]
        version: u8,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum BatteryBackend {
    Bluez,
    Btleplug,
}

#[derive(Subcommand)]
enum AncAction {
    /// Turn ANC off (VERIFIED against real HDB 630 hardware)
    Off,
    /// Turn on Adaptive ANC (VERIFIED against real HDB 630 hardware)
    Adaptive,
    /// Read the current ANC state from the device (UNVERIFIED - no confirmed
    /// "get" opcode was observed in the capture; this uses the older,
    /// unverified Sennheiser-app opcode as a guess)
    Status,
}

#[derive(Subcommand)]
enum BassBoostAction {
    /// Turn Bass Boost on
    On,
    /// Turn Bass Boost off
    Off,
}

#[derive(Subcommand)]
enum MultipointAction {
    /// Turn multipoint on
    On,
    /// Turn multipoint off
    Off,
    /// Query the multipoint status (reports a count: 2 = on, 1 = off - not
    /// device identities, see the `multipoint` command's help)
    Status,
}

#[derive(Subcommand)]
enum AntiWindAction {
    /// Turn Anti-wind on
    On,
    /// Turn Anti-wind off
    Off,
}

#[derive(Subcommand)]
enum CrossfeedAction {
    /// Turn crossfeed off
    Off,
    /// Set crossfeed to low
    Low,
    /// Set crossfeed to high
    High,
}

/// Shared by every simple SET `[0/1]` / GET boolean setting added from the
/// third-party protocol writeup (Smart Pause, On-Head Detection,
/// Auto-Answer, Comfort Call, Low Latency, BT Compatibility, Voice Prompt).
#[derive(Subcommand)]
enum ToggleAction {
    On,
    Off,
    Status,
}

#[derive(Subcommand)]
enum LevelAction {
    /// Set the level directly (e.g. Sidetone, 0-5)
    Set { level: u8 },
    Status,
}

#[derive(Subcommand)]
enum AudioModeAction {
    /// Set the raw mode byte (known value: 2 = podcast)
    Set { mode: u8 },
    Status,
}

#[derive(Subcommand)]
enum AutoPowerOffAction {
    /// Set the timer, in seconds (unit UNCONFIRMED - see
    /// `gaia::CMD_SONOVA_AUTO_POWER_OFF_GET` docs)
    Set { seconds: u16 },
    Status,
}

#[derive(Subcommand)]
enum EqAction {
    /// Apply a named preset: neutral, speech-clarity, rock, pop, dance,
    /// hip-hop, classical, movie (sends all 5 bands in sequence)
    Preset { name: String },
    /// Set a single band's gain directly (advanced / build your own curve)
    Band {
        /// Band index, 0-4
        #[arg(value_parser = clap::value_parser!(u8).range(0..=4))]
        band: u8,
        /// Signed gain value, e.g. -20 or 25
        gain: i8,
    },
    /// Read the current 5-band gain curve from the device (UNVERIFIED opcode
    /// 0x1002 - sourced from a third-party protocol writeup, not this
    /// project's own captures; see `gaia::CMD_SONOVA_EQ_GET_BAND` docs)
    Status,
}

#[derive(Subcommand)]
enum TransparencyAction {
    /// Turn transparency/ambient-sound mode on
    On,
    /// Turn transparency/ambient-sound mode off
    Off,
    /// Read the current transparency mode from the device
    Status,
}

fn print_volume(name: &str, volume: u16) {
    let pct = volume as u32 * 100 / bluez::MAX_VOLUME as u32;
    println!("{name}: {volume}/{} ({pct}%)", bluez::MAX_VOLUME);
}

fn print_toggle_status(name: &str, label: &str, value: Option<bool>) {
    match value {
        Some(v) => println!("{name}: {label} = {}", if v { "on" } else { "off" }),
        None => println!("{name}: {label} reply had no value"),
    }
}

fn print_gaia_response(name: &str, resp: &gaia::GaiaResponse) {
    let status = match resp.status {
        Some(0) => "OK".to_string(),
        Some(code) => format!("error/unknown code {code}"),
        None => "no status byte".to_string(),
    };
    println!(
        "{name}: response vendor=0x{:04x} command=0x{:04x} status={status} payload={:02x?}",
        resp.vendor_id, resp.command_id, resp.payload
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = bluez::system_bus().await?;

    match cli.command {
        Command::Status => {
            let candidates = bluez::list_candidates(&conn).await?;
            if candidates.is_empty() {
                println!("No connected device with an active AVRCP audio transport found.");
                return Ok(());
            }
            for c in candidates {
                match bluez::get_volume(&conn, &c.transport_path).await {
                    Ok(v) => print_volume(&c.display_name, v),
                    Err(e) => println!("{}: volume unavailable ({e})", c.display_name),
                }
            }
        }
        Command::Get => {
            let target = bluez::find_target(&conn, cli.device.as_deref()).await?;
            let v = bluez::get_volume(&conn, &target.transport_path).await?;
            print_volume(&target.display_name, v);
        }
        Command::Set { value } => {
            let target = bluez::find_target(&conn, cli.device.as_deref()).await?;
            bluez::set_volume(&conn, &target.transport_path, value).await?;
            let v = bluez::get_volume(&conn, &target.transport_path).await?;
            print_volume(&target.display_name, v);
        }
        Command::Up { step } => {
            let target = bluez::find_target(&conn, cli.device.as_deref()).await?;
            let current = bluez::get_volume(&conn, &target.transport_path).await?;
            let new = current.saturating_add(step).min(bluez::MAX_VOLUME);
            bluez::set_volume(&conn, &target.transport_path, new).await?;
            print_volume(&target.display_name, new);
        }
        Command::Down { step } => {
            let target = bluez::find_target(&conn, cli.device.as_deref()).await?;
            let current = bluez::get_volume(&conn, &target.transport_path).await?;
            let new = current.saturating_sub(step);
            bluez::set_volume(&conn, &target.transport_path, new).await?;
            print_volume(&target.display_name, new);
        }
        Command::Battery { backend } => match backend {
            BatteryBackend::Bluez => {
                let device = bluez::find_connected_device(&conn, cli.device.as_deref()).await?;
                let pct = bluez::get_battery(&conn, &device.path).await?;
                println!("{}: battery {pct}% (via org.bluez.Battery1)", device.name);
            }
            BatteryBackend::Btleplug => {
                let (name, pct) = battery_ble::read_battery_percentage(cli.device.as_deref()).await?;
                println!("{name}: battery {pct}% (via btleplug GATT read)");
            }
        },
        Command::Anc { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                AncAction::Status => {
                    let resp = commands::anc_status(&mut gaia_conn).await?;
                    println!("(unverified opcode for this device - interpret with caution)");
                    print_gaia_response(&name, &resp);
                }
                AncAction::Off | AncAction::Adaptive => {
                    let adaptive = matches!(action, AncAction::Adaptive);
                    let (primary, companion) = commands::set_anc(&mut gaia_conn, adaptive).await?;
                    print_gaia_response(&format!("{name} (primary)"), &primary);
                    print_gaia_response(&format!("{name} (companion)"), &companion);
                }
            }
        }
        Command::Transparency { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            let resp = match action {
                TransparencyAction::Status => commands::transparency_status(&mut gaia_conn).await?,
                TransparencyAction::On => commands::set_transparency(&mut gaia_conn, true).await?,
                TransparencyAction::Off => commands::set_transparency(&mut gaia_conn, false).await?,
            };
            print_gaia_response(&name, &resp);
        }
        Command::BassBoost { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            let resp = commands::set_bass_boost(&mut gaia_conn, matches!(action, BassBoostAction::On)).await?;
            print_gaia_response(&name, &resp);
        }
        Command::Eq { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                EqAction::Preset { name: preset_name } => {
                    let results = commands::set_eq_preset(&mut gaia_conn, &preset_name).await?;
                    for (band, gain, resp) in results {
                        print_gaia_response(&format!("{name} (band {band} -> {gain})"), &resp);
                    }
                }
                EqAction::Band { band, gain } => {
                    let resp = commands::set_eq_band(&mut gaia_conn, band, gain).await?;
                    print_gaia_response(&name, &resp);
                }
                EqAction::Status => {
                    let gains = commands::query_eq_bands(&mut gaia_conn).await?;
                    println!("{name}: eq bands = {gains:?}");
                    match commands::find_eq_preset(gains) {
                        Some((_, preset)) => println!("{name}: matches preset '{preset}'"),
                        None => println!("{name}: does not match any known preset"),
                    }
                }
            }
        }
        Command::NoiseControlCustom { transport, percent } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            let (set_resp, commit_resp) = commands::set_noise_control_custom(&mut gaia_conn, percent).await?;
            print_gaia_response(&format!("{name} (set)"), &set_resp);
            print_gaia_response(&format!("{name} (commit)"), &commit_resp);
        }
        Command::Multipoint { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                MultipointAction::On | MultipointAction::Off => {
                    let resp = commands::set_multipoint(&mut gaia_conn, matches!(action, MultipointAction::On)).await?;
                    print_gaia_response(&name, &resp);
                }
                MultipointAction::Status => match commands::multipoint_status(&mut gaia_conn).await? {
                    commands::MultipointStatus::On => println!("{name}: multipoint is ON (2 devices)"),
                    commands::MultipointStatus::Off => println!("{name}: multipoint is OFF (1 device)"),
                    commands::MultipointStatus::Unknown(other) => {
                        println!("{name}: multipoint status returned unexpected value {other}")
                    }
                    commands::MultipointStatus::NoValue => println!("{name}: multipoint status response had no value"),
                },
            }
        }
        Command::BluezDevices => {
            let devices = bluez::list_all_connected_devices(&conn).await?;
            if devices.is_empty() {
                println!("No devices currently connected to this machine.");
            } else {
                for d in devices {
                    println!("{} ({})", d.name, d.address);
                }
            }
        }
        Command::PairedDevices { transport } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            let devices = commands::paired_devices(&mut gaia_conn).await?;
            if devices.is_empty() {
                println!("{name}: no paired devices reported");
            } else {
                for d in devices {
                    println!("{name}: [{}] {} (connected: {})", d.index, d.name, d.connected);
                }
            }
        }
        Command::AntiWind { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            let resp = commands::set_anti_wind(&mut gaia_conn, matches!(action, AntiWindAction::On)).await?;
            print_gaia_response(&name, &resp);
        }
        Command::Crossfeed { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            let level = match action {
                CrossfeedAction::Off => commands::CrossfeedLevel::Off,
                CrossfeedAction::Low => commands::CrossfeedLevel::Low,
                CrossfeedAction::High => commands::CrossfeedLevel::High,
            };
            let resp = commands::set_crossfeed(&mut gaia_conn, level).await?;
            print_gaia_response(&name, &resp);
        }
        Command::DeviceInfo { transport } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match commands::battery_percent(&mut gaia_conn).await {
                Ok(Some(pct)) => println!("{name}: battery = {pct}%"),
                Ok(None) => println!("{name}: battery reply had no value"),
                Err(e) => println!("{name}: battery error: {e:#}"),
            }
            match commands::codec_in_use(&mut gaia_conn).await {
                Ok(Some(codec)) => println!("{name}: codec = {} ({codec})", commands::codec_name(codec)),
                Ok(None) => println!("{name}: codec reply had no value"),
                Err(e) => println!("{name}: codec error: {e:#}"),
            }
            match commands::model_id(&mut gaia_conn).await {
                Ok(model) => println!("{name}: model = {model:?}"),
                Err(e) => println!("{name}: model error: {e:#}"),
            }
            match commands::firmware_version(&mut gaia_conn).await {
                Ok(fw) => println!("{name}: firmware = {fw}"),
                Err(e) => println!("{name}: firmware error: {e:#}"),
            }
            match commands::hw_revision(&mut gaia_conn).await {
                Ok(rev) => println!("{name}: hw revision = {rev}"),
                Err(e) => println!("{name}: hw revision error: {e:#}"),
            }
            match commands::serial_number(&mut gaia_conn).await {
                Ok(serial) => println!("{name}: serial = {serial:?}"),
                Err(e) => println!("{name}: serial error: {e:#}"),
            }
        }
        Command::Sidetone { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                LevelAction::Set { level } => {
                    let resp = commands::set_sidetone(&mut gaia_conn, level).await?;
                    print_gaia_response(&name, &resp);
                }
                LevelAction::Status => match commands::sidetone_level(&mut gaia_conn).await? {
                    Some(level) => println!("{name}: sidetone level = {level}"),
                    None => println!("{name}: sidetone reply had no value"),
                },
            }
        }
        Command::SmartPause { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                ToggleAction::Status => print_toggle_status(&name, "smart pause", commands::smart_pause_status(&mut gaia_conn).await?),
                on_off => {
                    let resp = commands::set_smart_pause(&mut gaia_conn, matches!(on_off, ToggleAction::On)).await?;
                    print_gaia_response(&name, &resp);
                }
            }
        }
        Command::OnHeadDetection { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                ToggleAction::Status => {
                    print_toggle_status(&name, "on-head detection", commands::on_head_detection_status(&mut gaia_conn).await?)
                }
                on_off => {
                    let resp = commands::set_on_head_detection(&mut gaia_conn, matches!(on_off, ToggleAction::On)).await?;
                    print_gaia_response(&name, &resp);
                }
            }
        }
        Command::AutoAnswer { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                ToggleAction::Status => print_toggle_status(&name, "auto-answer", commands::auto_answer_status(&mut gaia_conn).await?),
                on_off => {
                    let resp = commands::set_auto_answer(&mut gaia_conn, matches!(on_off, ToggleAction::On)).await?;
                    print_gaia_response(&name, &resp);
                }
            }
        }
        Command::ComfortCall { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                ToggleAction::Status => print_toggle_status(&name, "comfort call", commands::comfort_call_status(&mut gaia_conn).await?),
                on_off => {
                    let resp = commands::set_comfort_call(&mut gaia_conn, matches!(on_off, ToggleAction::On)).await?;
                    print_gaia_response(&name, &resp);
                }
            }
        }
        Command::LowLatency { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                ToggleAction::Status => print_toggle_status(&name, "low latency", commands::low_latency_status(&mut gaia_conn).await?),
                on_off => {
                    let resp = commands::set_low_latency(&mut gaia_conn, matches!(on_off, ToggleAction::On)).await?;
                    print_gaia_response(&name, &resp);
                }
            }
        }
        Command::BtCompatibility { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                ToggleAction::Status => {
                    print_toggle_status(&name, "bt compatibility", commands::bt_compatibility_status(&mut gaia_conn).await?)
                }
                on_off => {
                    let resp = commands::set_bt_compatibility(&mut gaia_conn, matches!(on_off, ToggleAction::On)).await?;
                    print_gaia_response(&name, &resp);
                }
            }
        }
        Command::AudioMode { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                AudioModeAction::Set { mode } => {
                    let resp = commands::set_audio_mode(&mut gaia_conn, mode).await?;
                    print_gaia_response(&name, &resp);
                }
                AudioModeAction::Status => match commands::audio_mode(&mut gaia_conn).await? {
                    Some(mode) => println!("{name}: audio mode = {mode}"),
                    None => println!("{name}: audio mode reply had no value"),
                },
            }
        }
        Command::VoicePrompt { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                ToggleAction::Status => print_toggle_status(&name, "voice prompt", commands::voice_prompt_status(&mut gaia_conn).await?),
                on_off => {
                    let resp = commands::set_voice_prompt(&mut gaia_conn, matches!(on_off, ToggleAction::On)).await?;
                    print_gaia_response(&name, &resp);
                }
            }
        }
        Command::PromptLanguage { transport, index } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            let resp = commands::set_prompt_language(&mut gaia_conn, index).await?;
            print_gaia_response(&name, &resp);
        }
        Command::AutoPowerOff { transport, action } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            match action {
                AutoPowerOffAction::Set { seconds } => {
                    let resp = commands::set_auto_power_off(&mut gaia_conn, seconds).await?;
                    print_gaia_response(&name, &resp);
                }
                AutoPowerOffAction::Status => match commands::auto_power_off_seconds(&mut gaia_conn).await? {
                    Some(seconds) => println!("{name}: auto power-off = {seconds} seconds"),
                    None => println!("{name}: auto power-off reply had no value"),
                },
            }
        }
        Command::Listen { transport, seconds } => {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            commands::register_for_live_status(&mut gaia_conn).await?;
            // Extra, not-yet-trusted registration this command exists to
            // test - see gaia::CATEGORY_SONOVA_DEVICE_MANAGEMENT's docs.
            gaia_conn
                .send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_REGISTER_NOTIFICATION, &[gaia::CATEGORY_SONOVA_DEVICE_MANAGEMENT])
                .await?;
            let mut receiver = gaia_conn.subscribe();
            println!("{name}: listening for {seconds}s - do something to the device now (e.g. disconnect/reconnect a paired phone)...");
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, receiver.recv()).await {
                    Ok(Ok(resp)) => println!(
                        "[{:>6.1}s] vendor=0x{:04x} command=0x{:04x} status={:?} payload={:02x?}",
                        seconds as f64 - remaining.as_secs_f64(),
                        resp.vendor_id,
                        resp.command_id,
                        resp.status,
                        resp.payload
                    ),
                    Ok(Err(e)) => println!("notification channel error: {e:#}"),
                    Err(_timeout) => break,
                }
            }
            println!("{name}: done listening.");
        }
        Command::Gaia { transport, vendor, command, payload, version } => {
            let vendor_id = gaia::parse_hex_u16(&vendor).context("invalid --vendor")?;
            let command_id = gaia::parse_hex_u16(&command).context("invalid --command")?;
            let payload_bytes = gaia::parse_hex_bytes(&payload).context("invalid --payload")?;
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, cli.device.as_deref(), transport).await?;
            let resp = commands::send_raw(&mut gaia_conn, version, vendor_id, command_id, &payload_bytes).await?;
            print_gaia_response(&name, &resp);
        }
    }

    Ok(())
}
