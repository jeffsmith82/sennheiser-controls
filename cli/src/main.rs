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
    /// its status (VERIFIED against real HDB 630 hardware). Note: the
    /// headphones only report a device COUNT over GAIA, not device
    /// identities - there is no GAIA command for "list of connected
    /// devices with names"; use `bluez-devices` for that (see its help).
    Multipoint {
        /// Which physical channel to send the command over
        #[arg(long, value_enum, default_value = "classic")]
        transport: transport::TransportKind,
        #[command(subcommand)]
        action: MultipointAction,
    },
    /// List every Bluetooth device currently connected to THIS machine
    /// (standard BlueZ Device1.Connected - not a GAIA/headphone-specific
    /// query). This is the closest equivalent to "list of connected
    /// devices" available: the headphones themselves only report how many
    /// hosts are connected via multipoint (see `multipoint status`), not
    /// which ones or their names.
    BluezDevices,
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
