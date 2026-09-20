//! Reads battery level via the standard Bluetooth GATT Battery Service
//! (`0x180F`) / Battery Level characteristic (`0x2A19`) directly, using
//! `btleplug` instead of BlueZ's `org.bluez.Battery1` D-Bus property.
//!
//! This is deliberately kept separate from the rest of the tool (which
//! talks to BlueZ directly via `zbus`/`bluer`): `btleplug` is cross-platform
//! (BlueZ on Linux, WinRT on Windows, CoreBluetooth on macOS), so this one
//! function is the part of `sennheiser-controls` that would port to Windows
//! or macOS without changes - unlike volume (AVRCP, not GATT) or the
//! GAIA-over-RFCOMM commands (classic Bluetooth, outside btleplug's
//! BLE-only scope), which are inherently Linux/BlueZ-specific here.
//!
//! `org.bluez.Battery1.Percentage` is itself just BlueZ's own read of this
//! same GATT characteristic, so this is expected to return the same value.

use anyhow::{anyhow, Context, Result};
use btleplug::api::{Central, Manager as _, Peripheral as _};
use btleplug::platform::Manager;
use std::time::Duration;
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

const BATTERY_LEVEL_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x0000_2a19_0000_1000_8000_00805f9b34fb);

/// Finds a matching, already-known-to-the-OS BLE peripheral (no active scan
/// needed - paired/previously-seen devices are already in BlueZ's list) and
/// reads its standard Battery Level characteristic.
pub async fn read_battery_percentage(filter: Option<&str>) -> Result<(String, u8)> {
    let manager = Manager::new().await.context("failed to initialize the btleplug BLE manager")?;
    let adapters = manager.adapters().await.context("failed to list Bluetooth adapters")?;
    let adapter = adapters.into_iter().next().ok_or_else(|| anyhow!("no Bluetooth adapter available"))?;

    let peripherals = adapter
        .peripherals()
        .await
        .context("failed to list known BLE peripherals - is Bluetooth powered on?")?;

    let mut target = None;
    for peripheral in peripherals {
        let props = peripheral.properties().await?;
        let name = props
            .and_then(|p| p.local_name)
            .unwrap_or_else(|| peripheral.address().to_string());
        if let Some(f) = filter {
            if !name.to_lowercase().contains(&f.to_lowercase()) {
                continue;
            }
        }
        target = Some((peripheral, name));
        break;
    }
    let (peripheral, name) = target.ok_or_else(|| {
        anyhow!(
            "no known BLE device found{}",
            filter.map(|f| format!(" matching '{f}'")).unwrap_or_default()
        )
    })?;

    // Always call connect(), even if is_connected() already reports true -
    // on a dual-mode device that's usually just the classic BR/EDR audio
    // bearer. The underlying BlueZ Connect() brings up whichever bearer
    // isn't already connected, and LE is what's needed for GATT. This can
    // hang if the adapter is busy servicing an active BR/EDR audio stream
    // (seen with this tool's own RFCOMM path too), hence the timeout.
    tokio::time::timeout(CONNECT_TIMEOUT, peripheral.connect())
        .await
        .context("timed out connecting over BLE - the adapter may be too busy with an active audio stream")?
        .context("failed to connect to the device over BLE")?;
    peripheral
        .discover_services()
        .await
        .context("failed to discover GATT services")?;

    let characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == BATTERY_LEVEL_CHARACTERISTIC_UUID)
        .ok_or_else(|| anyhow!("device does not expose the standard Battery Level (0x2A19) characteristic"))?;

    let data = peripheral
        .read(&characteristic)
        .await
        .context("failed to read the Battery Level characteristic")?;
    let percent = *data.first().ok_or_else(|| anyhow!("empty Battery Level response"))?;

    Ok((name, percent))
}
