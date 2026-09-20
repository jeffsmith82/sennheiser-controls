//! Slint GUI frontend for `sennheiser-core`. Slint's event loop runs on the
//! main thread and all core operations are async (zbus/bluer), so this file
//! keeps one background tokio runtime, spawns work onto it from (synchronous)
//! Slint callbacks, and marshals results back to the UI thread with
//! `slint::invoke_from_event_loop`.

slint::include_modules!();

use anyhow::{anyhow, Result};
use sennheiser_core::{battery_ble, bluez, commands, gaia, transport, Connection};
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, Mutex as AsyncMutex};

/// Runs `fut` on the background tokio runtime, then marshals its result back
/// onto the UI thread and hands it to `on_done`. Shared by every callback
/// below so each one only has to state what it does, not how to get back to
/// the UI thread safely.
fn spawn<T, Fut>(
    rt: &tokio::runtime::Handle,
    ui: &AppWindow,
    fut: Fut,
    on_done: impl FnOnce(&AppWindow, Result<T>) + Send + 'static,
) where
    T: Send + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    let ui_weak = ui.as_weak();
    rt.spawn(async move {
        let result = fut.await;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                on_done(&ui, result);
            }
        });
    });
}

fn append_log(ui: &AppWindow, line: impl AsRef<str>) {
    let mut text = ui.get_log_text().to_string();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line.as_ref());
    ui.set_log_text(text.into());
}

fn format_volume(volume: u16) -> String {
    let pct = volume as u32 * 100 / bluez::MAX_VOLUME as u32;
    format!("{volume}/{} ({pct}%)", bluez::MAX_VOLUME)
}

/// Converts a raw EQ gain unit (the signed byte `commands::set_eq_band`
/// sends/receives) to a dB display label: a uniform `raw / 10` scale,
/// matching the "rock" preset's known raw values `[0, 20, 25, 15, -20]`
/// against their dB equivalents `[0, 2, 2.5, 1.5, -2]`.
fn eq_gain_db_label(raw: i8) -> String {
    let db = raw as f32 / 10.0;
    let sign = if db > 0.0 { "+" } else { "" };
    format!("{sign}{db:.1}dB")
}

fn set_eq_band_slider(ui: &AppWindow, band: u8, gain: i8) {
    let label = eq_gain_db_label(gain).into();
    match band {
        0 => {
            ui.set_eq_band_0(gain as f32);
            ui.set_eq_band_0_label(label);
        }
        1 => {
            ui.set_eq_band_1(gain as f32);
            ui.set_eq_band_1_label(label);
        }
        2 => {
            ui.set_eq_band_2(gain as f32);
            ui.set_eq_band_2_label(label);
        }
        3 => {
            ui.set_eq_band_3(gain as f32);
            ui.set_eq_band_3_label(label);
        }
        4 => {
            ui.set_eq_band_4(gain as f32);
            ui.set_eq_band_4_label(label);
        }
        _ => {}
    }
}

fn format_gaia_response(resp: &gaia::GaiaResponse) -> String {
    let status = match resp.status {
        Some(0) => "OK".to_string(),
        Some(code) => format!("error/unknown code {code}"),
        None => "no status byte".to_string(),
    };
    format!(
        "response vendor=0x{:04x} command=0x{:04x} status={status} payload={:02x?}",
        resp.vendor_id, resp.command_id, resp.payload
    )
}

/// State shared across every callback. `Arc<Mutex<_>>` rather than
/// `Rc<RefCell<_>>` because closures capturing it are moved into tokio
/// futures that must be `Send`, even though they only ever execute on the UI
/// thread in practice (once via a callback, once via `invoke_from_event_loop`).
/// The GAIA channel itself is additionally behind its own `Arc<AsyncMutex<_>>`
/// so spawned tokio tasks can lock and use it directly without going through
/// this outer mutex.
struct AppState {
    conn: Connection,
    devices: Vec<bluez::ConnectedDevice>,
    transport_kind: transport::TransportKind,
    gaia: Arc<AsyncMutex<Option<transport::GaiaConnection>>>,
}

fn selected_device_name(state: &AppState, index: i32) -> Option<String> {
    usize::try_from(index).ok().and_then(|i| state.devices.get(i)).map(|d| d.name.clone())
}

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let rt_handle = rt.handle().clone();

    let ui = AppWindow::new()?;
    ui.set_eq_preset_names(
        gaia::EQ_PRESETS
            .iter()
            .map(|(name, _)| slint::SharedString::from(*name))
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );

    let conn = rt_handle.block_on(bluez::system_bus())?;
    let state = Arc::new(Mutex::new(AppState {
        conn,
        devices: Vec::new(),
        transport_kind: transport::TransportKind::Classic,
        gaia: Arc::new(AsyncMutex::new(None)),
    }));

    refresh_devices(&rt_handle, &ui, &state);

    // --- Device panel ---
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_refresh_devices(move || {
            if let Some(ui) = ui_weak.upgrade() {
                refresh_devices(&rt, &ui, &state);
            }
        });
    }
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_device_selected(move |_index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            state.lock().unwrap().gaia = Arc::new(AsyncMutex::new(None));
            ui.set_gaia_connected(false);
            ui.set_connection_status("Not connected".into());
            reset_gaia_status_labels(&ui);
            refresh_device_data(&rt, &ui, &state);
        });
    }
    {
        let (rt, state, ui_weak) = (rt_handle.clone(), state.clone(), ui.as_weak());
        ui.on_set_transport(move |kind| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let transport_kind = if kind == "ble" { transport::TransportKind::Ble } else { transport::TransportKind::Classic };
            state.lock().unwrap().transport_kind = transport_kind;
            append_log(&ui, format!("transport set to {kind}"));
            ui.set_transport_kind(kind);
            // Re-open the GAIA channel over the newly chosen transport.
            connect_and_query(&rt, &ui, &state);
        });
    }
    wire_connect_disconnect(&ui, &rt_handle, &state);

    // --- Volume ---
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_refresh_volume(move || {
            if let Some(ui) = ui_weak.upgrade() {
                refresh_volume(&rt, &ui, &state);
            }
        });
    }
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_set_volume(move |value| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (conn, filter) = {
                let s = state.lock().unwrap();
                (s.conn.clone(), selected_device_name(&s, ui.get_selected_device_index()))
            };
            let value = value.clamp(0, bluez::MAX_VOLUME as i32) as u16;
            spawn(
                &rt,
                &ui,
                async move {
                    let target = bluez::find_target(&conn, filter.as_deref()).await?;
                    bluez::set_volume(&conn, &target.transport_path, value).await?;
                    bluez::get_volume(&conn, &target.transport_path).await
                },
                |ui, result| match result {
                    Ok(v) => {
                        ui.set_volume_value(v as f32);
                        ui.set_volume_text(format_volume(v).into());
                        append_log(ui, format!("volume set: {}", format_volume(v)));
                    }
                    Err(e) => append_log(ui, format!("error: {e:#}")),
                },
            );
        });
    }
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_volume_step(move |delta| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (conn, filter) = {
                let s = state.lock().unwrap();
                (s.conn.clone(), selected_device_name(&s, ui.get_selected_device_index()))
            };
            spawn(
                &rt,
                &ui,
                async move {
                    let target = bluez::find_target(&conn, filter.as_deref()).await?;
                    let current = bluez::get_volume(&conn, &target.transport_path).await?;
                    let new = if delta >= 0 {
                        current.saturating_add(delta as u16).min(bluez::MAX_VOLUME)
                    } else {
                        current.saturating_sub((-delta) as u16)
                    };
                    bluez::set_volume(&conn, &target.transport_path, new).await?;
                    Ok(new)
                },
                |ui, result| match result {
                    Ok(v) => {
                        ui.set_volume_value(v as f32);
                        ui.set_volume_text(format_volume(v).into());
                        append_log(ui, format!("volume set: {}", format_volume(v)));
                    }
                    Err(e) => append_log(ui, format!("error: {e:#}")),
                },
            );
        });
    }

    // --- Battery ---
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_refresh_battery_bluez(move || {
            if let Some(ui) = ui_weak.upgrade() {
                refresh_battery_bluez(&rt, &ui, &state);
            }
        });
    }
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_refresh_battery_btleplug(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let filter = selected_device_name(&state.lock().unwrap(), ui.get_selected_device_index());
            spawn(
                &rt,
                &ui,
                async move { battery_ble::read_battery_percentage(filter.as_deref()).await },
                |ui, result| match result {
                    Ok((name, pct)) => {
                        let text = format!("{pct}% (via btleplug GATT read)");
                        ui.set_battery_text(text.clone().into());
                        append_log(ui, format!("battery ({name}): {text}"));
                    }
                    Err(e) => append_log(ui, format!("error: {e:#}")),
                },
            );
        });
    }

    // --- GAIA-dependent controls ---
    macro_rules! gaia_action {
        ($setter:ident, |$gaia_conn:ident $(, $arg:ident : $arg_ty:ty)*| $body:expr, |$ok:ident| $log:expr) => {
            let (rt, state) = (rt_handle.clone(), state.clone());
            let ui_weak = ui.as_weak();
            ui.$setter(move |$($arg: $arg_ty),*| {
                let Some(ui) = ui_weak.upgrade() else { return };
                let gaia = state.lock().unwrap().gaia.clone();
                spawn(
                    &rt,
                    &ui,
                    async move {
                        let mut guard = gaia.lock().await;
                        let $gaia_conn = guard.as_mut().ok_or_else(|| anyhow!("not connected to a GAIA control channel"))?;
                        $body
                    },
                    |ui, result| match result {
                        Ok($ok) => append_log(ui, $log),
                        Err(e) => append_log(ui, format!("error: {e:#}")),
                    },
                );
            });
        };
    }

    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_anc_status(move || {
            if let Some(ui) = ui_weak.upgrade() {
                query_anc_status(&rt, &ui, state.lock().unwrap().gaia.clone());
            }
        });
    }
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_multipoint_status(move || {
            if let Some(ui) = ui_weak.upgrade() {
                query_multipoint_status(&rt, &ui, state.lock().unwrap().gaia.clone());
            }
        });
    }

    gaia_action!(
        on_anc_set,
        |g, adaptive: bool| commands::set_anc(g, adaptive).await,
        |pair| format!(
            "anc set: primary {} / companion {}",
            format_gaia_response(&pair.0),
            format_gaia_response(&pair.1)
        )
    );
    gaia_action!(
        on_bass_boost_set,
        |g, on: bool| commands::set_bass_boost(g, on).await,
        |resp| format!("bass boost set: {}", format_gaia_response(&resp))
    );
    // Pulled out of `gaia_action!` because, unlike the others, this needs to
    // push the confirmed gains back into the (read-only, preset-driven -
    // see app.slint) band sliders on success.
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_eq_preset_apply(move |name| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let gaia = state.lock().unwrap().gaia.clone();
            spawn(
                &rt,
                &ui,
                async move {
                    let mut guard = gaia.lock().await;
                    let g = guard.as_mut().ok_or_else(|| anyhow!("not connected to a GAIA control channel"))?;
                    commands::set_eq_preset(g, name.as_str()).await
                },
                |ui, result| match result {
                    Ok(results) => {
                        let mut lines = vec![format!("eq preset applied ({} bands):", results.len())];
                        for (band, gain, resp) in &results {
                            set_eq_band_slider(ui, *band, *gain);
                            lines.push(format!("  band {band} -> {gain}: {}", format_gaia_response(resp)));
                        }
                        append_log(ui, lines.join("\n"));
                    }
                    Err(e) => append_log(ui, format!("error: {e:#}")),
                },
            );
        });
    }
    {
        let (rt, state) = (rt_handle.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_crossfeed_set(move |level| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let gaia = state.lock().unwrap().gaia.clone();
            let level = match level.as_str() {
                "low" => commands::CrossfeedLevel::Low,
                "high" => commands::CrossfeedLevel::High,
                _ => commands::CrossfeedLevel::Off,
            };
            spawn(
                &rt,
                &ui,
                async move {
                    let mut guard = gaia.lock().await;
                    let g = guard.as_mut().ok_or_else(|| anyhow!("not connected to a GAIA control channel"))?;
                    let set_resp = commands::set_crossfeed(g, level).await?;
                    // Crossfeed isn't known to push its own changes live (no
                    // notification category for it was ever found - see
                    // gaia::CMD_SONOVA_CROSSFEED_SET docs) - re-query
                    // explicitly instead of trusting the optimistic click.
                    let status = commands::crossfeed_status(g).await?;
                    Ok((set_resp, status))
                },
                |ui, result| match result {
                    Ok((resp, status)) => {
                        append_log(ui, format!("crossfeed set: {}", format_gaia_response(&resp)));
                        match status {
                            Some(level) => {
                                ui.set_crossfeed_selected(crossfeed_level_str(level).into());
                                append_log(ui, format!("crossfeed re-queried: {level:?}"));
                            }
                            None => append_log(ui, "crossfeed re-queried: unrecognized value"),
                        }
                    }
                    Err(e) => append_log(ui, format!("error: {e:#}")),
                },
            );
        });
    }
    gaia_action!(
        on_anti_wind_set,
        |g, on: bool| commands::set_anti_wind(g, on).await,
        |resp| format!("anti-wind set: {}", format_gaia_response(&resp))
    );
    gaia_action!(
        on_multipoint_set,
        |g, on: bool| commands::set_multipoint(g, on).await,
        |resp| format!("multipoint set: {}", format_gaia_response(&resp))
    );
    gaia_action!(
        on_noise_control_apply,
        |g, percent: i32| commands::set_noise_control_custom(g, percent.clamp(0, 100) as u8).await,
        |pair| format!("noise control set: {} / commit: {}", format_gaia_response(&pair.0), format_gaia_response(&pair.1))
    );
    gaia_action!(
        on_raw_gaia_send,
        |g,
         vendor: slint::SharedString,
         command: slint::SharedString,
         payload: slint::SharedString,
         version: slint::SharedString| {
            let vendor_id = gaia::parse_hex_u16(vendor.as_str())?;
            let command_id = gaia::parse_hex_u16(command.as_str())?;
            let payload_bytes = gaia::parse_hex_bytes(payload.as_str())?;
            let version: u8 = version.as_str().trim().parse().map_err(|_| anyhow!("invalid version"))?;
            commands::send_raw(g, version, vendor_id, command_id, &payload_bytes).await
        },
        |resp| format!("raw gaia: {}", format_gaia_response(&resp))
    );

    ui.run()?;
    Ok(())
}

fn wire_connect_disconnect(ui: &AppWindow, rt: &tokio::runtime::Handle, state: &Arc<Mutex<AppState>>) {
    {
        let (rt, state) = (rt.clone(), state.clone());
        let ui_weak = ui.as_weak();
        ui.on_connect_gaia(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (conn, filter, transport_kind, gaia) = {
                let s = state.lock().unwrap();
                (s.conn.clone(), selected_device_name(&s, ui.get_selected_device_index()), s.transport_kind, s.gaia.clone())
            };
            connect_gaia(&rt, &ui, conn, filter, transport_kind, gaia);
        });
    }
    {
        let state = state.clone();
        let ui_weak = ui.as_weak();
        ui.on_disconnect_gaia(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            *state.lock().unwrap().gaia.blocking_lock() = None;
            ui.set_gaia_connected(false);
            ui.set_connection_status("Not connected".into());
            reset_gaia_status_labels(&ui);
            append_log(&ui, "disconnected");
        });
    }
}

fn reset_gaia_status_labels(ui: &AppWindow) {
    ui.set_anc_status_text("unknown".into());
    ui.set_multipoint_status_text("unknown".into());
}

/// Subscribes to the just-opened GAIA channel's live notification feed and
/// keeps the UI in sync with it for as long as the connection lives - covers
/// both the full state snapshot the headset broadcasts unprompted the
/// moment the channel opens and any later change, including ones made via
/// the headset's own physical buttons with no app involvement at all (see
/// `sennheiser_core::gaia::CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY` docs).
///
/// Needs no explicit teardown: once `state.gaia` is replaced with `None` or
/// a new connection (disconnect, reconnect, switching transport), the old
/// `GaiaConnection` is dropped, which drops its notification channel, which
/// ends this task's `recv()` loop with `RecvError::Closed`.
fn spawn_gaia_notification_listener(
    rt: &tokio::runtime::Handle,
    ui: &AppWindow,
    gaia: Arc<AsyncMutex<Option<transport::GaiaConnection>>>,
) {
    let ui_weak = ui.as_weak();
    rt.spawn(async move {
        let mut receiver = {
            let mut guard = gaia.lock().await;
            match guard.as_mut() {
                Some(conn) => conn.subscribe(),
                None => return,
            }
        };
        loop {
            match receiver.recv().await {
                Ok(resp) => {
                    let Some(event) = commands::interpret_event(&resp) else { continue };
                    let ui_weak = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            apply_device_event(&ui, event);
                        }
                    });
                }
                // A slow consumer missed some notifications - just keep
                // going with whatever arrives next, no need to treat a
                // live-status display as needing every single one.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// Applies one decoded live-device event to the UI. Called from the
/// notification listener above (already marshaled onto the UI thread).
fn apply_device_event(ui: &AppWindow, event: commands::DeviceEvent) {
    match event {
        commands::DeviceEvent::AncMode(on) => {
            ui.set_anc_selected(if on { "adaptive" } else { "off" }.into());
            append_log(ui, format!("[live] ANC mode: {}", if on { "Adaptive" } else { "Off" }));
        }
        commands::DeviceEvent::AntiWind(on) => {
            ui.set_anti_wind_on(on);
            append_log(ui, format!("[live] anti-wind: {}", if on { "on" } else { "off" }));
        }
        commands::DeviceEvent::CustomNoiseControl(percent) => {
            ui.set_noise_control_percent(percent as f32);
            append_log(ui, format!("[live] custom noise control: {percent}%"));
        }
        commands::DeviceEvent::CustomModeActive(active) => {
            if active {
                ui.set_anc_selected("custom".into());
                ui.set_anc_custom_visible(true);
            } else {
                // On every transition captured live, leaving custom mode
                // (e.g. the physical double-tap gesture) lands back on
                // Adaptive - there's no observed case of it landing on the
                // discrete "Off" mode instead, so that's the assumption here.
                ui.set_anc_selected("adaptive".into());
            }
            append_log(ui, format!("[live] custom mode active: {active}"));
        }
        commands::DeviceEvent::BassBoost(on) => {
            ui.set_bass_boost_on(on);
            append_log(ui, format!("[live] bass boost: {}", if on { "on" } else { "off" }));
        }
        commands::DeviceEvent::MultipointEnabled(on) => {
            ui.set_multipoint_on(on);
            append_log(ui, format!("[live] multipoint: {}", if on { "on" } else { "off" }));
        }
        commands::DeviceEvent::MultipointStatus(status) => {
            let text = match status {
                commands::MultipointStatus::On => "ON (2 devices)".to_string(),
                commands::MultipointStatus::Off => "OFF (1 device)".to_string(),
                commands::MultipointStatus::Unknown(v) => format!("unexpected value {v}"),
                commands::MultipointStatus::NoValue => "no value returned".to_string(),
            };
            ui.set_multipoint_status_text(text.clone().into());
            if let commands::MultipointStatus::On | commands::MultipointStatus::Off = status {
                ui.set_multipoint_on(matches!(status, commands::MultipointStatus::On));
            }
            append_log(ui, format!("[live] multipoint status: {text}"));
        }
        commands::DeviceEvent::Crossfeed(level) => {
            ui.set_crossfeed_selected(crossfeed_level_str(level).into());
            append_log(ui, format!("[live] crossfeed: {level:?}"));
        }
        commands::DeviceEvent::EqBands(gains) => {
            for (band, gain) in gains.into_iter().enumerate() {
                set_eq_band_slider(ui, band as u8, gain);
            }
            append_log(ui, format!("[live] eq bands: {gains:?}"));
            // Best-effort: there's no real "get current preset name" opcode,
            // only the raw gains, so this is a fingerprint match against
            // gaia::EQ_PRESETS, not a device-reported preset name.
            if let Some((index, name)) = commands::find_eq_preset(gains) {
                ui.set_eq_preset_index(index as i32);
                append_log(ui, format!("[live] eq bands match preset '{name}'"));
            }
        }
    }
}

/// Opens the GAIA control channel for the given device/transport, registers
/// it for live push notifications (`commands::register_for_live_status` -
/// without this the headset never pushes anything, see its docs), then - on
/// success - automatically queries the settings that have a known "get"
/// opcode (ANC is an unverified opcode, Multipoint is verified; see
/// `core/src/gaia.rs`), and starts the live notification listener
/// (`spawn_gaia_notification_listener`) that keeps every other GAIA-backed
/// control in sync with the device from here on.
fn connect_gaia(
    rt: &tokio::runtime::Handle,
    ui: &AppWindow,
    conn: Connection,
    filter: Option<String>,
    transport_kind: transport::TransportKind,
    gaia_slot: Arc<AsyncMutex<Option<transport::GaiaConnection>>>,
) {
    ui.set_connection_status("Connecting...".into());
    reset_gaia_status_labels(ui);
    let (rt_followup, gaia_followup) = (rt.clone(), gaia_slot.clone());
    spawn(
        rt,
        ui,
        async move {
            let (name, mut gaia_conn) = bluez::open_gaia_connection(&conn, filter.as_deref(), transport_kind).await?;
            // Without this, the headset never pushes anything at all - see
            // `commands::register_for_live_status` docs.
            commands::register_for_live_status(&mut gaia_conn).await?;
            *gaia_slot.lock().await = Some(gaia_conn);
            Ok(name)
        },
        move |ui, result| match result {
            Ok(name) => {
                ui.set_gaia_connected(true);
                ui.set_connection_status(format!("Connected to {name}").into());
                append_log(ui, format!("connected to {name}"));
                query_anc_status(&rt_followup, ui, gaia_followup.clone());
                query_multipoint_status(&rt_followup, ui, gaia_followup.clone());
                query_crossfeed_status(&rt_followup, ui, gaia_followup.clone());
                spawn_gaia_notification_listener(&rt_followup, ui, gaia_followup);
            }
            Err(e) => {
                ui.set_gaia_connected(false);
                ui.set_connection_status("Not connected".into());
                append_log(ui, format!("error: {e:#}"));
            }
        },
    );
}

/// Looks up the currently selected device and (re-)opens the GAIA channel to
/// it. A no-op if nothing is selected yet.
fn connect_and_query(rt: &tokio::runtime::Handle, ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    let (conn, filter, transport_kind, gaia) = {
        let s = state.lock().unwrap();
        (s.conn.clone(), selected_device_name(&s, ui.get_selected_device_index()), s.transport_kind, s.gaia.clone())
    };
    let Some(filter) = filter else { return };
    connect_gaia(rt, ui, conn, Some(filter), transport_kind, gaia);
}

fn refresh_volume(rt: &tokio::runtime::Handle, ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    let (conn, filter) = {
        let s = state.lock().unwrap();
        (s.conn.clone(), selected_device_name(&s, ui.get_selected_device_index()))
    };
    spawn(
        rt,
        ui,
        async move {
            let target = bluez::find_target(&conn, filter.as_deref()).await?;
            bluez::get_volume(&conn, &target.transport_path).await
        },
        |ui, result| match result {
            Ok(v) => {
                ui.set_volume_value(v as f32);
                ui.set_volume_text(format_volume(v).into());
                append_log(ui, format!("volume: {}", format_volume(v)));
            }
            Err(e) => append_log(ui, format!("error: {e:#}")),
        },
    );
}

fn refresh_battery_bluez(rt: &tokio::runtime::Handle, ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    let (conn, filter) = {
        let s = state.lock().unwrap();
        (s.conn.clone(), selected_device_name(&s, ui.get_selected_device_index()))
    };
    spawn(
        rt,
        ui,
        async move {
            let device = bluez::find_connected_device(&conn, filter.as_deref()).await?;
            bluez::get_battery(&conn, &device.path).await
        },
        |ui, result| match result {
            Ok(pct) => {
                let text = format!("{pct}% (via org.bluez.Battery1)");
                ui.set_battery_text(text.clone().into());
                append_log(ui, format!("battery: {text}"));
            }
            Err(e) => append_log(ui, format!("error: {e:#}")),
        },
    );
}

/// Refreshes everything about the currently selected device that can be
/// auto-populated: volume, battery, and (via `connect_and_query`) the GAIA
/// channel plus its readable settings.
fn refresh_device_data(rt: &tokio::runtime::Handle, ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    refresh_volume(rt, ui, state);
    refresh_battery_bluez(rt, ui, state);
    connect_and_query(rt, ui, state);
}

fn query_anc_status(rt: &tokio::runtime::Handle, ui: &AppWindow, gaia: Arc<AsyncMutex<Option<transport::GaiaConnection>>>) {
    spawn(
        rt,
        ui,
        async move {
            let mut guard = gaia.lock().await;
            let g = guard.as_mut().ok_or_else(|| anyhow!("not connected to a GAIA control channel"))?;
            commands::anc_status(g).await
        },
        |ui, result| match result {
            Ok(resp) => {
                let text = format_gaia_response(&resp);
                ui.set_anc_status_text(text.clone().into());
                append_log(ui, format!("anc status (unverified opcode): {text}"));
            }
            Err(e) => {
                ui.set_anc_status_text("unknown".into());
                append_log(ui, format!("anc status error: {e:#}"));
            }
        },
    );
}

fn query_multipoint_status(rt: &tokio::runtime::Handle, ui: &AppWindow, gaia: Arc<AsyncMutex<Option<transport::GaiaConnection>>>) {
    spawn(
        rt,
        ui,
        async move {
            let mut guard = gaia.lock().await;
            let g = guard.as_mut().ok_or_else(|| anyhow!("not connected to a GAIA control channel"))?;
            commands::multipoint_status(g).await
        },
        |ui, result| match result {
            Ok(status) => {
                let text = match status {
                    commands::MultipointStatus::On => "ON (2 devices)".to_string(),
                    commands::MultipointStatus::Off => "OFF (1 device)".to_string(),
                    commands::MultipointStatus::Unknown(v) => format!("unexpected value {v}"),
                    commands::MultipointStatus::NoValue => "no value returned".to_string(),
                };
                ui.set_multipoint_status_text(text.clone().into());
                if let commands::MultipointStatus::On | commands::MultipointStatus::Off = status {
                    ui.set_multipoint_on(matches!(status, commands::MultipointStatus::On));
                }
                append_log(ui, format!("multipoint status: {text}"));
            }
            Err(e) => {
                ui.set_multipoint_status_text("unknown".into());
                append_log(ui, format!("multipoint status error: {e:#}"));
            }
        },
    );
}

fn query_crossfeed_status(rt: &tokio::runtime::Handle, ui: &AppWindow, gaia: Arc<AsyncMutex<Option<transport::GaiaConnection>>>) {
    spawn(
        rt,
        ui,
        async move {
            let mut guard = gaia.lock().await;
            let g = guard.as_mut().ok_or_else(|| anyhow!("not connected to a GAIA control channel"))?;
            commands::crossfeed_status(g).await
        },
        |ui, result| match result {
            Ok(Some(level)) => {
                ui.set_crossfeed_selected(crossfeed_level_str(level).into());
                append_log(ui, format!("crossfeed status: {level:?}"));
            }
            Ok(None) => append_log(ui, "crossfeed status: device returned an unrecognized value"),
            Err(e) => append_log(ui, format!("crossfeed status error: {e:#}")),
        },
    );
}

fn crossfeed_level_str(level: commands::CrossfeedLevel) -> &'static str {
    match level {
        commands::CrossfeedLevel::Off => "off",
        commands::CrossfeedLevel::Low => "low",
        commands::CrossfeedLevel::High => "high",
    }
}

fn refresh_devices(rt: &tokio::runtime::Handle, ui: &AppWindow, state: &Arc<Mutex<AppState>>) {
    let (conn, state, rt_owned) = (state.lock().unwrap().conn.clone(), state.clone(), rt.clone());
    spawn(
        rt,
        ui,
        async move { bluez::list_all_connected_devices(&conn).await },
        move |ui, result| match result {
            Ok(devices) => {
                let has_devices = !devices.is_empty();
                let names: Vec<slint::SharedString> =
                    devices.iter().map(|d| slint::SharedString::from(format!("{} ({})", d.name, d.address))).collect();
                ui.set_device_names(names.as_slice().into());
                if has_devices {
                    ui.set_selected_device_index(0);
                } else {
                    ui.set_selected_device_index(-1);
                    append_log(ui, "no connected devices found");
                }
                state.lock().unwrap().devices = devices;
                if has_devices {
                    refresh_device_data(&rt_owned, ui, &state);
                }
            }
            Err(e) => append_log(ui, format!("error: {e:#}")),
        },
    );
}
