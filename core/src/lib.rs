//! Core library for controlling Sennheiser/Sonova Bluetooth headphones:
//! BlueZ D-Bus device discovery, AVRCP volume, GATT/Battery1 battery level,
//! and the reverse-engineered GAIA control protocol (ANC, transparency, EQ,
//! etc). Shared by the `sennheiser-controls` CLI and the Slint GUI.

pub mod battery_ble;
pub mod bluez;
pub mod commands;
pub mod gaia;
pub mod transport;

pub use zbus::Connection;
