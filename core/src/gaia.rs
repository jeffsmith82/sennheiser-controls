//! GAIA-over-classic-Bluetooth (RFCOMM/SPP) framing.
//!
//! The frame format (SOF/version/flags/length header, then a 4-byte
//! vendor_id+command_id GAIA header, then payload) was reverse engineered
//! from Sennheiser's Android app (`SPPGaiaFramer`/`SPPGaiaDeframer`/
//! `GaiaCommand` in its JS bundle) and independently confirmed byte-for-byte
//! against a live HCI snoop capture of a real headset (Sonova-branded
//! "HDB 630", controlled by Sonova's `com.sonova.chb.control` app - Sonova
//! acquired Sennheiser's consumer headphone business and evidently kept
//! using GAIA, just under their own vendor ID and command set).
//!
//! GAIA is Qualcomm/CSR's generic headset control protocol: `vendor_id` is
//! a Bluetooth SIG company identifier that namespaces the `command_id`
//! space, so different vendors (or the same vendor's old vs. new firmware)
//! define entirely different commands under it.

use anyhow::{bail, Result};

/// Sonova's own GAIA vendor ID. Confirmed live: captured via HCI snoop log
/// while toggling ANC in `com.sonova.chb.control` against a real HDB 630,
/// repeated across two separate captures / 6 total on-off transitions, byte
/// for byte identical every time.
pub const SONOVA_VENDOR_ID: u16 = 0x0495;

/// Qualcomm/CSR's own GAIA vendor ID (the chipset vendor's reserved
/// namespace, not Sonova's). Confirmed live: multipoint is apparently a
/// core chipset-level feature, not a Sonova app feature, so it lives here
/// instead of under [`SONOVA_VENDOR_ID`].
pub const QUALCOMM_VENDOR_ID: u16 = 0x001d;

/// VERIFIED against real hardware: registers this GAIA client for
/// unsolicited push notifications on one category of device state. Payload
/// is 1 byte: the category ID (see the `CATEGORY_*` constants below). The
/// response, `0x0107` (this ID + 0x100), is a bare ack with no informative
/// payload - the actual state, both an immediate "here's the current value"
/// push and every later change, arrives on its own command ID (see each
/// `CATEGORY_*` constant's docs) once registration completes.
///
/// This is a real prerequisite, not optional connect-time noise: a from-
/// scratch client that opens the GAIA channel and skips this step gets
/// nothing pushed at all, ever - confirmed live, including across a
/// physical double-tap ANC gesture that produced zero frames on an
/// unregistered connection. Registering is what turns it on.
///
/// Seen under both [`SONOVA_VENDOR_ID`] and [`QUALCOMM_VENDOR_ID`] with
/// identical behavior (register a category, get the bare ack, then start
/// receiving that vendor's own pushes for it) - likely a shared GAIA-core
/// command rather than something either vendor defined themselves, reused
/// unchanged across both vendor_id namespaces.
///
/// Confirmed live: an HCI snoop of the real app's connection handshake
/// showed each `CATEGORY_*` registration below immediately followed (within
/// single-digit-to-tens of milliseconds) by exactly the push its docs
/// describe, with no other plausible trigger nearby.
pub const CMD_REGISTER_NOTIFICATION: u16 = 0x0007;

/// Registers for ANC mode changes (see [`CMD_REGISTER_NOTIFICATION`]) -
/// triggers pushes on [`RSP_SONOVA_ANC_SET`] and
/// [`RSP_SONOVA_ANC_SET_COMPANION`] (which covers Anti-wind too, since it
/// shares [`RSP_SONOVA_ANC_SET`]'s response ID). Under [`SONOVA_VENDOR_ID`].
pub const CATEGORY_SONOVA_ANC: u8 = 0x0d;

/// Registers for the "Custom noise control" slider and its companion
/// active-flag (see [`CMD_REGISTER_NOTIFICATION`]) - triggers pushes on
/// [`RSP_SONOVA_CUSTOM_NOISE_CONTROL_SET`] and
/// [`CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY`]. Under [`SONOVA_VENDOR_ID`].
pub const CATEGORY_SONOVA_CUSTOM_NOISE_CONTROL: u8 = 0x0c;

/// Registers for a broader "audio settings" bundle than the name given here
/// suggests (see [`CMD_REGISTER_NOTIFICATION`]) - confirmed live triggering
/// pushes on [`RSP_SONOVA_BASS_BOOST_SET`] AND [`RSP_SONOVA_EQ_SET_BAND`],
/// plus several other command IDs in the same `0x108x`/`0x109x` range this
/// library doesn't decode yet (`0x1091`, `0x108b`, `0x108d`, `0x1093`,
/// `0x108f` - all observed live, contents not yet deciphered). Named after
/// Bass Boost since that was confirmed first; kept rather than renamed to
/// avoid overstating how completely this category's contents are understood.
/// Under [`SONOVA_VENDOR_ID`].
pub const CATEGORY_SONOVA_BASS_BOOST: u8 = 0x08;

/// Registers for Multipoint changes (see [`CMD_REGISTER_NOTIFICATION`]) -
/// triggers pushes on [`RSP_QUALCOMM_MULTIPOINT_SET`]. Under
/// [`QUALCOMM_VENDOR_ID`], not Sonova's.
pub const CATEGORY_QUALCOMM_MULTIPOINT: u8 = 0x07;

/// VERIFIED against real hardware, under [`QUALCOMM_VENDOR_ID`] (not Sonova's
/// vendor ID - see its docs). Payload is 1 byte: `0x00` off, `0x01` on.
/// Confirmed across 3 on/off/on transitions, byte-for-byte identical every
/// time. Response command ID `0x0e80` (this ID + 0x80) echoes the state
/// byte; the device also separately sends a no-payload `0x0f00` right after.
pub const CMD_QUALCOMM_MULTIPOINT_SET: u16 = 0x0e00;

/// The response ID for [`CMD_QUALCOMM_MULTIPOINT_SET`] (`+ 0x80`, not the
/// usual `+ 0x81` most other commands in this file use for their ack) -
/// see its docs.
pub const RSP_QUALCOMM_MULTIPOINT_SET: u16 = 0x0e80;

/// VERIFIED against real hardware, under [`SONOVA_VENDOR_ID`]. A status
/// query the app sends (empty payload) right after every multipoint change;
/// the response, command ID `0x1509` (this ID + 0x100), carries a 1-byte
/// payload: `0x02` when multipoint is on, `0x01` when off. This reports a
/// count/capacity, not device identities - the headphones don't report
/// *which* devices are connected or their names over GAIA at all. The
/// app's "Connection Management" device list (names like "Pixel 6",
/// "jeffpc") comes entirely from the phone's own Bluetooth pairing
/// history, cross-referenced with this count - confirmed by capturing the
/// screen being opened fresh, which sent no additional GAIA query at all.
pub const CMD_SONOVA_MULTIPOINT_STATUS_GET: u16 = 0x1409;

/// The response ID for [`CMD_SONOVA_MULTIPOINT_STATUS_GET`] (`+ 0x100`) -
/// see its docs. The count (`0x02`/`0x01`) lands in [`GaiaResponse::status`],
/// not `payload` - see [`parse_packet`].
pub const RSP_SONOVA_MULTIPOINT_STATUS_GET: u16 = 0x1509;

/// VERIFIED against real hardware (see module docs). This is really a
/// general "set a noise-control parameter" command, not ANC-specific -
/// the payload's first byte is a parameter ID and the second is its value:
///
/// - param `0x03` = ANC mode (`state`: `0x00` Off, `0x01` Adaptive/On -
///   "Custom" mode's value is unverified). Always sent together with
///   [`CMD_SONOVA_ANC_SET_COMPANION`] on every change - both were seen back
///   to back every time. Response command ID `0x1a81` (this ID + 0x81)
///   echoes the state byte in a 6-byte payload.
/// - param `0x01` = Anti-wind (`state`: `0x00` off, `0x01` on). No
///   companion command this time - confirmed via 7 on/off transitions in a
///   separate capture, byte-for-byte identical every time.
pub const CMD_SONOVA_ANC_SET: u16 = 0x1a00;

/// The response ID for [`CMD_SONOVA_ANC_SET`] (`+ 0x81`) - see its docs.
/// Also pushed unprompted (not just as an ack to our own writes) once a
/// client has registered for [`CATEGORY_SONOVA_ANC`] - see
/// [`CMD_REGISTER_NOTIFICATION`] docs.
pub const RSP_SONOVA_ANC_SET: u16 = 0x1a81;

/// VERIFIED against real hardware (see module docs). Sent alongside
/// [`CMD_SONOVA_ANC_SET`] on every ANC (param `0x03`) change only - Anti-wind
/// (param `0x01`) doesn't use this. Payload is 1 byte: the same `state`
/// value (0x00 Off / 0x01 Adaptive). Response command ID is `0x1a85` (this
/// command's ID + 0x81), echoing the state byte.
pub const CMD_SONOVA_ANC_SET_COMPANION: u16 = 0x1a04;

/// The response ID for [`CMD_SONOVA_ANC_SET_COMPANION`] (`+ 0x81`) - see its
/// docs. Also pushed unprompted alongside [`RSP_SONOVA_ANC_SET`], gated by
/// the same [`CATEGORY_SONOVA_ANC`] registration.
pub const RSP_SONOVA_ANC_SET_COMPANION: u16 = 0x1a85;

/// VERIFIED against real hardware (see module docs): the "Noise control:
/// Custom" slider, which crossfades continuously between full ANC (0) and
/// full Transparency (100), with 50 presumably being neutral/off (matches
/// the app's own axis label, "0% - ANC / 100% - Transparency"). Confirmed
/// with 8 distinct clean single-tap values (0, 1, 2, 25, 50, 75, 97, 98),
/// all decoding byte-for-byte to the tapped percentage. Payload is 1 byte:
/// the percentage, 0-100.
///
/// Selecting "Custom" in the *app* does NOT send a separate mode-select
/// command (param `0x03` under [`CMD_SONOVA_ANC_SET`] was never seen with
/// any value besides 0x00/0x01 across every app-driven capture) - opening
/// the slider screen is pure local UI state. Sending this command directly,
/// with no prior "enter Custom mode" step, is expected to work.
///
/// However, the *device* does track a real "custom mode engaged" bit
/// independent of the app's UI: see [`CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY`],
/// which the headset pushes unprompted alongside this command's ack whenever
/// its physical ANC/Transparency button is double-tapped. So the earlier
/// claim was only true for the app-initiated path - the firmware itself
/// clearly has a notion of "am I in a named mode (Adaptive) or a manual
/// crossfade position" that isn't purely cosmetic.
pub const CMD_SONOVA_CUSTOM_NOISE_CONTROL_SET: u16 = 0x1a02;

/// The response ID for [`CMD_SONOVA_CUSTOM_NOISE_CONTROL_SET`] (`+ 0x81`) -
/// see its docs. Also pushed unprompted on every physical-button change,
/// gated by [`CATEGORY_SONOVA_CUSTOM_NOISE_CONTROL`] registration - see
/// [`CMD_REGISTER_NOTIFICATION`] docs. [`CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY`]
/// fires alongside it under the same registration.
pub const RSP_SONOVA_CUSTOM_NOISE_CONTROL_SET: u16 = 0x1a83;

/// VERIFIED against real hardware (see module docs): a no-payload companion
/// sent immediately after every [`CMD_SONOVA_CUSTOM_NOISE_CONTROL_SET`], with
/// no exceptions across 8 confirmed taps - looks like a "commit" signal.
pub const CMD_SONOVA_CUSTOM_NOISE_CONTROL_COMMIT: u16 = 0x1a03;

/// VERIFIED against real hardware, but as a **push-only device notification**,
/// not something this library ever sends: captured via an HCI snoop of the
/// headset's physical ANC/Transparency double-tap button (no app involved -
/// phone-side traffic on the GAIA channel was silent throughout). The
/// double-tap gesture does not touch [`CMD_SONOVA_ANC_SET`] at all; instead
/// the headset drives its "Custom noise control" slider to its extremes and
/// spontaneously reports both:
/// - this command's response ID (`0x1a02` + `0x81` = `0x1a83`) with payload
///   `0x64` (100, full Transparency) or `0x00` (0, back to neutral/Adaptive),
/// - and this ID, with payload `0x01` (custom/manual position engaged) or
///   `0x00` (not engaged) - firing right alongside, always in that order.
///
/// Confirmed byte-for-byte identical across 2 full off/on double-tap cycles
/// (4 transitions total) in one capture session; not yet cross-checked in a
/// second independent session the way most other commands in this file are.
///
/// This, and the analogous [`CMD_SONOVA_ANC_SET`]/[`CMD_SONOVA_ANC_SET_COMPANION`]
/// acks, are what answer "how does the app know the current ANC state" -
/// there is no GET-status command for it at all. But it's not quite as
/// simple as "the headset just broadcasts state on connect": a first
/// capture made it look that way, because the app always performs the same
/// connect-time handshake and one step of that handshake is exactly what's
/// needed to see it (see [`CMD_REGISTER_NOTIFICATION`], added after a
/// from-scratch client that skipped that step confirmed live it receives
/// nothing at all, ever, without it). Once a client sends that registration
/// for [`CATEGORY_SONOVA_CUSTOM_NOISE_CONTROL`], *then* it gets both this
/// push and [`CMD_SONOVA_CUSTOM_NOISE_CONTROL_SET`]'s ack immediately (the
/// current value, as a kind of "registration confirmed" snapshot) and again
/// on every later change, from either the app or the physical button.
///
/// The SET-side base ID this would pair with under the usual `base`/`base+0x81`
/// convention (`0x1804`) was never observed on the wire - inferred from the
/// pattern the other companion commands in this file follow, not confirmed.
pub const CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY: u16 = 0x1885;

/// VERIFIED against real hardware (see module docs): captured via a second,
/// independent HCI snoop session toggling the "Bass boost" switch, 8 total
/// on/off transitions, byte-for-byte identical every time. Unlike ANC, this
/// is a single command with no companion - payload is 1 byte: `0x00` = off,
/// `0x01` = on. The device sends back two replies: command ID `0x1108` (this
/// command's ID + 0x100, empty payload, a bare ack) and `0x1089` (this
/// command's ID + 0x81, payload = the same state byte, the real echo).
pub const CMD_SONOVA_BASS_BOOST_SET: u16 = 0x1008;

/// The "real echo" response ID for [`CMD_SONOVA_BASS_BOOST_SET`] (`+ 0x81`) -
/// see its docs. The other reply, `0x1108` (`+ 0x100`), is a bare ack with
/// no payload and isn't given its own constant since there's nothing to decode.
pub const RSP_SONOVA_BASS_BOOST_SET: u16 = 0x1089;

/// VERIFIED against real hardware (see module docs): captured via a third
/// HCI snoop session, cycling through 8 of the 9 EQ preset buttons in the
/// app (Jazz's write burst wasn't captured - a gap in that particular
/// session, not a different protocol). There is no single "select preset N"
/// command - each preset is just a fixed set of 5 band gains that the app
/// uploads individually with this command whenever you pick it. Payload is
/// 2 bytes: `[band_index (0-4), gain]`, where `gain` is a signed 8-bit dB-ish
/// value (`i8` bit pattern in a `u8`). Sending this to a band the app itself
/// doesn't have a button for still works - it's a real per-band control, not
/// a preset-only command.
pub const CMD_SONOVA_EQ_SET_BAND: u16 = 0x1001;

/// The response ID for [`CMD_SONOVA_EQ_SET_BAND`] (`+ 0x81`) - unlike every
/// other command in this file, also pushed unprompted as a **full snapshot
/// of all 5 bands' current gain** - not the `[band_index, gain]` pairs the
/// SET side uses. Following this file's usual convention (see
/// [`parse_packet`]), the response's first extra byte lands in
/// [`GaiaResponse::status`] rather than `payload` - here that byte is
/// genuinely band 0's gain, not a real status/OK code, and `payload[0..4]`
/// are bands 1 through 4.
///
/// VERIFIED live in two stages. First, registering
/// [`CATEGORY_SONOVA_BASS_BOOST`] (which this rides along with, despite the
/// name) on a device set to `"rock"` (`[0, 20, 25, 15, -20]`, see
/// [`EQ_PRESETS`]) produced `status=Some(0)`, `payload=[0x14, 0x19, 0x0f,
/// 0xec]` - `0` plus `20, 25, 15, -20`, matching Rock exactly, but with `0`
/// ambiguous between "band 0 is really 0" and "this byte is just a generic
/// OK status". That was resolved by applying `"dance"` (`[35, 20, -15, 15,
/// 30]` - chosen specifically for its non-zero band 0) and watching the acks
/// arrive one write at a time: `status=Some(35)` from the very first ack
/// (band 0's own write) onward, then `payload` updating band-by-band as each
/// subsequent write landed (`[14,19,0f,ec]` -> `[14,f1,0f,ec]` ->
/// `[14,f1,0f,1e]`, i.e. bands 1-4 converging on Dance's `20,-15,15,30` in
/// order) while `status` stayed `35` throughout - a generic status code
/// would not track the specific preset being applied like that.
pub const RSP_SONOVA_EQ_SET_BAND: u16 = 0x1082;

/// The 5-band gain values (see [`CMD_SONOVA_EQ_SET_BAND`]) for each named
/// preset, captured live from the real app.
///
/// The original capture session tapped through the carousel in order
/// (neutral, speech-clarity, rock, pop, dance, hip-hop, classical, movie,
/// jazz) and attributed each observed write burst to whichever preset had
/// just been tapped. Speech Clarity's tap apparently produced no
/// distinguishable write burst of its own (the app skips re-sending state
/// that already matches what it thinks the device has - the same behavior
/// separately confirmed for Jazz/Movie below), so the *next* burst (Rock's)
/// was mistakenly attributed to Speech Clarity's label, cascading a
/// one-slot shift through the rest of the table. This was caught by
/// comparing the CLI/GUI's applied preset against what the official
/// Sonova/Sennheiser app reported afterwards - every preset from Rock
/// through Jazz was one slot off. Neutral and Jazz were unaffected: Neutral
/// anchors the start, and Jazz's own tap produced no write burst either (it
/// was tapped right after Movie, whose values are identical), independently
/// confirmed in a second capture to carry the same 5 values as Movie - so
/// Movie and Jazz being identical below is expected, not a leftover of the
/// shift.
///
/// Net effect: Rock/Pop/Dance/Hip-hop/Classical/Movie/Jazz below have each
/// been relabeled to the value one slot ahead of where the original capture
/// put them (e.g. real Rock is the data the original table had filed under
/// Speech Clarity). Real Movie and real Jazz are consequently no longer
/// identical to each other post-relabel, even though the two adjacent
/// *original* capture slots they came from (old "movie" and old "jazz",
/// both `[-32, 0, 22, 22, 0]`) were - that was the shift bug in the first
/// place, not a property of the real presets.
///
/// Speech Clarity's true values were never actually captured - there is no
/// slot 10 to recover them from - so its entry below is an UNVERIFIED,
/// fabricated placeholder (a generic vocal-clarity curve: cut bass, boost
/// upper-mid/treble), not hardware-confirmed data like the rest of the
/// table. Treat it as a starting point to tweak, not a known-good preset.
pub const EQ_PRESETS: &[(&str, [i8; 5])] = &[
    ("neutral", [0, 0, 0, 0, 0]),
    ("speech-clarity", [-10, -5, 5, 15, 10]), // UNVERIFIED / fabricated - see doc comment above
    ("rock", [0, 20, 25, 15, -20]),
    ("pop", [0, -25, 0, 25, 0]),
    ("dance", [35, 20, -15, 15, 30]),
    ("hip-hop", [30, 15, -15, 0, -15]),
    ("classical", [-20, -15, 0, 35, 40]),
    ("movie", [0, 0, 20, 20, -20]),
    ("jazz", [-32, 0, 22, 22, 0]),
];

/// VERIFIED against real hardware (see module docs): captured via a fourth
/// HCI snoop session, cycling Crossfeed through Off -> Low -> High. Single
/// command, payload is 1 byte. Unlike every other verified command here, the
/// values are NOT sequential with the UI order: `Off = 0x02`, `Low = 0x00`,
/// `High = 0x01`. Response command ID is `0x2f00` (this ID + 0x100, not the
/// usual + 0x81 the ANC-family commands use) - a bare ack with no payload,
/// confirmed live via [`CMD_SONOVA_CROSSFEED_GET`] genuinely reporting the
/// new value back afterward, not just this ack arriving.
pub const CMD_SONOVA_CROSSFEED_SET: u16 = 0x2e00;
pub const CROSSFEED_OFF: u8 = 0x02;
pub const CROSSFEED_LOW: u8 = 0x00;
pub const CROSSFEED_HIGH: u8 = 0x01;

/// VERIFIED against real hardware, from the same session as
/// [`CMD_SONOVA_CROSSFEED_SET`]: the app sends this (empty payload) once
/// during its connect-time handshake, before Crossfeed was ever toggled -
/// this is a plain GET, not a `CMD_REGISTER_NOTIFICATION` category, and
/// wasn't seen sent again on its own after a `CMD_SONOVA_CROSSFEED_SET`
/// write, so re-querying after every change (rather than trusting a push)
/// is the safe approach until a capture proves otherwise.
pub const CMD_SONOVA_CROSSFEED_GET: u16 = 0x2e01;

/// The response ID for [`CMD_SONOVA_CROSSFEED_GET`] (`+ 0x100`) - see its
/// docs. The value lands in [`GaiaResponse::status`], not `payload`, same
/// as every other single-byte reply in this file - see [`parse_packet`].
/// Uses the same non-sequential encoding as [`CMD_SONOVA_CROSSFEED_SET`]
/// (`CROSSFEED_OFF`/`CROSSFEED_LOW`/`CROSSFEED_HIGH`).
pub const RSP_SONOVA_CROSSFEED_GET: u16 = 0x2f01;

/// Sennheiser electronic GmbH's own (older) GAIA vendor ID, reverse
/// engineered from the original (pre-Sonova-acquisition) Sennheiser Android
/// app's device schemas for the CX 400 / True Wireless 2 product lines.
/// UNVERIFIED against real hardware - kept for devices that might still run
/// that older firmware generation, reachable via the `gaia`/`transparency`
/// raw commands.
pub const SENNHEISER_VENDOR_ID: u16 = 0x0494;

// ANC_SetAdaptiveNoiseCancelationState / _RSP, from CX400/TW2 GAIA schema (unverified).
// CMD_ANC_SET is unused now that the `anc` command uses the verified Sonova
// opcodes above, but is kept as a documented reference for the `gaia` raw
// command (pass --vendor 0x0494 --command 0x0708 manually).
#[allow(dead_code)]
pub const CMD_ANC_SET: u16 = 0x0708;
pub const CMD_ANC_GET: u16 = 0x0788;

// TransparentHearing_Mode_Set_TW / _Get_TW, from CX400/TW2 GAIA schema (unverified).
pub const CMD_TRANSPARENCY_SET: u16 = 0x0704;
pub const CMD_TRANSPARENCY_GET: u16 = 0x0784;

const SOF: u8 = 0xFF;
const FLAG_CHECKSUM: u8 = 0x01;
const FLAG_LENGTH_EXT: u8 = 0x02;

/// Builds the raw GAIA packet: vendor_id(2, BE) + command_id(2, BE) + payload.
/// This is everything a BLE GATT characteristic write carries directly -
/// BLE has no extra framing, since the write itself is already a bounded
/// packet (unlike a classic-Bluetooth RFCOMM byte stream).
pub fn build_packet(vendor_id: u16, command_id: u16, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.extend_from_slice(&vendor_id.to_be_bytes());
    packet.extend_from_slice(&command_id.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

/// Builds a GAIA command packet and wraps it in the SPP frame (SOF, version,
/// flags, length) that the app sends over classic-Bluetooth RFCOMM for
/// GAIA/GAIA3. No checksum byte is appended, matching the app's default
/// behavior (it never enables the XOR-check flag for classic transport).
pub fn build_frame(protocol_version: u8, vendor_id: u16, command_id: u16, payload: &[u8]) -> Result<Vec<u8>> {
    let packet = build_packet(vendor_id, command_id, payload);

    if packet.len() > 254 {
        bail!(
            "GAIA payload too large for this simplified framer ({} bytes; extended-length framing isn't implemented)",
            packet.len()
        );
    }

    let mut frame = Vec::with_capacity(packet.len() + 4);
    frame.push(SOF);
    frame.push(protocol_version);
    frame.push(0); // flags: no checksum, no length extension
    frame.push((packet.len() - 4) as u8);
    frame.extend_from_slice(&packet);
    Ok(frame)
}

/// A parsed GAIA response: the vendor/command IDs the device echoed back,
/// an optional status byte, and any trailing payload. `Clone` so a single
/// incoming frame can be handed both to whichever `GaiaConnection::send`
/// call is waiting on it and to any live-status subscribers via
/// `GaiaConnection::subscribe` - see `transport.rs`.
#[derive(Debug, Clone)]
pub struct GaiaResponse {
    pub vendor_id: u16,
    pub command_id: u16,
    pub status: Option<u8>,
    pub payload: Vec<u8>,
}

/// Parses a raw GAIA packet (vendor_id + command_id + [status] + [payload]),
/// as received directly from a BLE GATT notification/indication. Assumes
/// the packet carries a status byte, which holds for every ordinary command
/// response (it only doesn't for the Notification_Event_* commands, which
/// this tool never sends).
pub fn parse_packet(packet: &[u8]) -> Result<GaiaResponse> {
    if packet.len() < 4 {
        bail!("GAIA packet too short (need at least vendor_id+command_id): {packet:02x?}");
    }
    let vendor_id = u16::from_be_bytes([packet[0], packet[1]]);
    let command_id = u16::from_be_bytes([packet[2], packet[3]]);
    let (status, payload) = if packet.len() > 4 {
        (Some(packet[4]), packet[5..].to_vec())
    } else {
        (None, Vec::new())
    };
    Ok(GaiaResponse { vendor_id, command_id, status, payload })
}

/// Strips the SPP frame (SOF/version/flags/length[/checksum]) from a
/// received buffer and parses the inner GAIA packet. Used for the classic
/// RFCOMM transport only - BLE notifications/indications carry the raw
/// packet directly and should be parsed with [`parse_packet`] instead.
pub fn parse_frame(buf: &[u8]) -> Result<GaiaResponse> {
    if buf.len() < 4 || buf[0] != SOF {
        bail!("not a GAIA SPP frame (missing 0xFF start-of-frame byte): {buf:02x?}");
    }
    let flags = buf[2];
    let has_checksum = flags & FLAG_CHECKSUM != 0;
    let length_extended = flags & FLAG_LENGTH_EXT != 0;

    let (payload_len, header_len) = if length_extended {
        if buf.len() < 5 {
            bail!("truncated extended-length GAIA frame");
        }
        (((buf[3] as usize) << 8 | buf[4] as usize), 5)
    } else {
        (buf[3] as usize, 4)
    };

    let packet_len = payload_len + 4; // + vendor_id(2) + command_id(2)
    let end = header_len + packet_len;
    let expected = end + if has_checksum { 1 } else { 0 };
    if buf.len() < expected {
        bail!("truncated GAIA frame: expected {expected} bytes, got {}: {buf:02x?}", buf.len());
    }

    parse_packet(&buf[header_len..end])
}

/// Like [`parse_frame`], but tolerant of a buffer that doesn't yet hold one
/// complete frame: returns `Ok(None)` instead of erroring, plus (on success)
/// how many bytes the frame consumed so the caller can drain just that much
/// and keep any trailing bytes (the start of the next frame) for next time.
///
/// For use by a stream reader that accumulates bytes across multiple socket
/// reads and needs to peel off complete frames one at a time - unlike
/// `parse_frame`, which assumes `buf` is already exactly one frame (e.g. a
/// single already-delimited RFCOMM read in the common case where the kernel
/// happens to hand back exactly one frame per `read()`).
pub fn deframe_one(buf: &[u8]) -> Result<Option<(usize, GaiaResponse)>> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] != SOF {
        bail!("not a GAIA SPP frame (missing 0xFF start-of-frame byte): {buf:02x?}");
    }
    if buf.len() < 4 {
        return Ok(None);
    }
    let flags = buf[2];
    let has_checksum = flags & FLAG_CHECKSUM != 0;
    let length_extended = flags & FLAG_LENGTH_EXT != 0;

    let (payload_len, header_len) = if length_extended {
        if buf.len() < 5 {
            return Ok(None);
        }
        (((buf[3] as usize) << 8 | buf[4] as usize), 5)
    } else {
        (buf[3] as usize, 4)
    };

    let packet_len = payload_len + 4; // + vendor_id(2) + command_id(2)
    let end = header_len + packet_len;
    let total = end + if has_checksum { 1 } else { 0 };
    if buf.len() < total {
        return Ok(None);
    }

    Ok(Some((total, parse_packet(&buf[header_len..end])?)))
}

/// Parses a "0x1234" or "1234" hex string into a u16.
pub fn parse_hex_u16(s: &str) -> Result<u16> {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    Ok(u16::from_str_radix(s, 16)?)
}

/// Parses a hex byte string like "0001" or "00 01" into raw bytes.
pub fn parse_hex_bytes(s: &str) -> Result<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        bail!("hex payload must have an even number of digits");
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(Into::into))
        .collect()
}
