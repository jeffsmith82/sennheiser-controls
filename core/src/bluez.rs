//! BlueZ D-Bus device discovery, AVRCP absolute volume, and Battery1 level.
//!
//! Reads and adjusts the volume of a connected Bluetooth headset via BlueZ's
//! AVRCP "absolute volume" support (org.bluez.MediaTransport1.Volume), which
//! is exposed over the D-Bus system bus by bluetoothd.
//!
//! This works for any headset that supports AVRCP absolute volume (most
//! modern Bluetooth headphones do) - it is not specific to a Sennheiser
//! proprietary protocol, since Sennheiser's own headphone/earbud firmware
//! does not expose a separate volume command of its own.

use crate::transport::{self, TransportKind};
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::str::FromStr;
use zbus::fdo;
use zbus::names::InterfaceName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

const BLUEZ_SERVICE: &str = "org.bluez";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const TRANSPORT_IFACE: &str = "org.bluez.MediaTransport1";
const BATTERY_IFACE: &str = "org.bluez.Battery1";
/// AVRCP 1.4 absolute volume is a 7-bit value: 0-127.
pub const MAX_VOLUME: u16 = 127;

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

#[zbus::proxy(
    interface = "org.freedesktop.DBus.ObjectManager",
    default_service = "org.bluez",
    default_path = "/"
)]
trait ObjectManager {
    fn get_managed_objects(&self) -> zbus::Result<ManagedObjects>;
}

fn as_string(value: &OwnedValue) -> Option<String> {
    <&str>::try_from(value).ok().map(|s| s.to_string())
}

fn as_bool(value: &OwnedValue) -> Option<bool> {
    bool::try_from(value).ok()
}

fn as_object_path(value: &OwnedValue) -> Option<OwnedObjectPath> {
    ObjectPath::try_from(&**value).ok().map(OwnedObjectPath::from)
}

fn device_display_name(path: &OwnedObjectPath, device_iface: &HashMap<String, OwnedValue>) -> String {
    device_iface
        .get("Alias")
        .and_then(as_string)
        .or_else(|| device_iface.get("Name").and_then(as_string))
        .unwrap_or_else(|| path.to_string())
}

/// Opens the D-Bus system bus connection used for every BlueZ call.
pub async fn system_bus() -> Result<Connection> {
    Connection::system().await.context("failed to connect to the D-Bus system bus")
}

/// One connected device with an active audio transport: transport path + display name.
pub struct Candidate {
    pub transport_path: OwnedObjectPath,
    pub display_name: String,
}

pub async fn list_candidates(conn: &Connection) -> Result<Vec<Candidate>> {
    let om = ObjectManagerProxy::new(conn).await?;
    let objects = om
        .get_managed_objects()
        .await
        .context("failed to talk to BlueZ over D-Bus - is bluetoothd running?")?;

    let mut candidates = Vec::new();
    for (transport_path, ifaces) in &objects {
        let Some(transport) = ifaces.get(TRANSPORT_IFACE) else {
            continue;
        };
        let Some(device_path) = transport.get("Device").and_then(as_object_path) else {
            continue;
        };
        let Some(device_ifaces) = objects.get(&device_path) else {
            continue;
        };
        let Some(device) = device_ifaces.get(DEVICE_IFACE) else {
            continue;
        };
        let connected = device.get("Connected").and_then(as_bool).unwrap_or(false);
        if !connected {
            continue;
        }
        candidates.push(Candidate {
            transport_path: transport_path.clone(),
            display_name: device_display_name(&device_path, device),
        });
    }
    Ok(candidates)
}

pub async fn find_target(conn: &Connection, filter: Option<&str>) -> Result<Candidate> {
    let mut candidates = list_candidates(conn).await?;
    if let Some(f) = filter {
        let f = f.to_lowercase();
        candidates.retain(|c| {
            c.display_name.to_lowercase().contains(&f)
                || c.transport_path.as_str().to_lowercase().contains(&f)
        });
    }
    candidates.into_iter().next().ok_or_else(|| {
        anyhow!(
            "No connected Bluetooth device with an active AVRCP audio transport was found{}. \
             Make sure the headset is connected and actively streaming audio (or has recently \
             been used for playback), then try again.",
            filter
                .map(|f| format!(" matching '{f}'"))
                .unwrap_or_default()
        )
    })
}

/// A Bluetooth device BlueZ currently shows as connected to this machine.
pub struct ConnectedDevice {
    pub path: OwnedObjectPath,
    pub address: String,
    pub name: String,
}

/// Lists every device BlueZ currently shows as connected to this machine.
/// Standard `org.bluez.Device1.Connected` - not a GAIA/headphone query.
pub async fn list_all_connected_devices(conn: &Connection) -> Result<Vec<ConnectedDevice>> {
    let om = ObjectManagerProxy::new(conn).await?;
    let objects = om
        .get_managed_objects()
        .await
        .context("failed to talk to BlueZ over D-Bus - is bluetoothd running?")?;

    let mut devices = Vec::new();
    for (path, ifaces) in &objects {
        let Some(device) = ifaces.get(DEVICE_IFACE) else {
            continue;
        };
        if !device.get("Connected").and_then(as_bool).unwrap_or(false) {
            continue;
        }
        let name = device_display_name(path, device);
        let address = device.get("Address").and_then(as_string).unwrap_or_default();
        devices.push(ConnectedDevice { path: path.clone(), address, name });
    }
    Ok(devices)
}

/// Finds a connected device by name/address filter, independent of whether
/// it has an active audio transport - used for the GAIA control commands,
/// which don't require audio to be streaming.
pub async fn find_connected_device(conn: &Connection, filter: Option<&str>) -> Result<ConnectedDevice> {
    let mut candidates = list_all_connected_devices(conn).await?;
    if let Some(f) = filter {
        let f = f.to_lowercase();
        candidates.retain(|d| d.name.to_lowercase().contains(&f) || d.address.to_lowercase().contains(&f));
    }
    candidates.into_iter().next().ok_or_else(|| {
        anyhow!(
            "No connected Bluetooth device was found{}",
            filter.map(|f| format!(" matching '{f}'")).unwrap_or_default()
        )
    })
}

async fn properties_proxy<'a>(conn: &Connection, path: &'a OwnedObjectPath) -> Result<fdo::PropertiesProxy<'a>> {
    Ok(fdo::PropertiesProxy::builder(conn)
        .destination(BLUEZ_SERVICE)?
        .path(path)?
        .build()
        .await?)
}

pub async fn get_battery(conn: &Connection, device_path: &OwnedObjectPath) -> Result<u8> {
    let props = properties_proxy(conn, device_path).await?;
    let iface = InterfaceName::try_from(BATTERY_IFACE)?;
    let value = props
        .get(iface, "Percentage")
        .await
        .context("failed to read battery level - the device may not report battery over Bluetooth")?;
    u8::try_from(&value).map_err(|_| anyhow!("Percentage property was not a u8"))
}

pub async fn get_volume(conn: &Connection, transport_path: &OwnedObjectPath) -> Result<u16> {
    let props = properties_proxy(conn, transport_path).await?;
    let iface = InterfaceName::try_from(TRANSPORT_IFACE)?;
    let value = props
        .get(iface, "Volume")
        .await
        .context("failed to read Volume property - the device may not support AVRCP absolute volume")?;
    u16::try_from(&value).map_err(|_| anyhow!("Volume property was not a u16"))
}

pub async fn set_volume(conn: &Connection, transport_path: &OwnedObjectPath, volume: u16) -> Result<()> {
    let volume = volume.min(MAX_VOLUME);
    let props = properties_proxy(conn, transport_path).await?;
    let iface = InterfaceName::try_from(TRANSPORT_IFACE)?;
    props
        .set(iface, "Volume", &Value::from(volume))
        .await
        .context("failed to write Volume property")?;
    Ok(())
}

/// Finds the target device (by filter) and opens a GAIA control channel to it.
pub async fn open_gaia_connection(
    conn: &Connection,
    filter: Option<&str>,
    transport_kind: TransportKind,
) -> Result<(String, transport::GaiaConnection)> {
    let device = find_connected_device(conn, filter).await?;
    let bt_address = bluer::Address::from_str(&device.address)
        .map_err(|e| anyhow!("device address '{}' was not a valid Bluetooth address: {e}", device.address))?;
    let gaia_conn = transport::GaiaConnection::connect(bt_address, transport_kind).await.context(
        "could not open the GAIA control channel - this only works if the device actually \
         exposes GAIA on this transport and runs GAIA-compatible firmware",
    )?;
    Ok((device.name, gaia_conn))
}
