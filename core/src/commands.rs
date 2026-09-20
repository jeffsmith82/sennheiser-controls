//! High-level GAIA control operations (ANC, transparency, EQ, etc), each one
//! sequencing one or more `GaiaConnection::send` calls using the opcodes
//! documented in [`crate::gaia`]. Kept separate from the raw protocol module
//! so callers (CLI, GUI) get a friendly verb-based API instead of having to
//! know vendor IDs and command IDs themselves.

use crate::gaia::{self, GaiaResponse};
use crate::transport::GaiaConnection;
use anyhow::{anyhow, Result};

pub async fn anc_status(gaia: &mut GaiaConnection) -> Result<GaiaResponse> {
    gaia.send(1, gaia::SENNHEISER_VENDOR_ID, gaia::CMD_ANC_GET, &[]).await
}

/// Verified live against a real HDB 630: the app always sends these two
/// commands back to back for a single ANC mode change.
pub async fn set_anc(gaia: &mut GaiaConnection, adaptive: bool) -> Result<(GaiaResponse, GaiaResponse)> {
    let state: u8 = if adaptive { 0x01 } else { 0x00 };
    let primary = gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_ANC_SET, &[0x03, state]).await?;
    let companion = gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_ANC_SET_COMPANION, &[state]).await?;
    Ok((primary, companion))
}

pub async fn transparency_status(gaia: &mut GaiaConnection) -> Result<GaiaResponse> {
    gaia.send(1, gaia::SENNHEISER_VENDOR_ID, gaia::CMD_TRANSPARENCY_GET, &[]).await
}

pub async fn set_transparency(gaia: &mut GaiaConnection, on: bool) -> Result<GaiaResponse> {
    let state: u8 = if on { 1 } else { 0 };
    gaia.send(1, gaia::SENNHEISER_VENDOR_ID, gaia::CMD_TRANSPARENCY_SET, &[state]).await
}

pub async fn set_bass_boost(gaia: &mut GaiaConnection, on: bool) -> Result<GaiaResponse> {
    let state: u8 = if on { 0x01 } else { 0x00 };
    gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_BASS_BOOST_SET, &[state]).await
}

pub async fn set_eq_band(gaia: &mut GaiaConnection, band: u8, gain: i8) -> Result<GaiaResponse> {
    gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_EQ_SET_BAND, &[band, gain as u8]).await
}

/// Applies a named preset by sending all 5 band gains in sequence, returning
/// each (band, gain, response) triple in the order they were sent.
pub async fn set_eq_preset(gaia: &mut GaiaConnection, name: &str) -> Result<Vec<(u8, i8, GaiaResponse)>> {
    let key = name.to_lowercase();
    let values = gaia::EQ_PRESETS
        .iter()
        .find(|(n, _)| *n == key)
        .map(|(_, v)| *v)
        .ok_or_else(|| {
            anyhow!(
                "unknown preset '{name}'. Known presets: {}",
                gaia::EQ_PRESETS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            )
        })?;

    let mut results = Vec::with_capacity(values.len());
    for (band, gain) in values.iter().enumerate() {
        let resp = set_eq_band(gaia, band as u8, *gain).await?;
        results.push((band as u8, *gain, resp));
    }
    Ok(results)
}

/// Finds the [`gaia::EQ_PRESETS`] entry whose 5 band gains exactly match
/// `gains`, for matching a [`DeviceEvent::EqBands`] snapshot back to a named
/// preset - there's no real "get current preset name" opcode, only the raw
/// gains, so this is a best-effort fingerprint match. `None` if nothing
/// matches (a manually-tweaked EQ that doesn't correspond to any preset).
pub fn find_eq_preset(gains: [i8; 5]) -> Option<(usize, &'static str)> {
    gaia::EQ_PRESETS.iter().position(|(_, preset_gains)| *preset_gains == gains).map(|i| (i, gaia::EQ_PRESETS[i].0))
}

/// Sets the "Noise control: Custom" ANC/Transparency crossfade slider (set +
/// commit, matching the app's always-paired command sequence).
pub async fn set_noise_control_custom(gaia: &mut GaiaConnection, percent: u8) -> Result<(GaiaResponse, GaiaResponse)> {
    let set = gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_CUSTOM_NOISE_CONTROL_SET, &[percent]).await?;
    let commit = gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_CUSTOM_NOISE_CONTROL_COMMIT, &[]).await?;
    Ok((set, commit))
}

pub async fn set_multipoint(gaia: &mut GaiaConnection, on: bool) -> Result<GaiaResponse> {
    let state: u8 = if on { 0x01 } else { 0x00 };
    gaia.send(3, gaia::QUALCOMM_VENDOR_ID, gaia::CMD_QUALCOMM_MULTIPOINT_SET, &[state]).await
}

/// The headphones only report a device COUNT over GAIA, not device
/// identities - there is no GAIA command for "list of connected devices
/// with names".
#[derive(Debug, Clone, Copy)]
pub enum MultipointStatus {
    On,
    Off,
    Unknown(u8),
    NoValue,
}

pub async fn multipoint_status(gaia: &mut GaiaConnection) -> Result<MultipointStatus> {
    let resp = gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_MULTIPOINT_STATUS_GET, &[]).await?;
    // This response has no real status byte - what our generic parser reads
    // as `status` is actually the count value itself.
    Ok(match resp.status {
        Some(2) => MultipointStatus::On,
        Some(1) => MultipointStatus::Off,
        Some(other) => MultipointStatus::Unknown(other),
        None => MultipointStatus::NoValue,
    })
}

pub async fn set_anti_wind(gaia: &mut GaiaConnection, on: bool) -> Result<GaiaResponse> {
    let state: u8 = if on { 0x01 } else { 0x00 };
    gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_ANC_SET, &[0x01, state]).await
}

/// Unlike every other verified command, the values are NOT sequential with
/// the UI order - see [`gaia::CROSSFEED_OFF`]/[`gaia::CROSSFEED_LOW`]/[`gaia::CROSSFEED_HIGH`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossfeedLevel {
    Off,
    Low,
    High,
}

impl CrossfeedLevel {
    /// Decodes the raw wire value (see `gaia::CROSSFEED_*`) - `None` for
    /// anything else, rather than guessing.
    fn from_wire(value: u8) -> Option<Self> {
        match value {
            gaia::CROSSFEED_OFF => Some(Self::Off),
            gaia::CROSSFEED_LOW => Some(Self::Low),
            gaia::CROSSFEED_HIGH => Some(Self::High),
            _ => None,
        }
    }
}

pub async fn set_crossfeed(gaia: &mut GaiaConnection, level: CrossfeedLevel) -> Result<GaiaResponse> {
    let state = match level {
        CrossfeedLevel::Off => gaia::CROSSFEED_OFF,
        CrossfeedLevel::Low => gaia::CROSSFEED_LOW,
        CrossfeedLevel::High => gaia::CROSSFEED_HIGH,
    };
    gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_CROSSFEED_SET, &[state]).await
}

/// Reads the current Crossfeed level via [`gaia::CMD_SONOVA_CROSSFEED_GET`].
/// `Ok(None)` means the device replied with a value this library doesn't
/// recognize, not that the request failed.
pub async fn crossfeed_status(gaia: &mut GaiaConnection) -> Result<Option<CrossfeedLevel>> {
    let resp = gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_CROSSFEED_GET, &[]).await?;
    Ok(resp.status.and_then(CrossfeedLevel::from_wire))
}

/// Registers this connection for live push notifications on every category
/// this library has confirmed a use for (see `gaia::CATEGORY_*` docs) -
/// without this, [`crate::transport::GaiaConnection::subscribe`] never
/// receives anything at all, even across changes made via the headset's own
/// physical buttons (see [`gaia::CMD_REGISTER_NOTIFICATION`] docs for why).
/// Callers should send this once, right after connecting, before relying on
/// `subscribe` for anything.
pub async fn register_for_live_status(gaia: &mut GaiaConnection) -> Result<()> {
    gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_REGISTER_NOTIFICATION, &[gaia::CATEGORY_SONOVA_ANC]).await?;
    gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_REGISTER_NOTIFICATION, &[gaia::CATEGORY_SONOVA_CUSTOM_NOISE_CONTROL])
        .await?;
    gaia.send(3, gaia::SONOVA_VENDOR_ID, gaia::CMD_REGISTER_NOTIFICATION, &[gaia::CATEGORY_SONOVA_BASS_BOOST]).await?;
    gaia.send(3, gaia::QUALCOMM_VENDOR_ID, gaia::CMD_REGISTER_NOTIFICATION, &[gaia::CATEGORY_QUALCOMM_MULTIPOINT])
        .await?;
    Ok(())
}

/// A push notification or command-ack decoded from a raw [`GaiaResponse`],
/// covering every command in [`gaia`] whose response/notify ID is VERIFIED
/// (see its docs there). Produced by [`interpret_event`] from whatever
/// arrives on [`crate::transport::GaiaConnection::subscribe`] - which,
/// *after* calling [`register_for_live_status`] on the connection, includes
/// both the reply to a `send` call in progress and every unprompted push:
/// the current-value snapshot each registration triggers immediately, and
/// every later change, including ones from the headset's own physical
/// buttons with no app involved at all (see [`gaia::CMD_REGISTER_NOTIFICATION`]
/// docs) - this is what makes it possible to show live device state instead
/// of only ever setting it.
#[derive(Debug, Clone, Copy)]
pub enum DeviceEvent {
    AncMode(bool),
    AntiWind(bool),
    CustomNoiseControl(u8),
    CustomModeActive(bool),
    BassBoost(bool),
    MultipointEnabled(bool),
    MultipointStatus(MultipointStatus),
    Crossfeed(CrossfeedLevel),
    /// Gains for all 5 bands, in order - see [`gaia::RSP_SONOVA_EQ_SET_BAND`]
    /// docs for how band 0 (in `status`) and bands 1-4 (in `payload`) get
    /// reassembled into this.
    EqBands([i8; 5]),
}

/// Decodes a raw GAIA frame into a [`DeviceEvent`] if it's one this library
/// knows the meaning of. Returns `None` for anything else - a command this
/// library doesn't decode, or a request-only frame with no informative
/// payload - which callers should treat as "nothing to update", not an error.
pub fn interpret_event(resp: &GaiaResponse) -> Option<DeviceEvent> {
    let payload = resp.payload.as_slice();
    match (resp.vendor_id, resp.command_id) {
        (gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_ANC_SET) => {
            // 6-byte payload; the last 2 bytes are [param, state] - see
            // CMD_SONOVA_ANC_SET's docs.
            let param = *payload.get(payload.len().checked_sub(2)?)?;
            let state = *payload.last()?;
            match param {
                0x03 => Some(DeviceEvent::AncMode(state == 1)),
                0x01 => Some(DeviceEvent::AntiWind(state == 1)),
                _ => None,
            }
        }
        // These are all single-byte-value replies. Per `parse_packet`'s
        // convention (see its docs), that one byte lands in `status`, not
        // `payload` - `payload` is only what comes *after* it, which is
        // empty here. Confirmed live: every one of these arrived with
        // `payload=[]` and the value sitting in `status`.
        (gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_ANC_SET_COMPANION) => {
            Some(DeviceEvent::AncMode(resp.status? == 1))
        }
        (gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_CUSTOM_NOISE_CONTROL_SET) => {
            Some(DeviceEvent::CustomNoiseControl(resp.status?))
        }
        (gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY) => {
            Some(DeviceEvent::CustomModeActive(resp.status? == 1))
        }
        (gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_BASS_BOOST_SET) => {
            Some(DeviceEvent::BassBoost(resp.status? == 1))
        }
        (gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_CROSSFEED_GET) => {
            Some(DeviceEvent::Crossfeed(CrossfeedLevel::from_wire(resp.status?)?))
        }
        // status = band 0, payload[0..4] = bands 1-4 - see
        // RSP_SONOVA_EQ_SET_BAND docs. Only decoded at this exact shape,
        // since the same command ID also carries other, undeciphered
        // payload shapes (from the same CATEGORY_SONOVA_BASS_BOOST
        // registration).
        (gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_EQ_SET_BAND) if payload.len() == 4 => {
            let band0 = resp.status? as i8;
            Some(DeviceEvent::EqBands([band0, payload[0] as i8, payload[1] as i8, payload[2] as i8, payload[3] as i8]))
        }
        (gaia::QUALCOMM_VENDOR_ID, gaia::RSP_QUALCOMM_MULTIPOINT_SET) => {
            Some(DeviceEvent::MultipointEnabled(resp.status? == 1))
        }
        (gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_MULTIPOINT_STATUS_GET) => {
            Some(DeviceEvent::MultipointStatus(match resp.status? {
                2 => MultipointStatus::On,
                1 => MultipointStatus::Off,
                other => MultipointStatus::Unknown(other),
            }))
        }
        _ => None,
    }
}

/// Sends an arbitrary raw GAIA command - for the CLI's `gaia` subcommand and
/// an "advanced" GUI panel, when experimenting with opcodes this library
/// doesn't otherwise know about.
pub async fn send_raw(
    gaia: &mut GaiaConnection,
    version: u8,
    vendor_id: u16,
    command_id: u16,
    payload: &[u8],
) -> Result<GaiaResponse> {
    gaia.send(version, vendor_id, command_id, payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a [`GaiaResponse`] without going through the wire-format
    /// parser, for tests that only care about `interpret_event`'s own
    /// decode logic.
    fn resp(vendor_id: u16, command_id: u16, status: Option<u8>, payload: &[u8]) -> GaiaResponse {
        GaiaResponse { vendor_id, command_id, status, payload: payload.to_vec() }
    }

    #[test]
    fn interpret_event_decodes_anc_mode_from_real_capture() {
        // Real capture: the RSP_SONOVA_ANC_SET push during the connect-time
        // handshake burst - status=0x01, payload=[00,02,00,03,01] (param
        // 0x03 = ANC mode, state 0x01 = Adaptive) - see
        // gaia::RSP_SONOVA_ANC_SET docs.
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_ANC_SET, Some(0x01), &[0x00, 0x02, 0x00, 0x03, 0x01]);
        assert!(matches!(interpret_event(&r), Some(DeviceEvent::AncMode(true))));
    }

    #[test]
    fn interpret_event_decodes_anti_wind_from_anc_set_response() {
        // Same response ID as ANC mode, distinguished only by the param
        // byte (0x01 = Anti-wind here instead of 0x03 = ANC mode) - see
        // CMD_SONOVA_ANC_SET's docs.
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_ANC_SET, Some(0x00), &[0x00, 0x00, 0x00, 0x01, 0x00]);
        assert!(matches!(interpret_event(&r), Some(DeviceEvent::AntiWind(false))));
    }

    #[test]
    fn interpret_event_ignores_anc_set_response_with_unknown_param() {
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_ANC_SET, Some(0x00), &[0x00, 0x00, 0x00, 0x99, 0x01]);
        assert!(interpret_event(&r).is_none());
    }

    #[test]
    fn interpret_event_does_not_panic_on_too_short_anc_set_payload() {
        // Regression guard for the `payload.len().checked_sub(2)` guard -
        // an empty or 1-byte payload must decode to None, not panic.
        assert!(interpret_event(&resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_ANC_SET, Some(0), &[])).is_none());
        assert!(interpret_event(&resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_ANC_SET, Some(0), &[0x01])).is_none());
    }

    #[test]
    fn interpret_event_decodes_anc_set_companion_from_status_byte() {
        // Real capture: pushed alongside RSP_SONOVA_ANC_SET at connect time,
        // payload=[] - the value is genuinely in `status`, not `payload`
        // (see parse_packet's docs) - this exact mixup was a real bug fixed
        // live this session.
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_ANC_SET_COMPANION, Some(0x01), &[]);
        assert!(matches!(interpret_event(&r), Some(DeviceEvent::AncMode(true))));
    }

    #[test]
    fn interpret_event_decodes_custom_noise_control_from_real_captures() {
        // Real captures from the double-tap ANC/Transparency gesture:
        // status=100 (full Transparency) and status=0 (back to Adaptive).
        let transparency = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_CUSTOM_NOISE_CONTROL_SET, Some(100), &[]);
        assert!(matches!(interpret_event(&transparency), Some(DeviceEvent::CustomNoiseControl(100))));

        let adaptive = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_CUSTOM_NOISE_CONTROL_SET, Some(0), &[]);
        assert!(matches!(interpret_event(&adaptive), Some(DeviceEvent::CustomNoiseControl(0))));
    }

    #[test]
    fn interpret_event_decodes_custom_mode_active_from_real_captures() {
        let active = resp(gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY, Some(1), &[]);
        assert!(matches!(interpret_event(&active), Some(DeviceEvent::CustomModeActive(true))));

        let inactive = resp(gaia::SONOVA_VENDOR_ID, gaia::CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY, Some(0), &[]);
        assert!(matches!(interpret_event(&inactive), Some(DeviceEvent::CustomModeActive(false))));
    }

    #[test]
    fn interpret_event_decodes_bass_boost_from_real_capture() {
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_BASS_BOOST_SET, Some(0), &[]);
        assert!(matches!(interpret_event(&r), Some(DeviceEvent::BassBoost(false))));
    }

    #[test]
    fn interpret_event_decodes_crossfeed_using_non_sequential_encoding() {
        let off = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_CROSSFEED_GET, Some(gaia::CROSSFEED_OFF), &[]);
        assert!(matches!(interpret_event(&off), Some(DeviceEvent::Crossfeed(CrossfeedLevel::Off))));

        let low = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_CROSSFEED_GET, Some(gaia::CROSSFEED_LOW), &[]);
        assert!(matches!(interpret_event(&low), Some(DeviceEvent::Crossfeed(CrossfeedLevel::Low))));

        let high = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_CROSSFEED_GET, Some(gaia::CROSSFEED_HIGH), &[]);
        assert!(matches!(interpret_event(&high), Some(DeviceEvent::Crossfeed(CrossfeedLevel::High))));
    }

    #[test]
    fn interpret_event_ignores_unrecognized_crossfeed_value() {
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_CROSSFEED_GET, Some(0x05), &[]);
        assert!(interpret_event(&r).is_none());
    }

    #[test]
    fn interpret_event_decodes_eq_bands_from_real_rock_capture() {
        // Real capture: device set to "rock" ([0, 20, 25, 15, -20]) -
        // status=0 (band 0), payload=[0x14, 0x19, 0x0f, 0xec] (bands 1-4) -
        // see gaia::RSP_SONOVA_EQ_SET_BAND docs.
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_EQ_SET_BAND, Some(0), &[0x14, 0x19, 0x0f, 0xec]);
        assert!(matches!(interpret_event(&r), Some(DeviceEvent::EqBands([0, 20, 25, 15, -20]))));
    }

    #[test]
    fn interpret_event_decodes_eq_bands_from_real_dance_capture() {
        // Real capture: the final ack while applying "dance"
        // ([35, 20, -15, 15, 30]) - status=35 (band 0, chosen specifically
        // because it's non-zero, which is what proved band 0 lives in
        // `status` and isn't just omitted - see gaia::RSP_SONOVA_EQ_SET_BAND
        // docs for the full story).
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_EQ_SET_BAND, Some(35), &[0x14, 0xf1, 0x0f, 0x1e]);
        let event = interpret_event(&r);
        assert!(matches!(event, Some(DeviceEvent::EqBands([35, 20, -15, 15, 30]))));

        let DeviceEvent::EqBands(gains) = event.unwrap() else { unreachable!() };
        assert_eq!(find_eq_preset(gains), Some((4, "dance")));
    }

    #[test]
    fn interpret_event_ignores_eq_set_band_response_with_other_payload_shapes() {
        // The same command ID also carries other, undeciphered payload
        // shapes under the same registration category - only the exact
        // 4-byte shape should decode.
        let wrong_len = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_EQ_SET_BAND, Some(0), &[0x14, 0x19, 0x0f]);
        assert!(interpret_event(&wrong_len).is_none());
    }

    #[test]
    fn interpret_event_decodes_multipoint_enabled_from_real_capture() {
        let r = resp(gaia::QUALCOMM_VENDOR_ID, gaia::RSP_QUALCOMM_MULTIPOINT_SET, Some(1), &[]);
        assert!(matches!(interpret_event(&r), Some(DeviceEvent::MultipointEnabled(true))));
    }

    #[test]
    fn interpret_event_decodes_multipoint_status_from_real_capture() {
        let on = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_MULTIPOINT_STATUS_GET, Some(2), &[]);
        assert!(matches!(interpret_event(&on), Some(DeviceEvent::MultipointStatus(MultipointStatus::On))));

        let unexpected = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_SONOVA_MULTIPOINT_STATUS_GET, Some(9), &[]);
        assert!(matches!(interpret_event(&unexpected), Some(DeviceEvent::MultipointStatus(MultipointStatus::Unknown(9)))));
    }

    #[test]
    fn interpret_event_requires_the_right_vendor_id_not_just_command_id() {
        // RSP_QUALCOMM_MULTIPOINT_SET (0x0e80) under the WRONG vendor should
        // not decode as anything - vendor_id namespaces command_id, so a
        // matching command_id under a different vendor is a different,
        // unrelated command.
        let r = resp(gaia::SONOVA_VENDOR_ID, gaia::RSP_QUALCOMM_MULTIPOINT_SET, Some(1), &[]);
        assert!(interpret_event(&r).is_none());
    }

    #[test]
    fn interpret_event_ignores_unknown_command() {
        let r = resp(gaia::SONOVA_VENDOR_ID, 0xffff, Some(0), &[]);
        assert!(interpret_event(&r).is_none());
    }

    #[test]
    fn find_eq_preset_matches_known_presets_exactly() {
        assert_eq!(find_eq_preset([0, 20, 25, 15, -20]), Some((2, "rock")));
        assert_eq!(find_eq_preset([0, 0, 0, 0, 0]), Some((0, "neutral")));
    }

    #[test]
    fn find_eq_preset_returns_none_for_a_manually_tweaked_eq() {
        assert_eq!(find_eq_preset([1, 2, 3, 4, 5]), None);
    }
}
