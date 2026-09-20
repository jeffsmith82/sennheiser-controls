//! Opens the GAIA control channel used for everything besides volume (ANC,
//! transparency mode, EQ, etc), over either of two transports:
//!
//! - **Classic Bluetooth (RFCOMM)**: BlueZ doesn't let a client open an
//!   arbitrary RFCOMM channel by UUID directly - you register a `Profile1`
//!   D-Bus object for that UUID, then call `Device1.ConnectProfile`, which
//!   makes BlueZ do the SDP lookup and hand the resulting socket to your
//!   registered profile via a `NewConnection` callback. `bluer::rfcomm`
//!   wraps that dance. Commands are framed with a small SOF/version/flags/
//!   length header (see `gaia::build_frame`/`parse_frame`).
//!
//!   [`GAIA_CLASSIC_UUID`] is confirmed live via an HCI snoop capture: the
//!   SDP response for it literally names the service `"GAIA"`, and the
//!   RFCOMM channel it resolves to is exactly what Sonova's
//!   `com.sonova.chb.control` app uses to control ANC on a real HDB 630. Do
//!   not confuse it with the generic `eb10...` UUID some GAIA devices also
//!   advertise (see [`GAIA_BLE_SERVICE_UUID`]) - on HDB 630, connecting to
//!   *that* one as a classic RFCOMM profile fails with
//!   `br-connection-not-supported`; only [`GAIA_CLASSIC_UUID`] actually works.
//!
//! - **BLE (GATT)**: the generic Qualcomm/CSR GAIA UUID base also shows up
//!   as a GATT service with a write+indicate characteristic. Over BLE
//!   there's no extra framing - you write the raw vendor_id+command_id+
//!   payload packet directly and the response arrives as an indication on
//!   the same characteristic. Unlike the classic UUID above, this path is
//!   unverified end-to-end (BLE connection reliability issues prevented a
//!   live test) - kept in case another device only implements this side.
//!
//! Both transports also carry **unsolicited pushes** - but only once a
//! client asks for them: an HCI snoop capture showed the headset pushing a
//! state snapshot (ANC mode, custom noise-control slider, etc - see
//! `gaia::CMD_SONOVA_CUSTOM_MODE_ACTIVE_NOTIFY`) immediately after the app's
//! connect-time handshake registers for it (see `gaia::CMD_REGISTER_NOTIFICATION`),
//! and again on every later change, including ones made via the headset's
//! own physical buttons with no app involved at all - there is no GET-status
//! opcode for most of these. `commands::register_for_live_status` sends
//! that registration; skipping it (confirmed live) means [`GaiaConnection::subscribe`]
//! never receives anything at all. Each transport runs a background task
//! that owns the raw read side of the connection and fans every parsed
//! frame out two ways: into a queue that [`GaiaConnection::send`] consumes
//! for its own request/response calls, and onto a [`broadcast`] channel
//! that [`GaiaConnection::subscribe`] exposes for anyone who just wants to
//! observe live state.

use anyhow::{anyhow, bail, Context, Result};
use bluer::gatt::remote::Characteristic;
use bluer::rfcomm::{Profile, Stream as RfcommStream};
use bluer::Address;
use futures_util::StreamExt;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::{split, AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::sync::{broadcast, mpsc};

use crate::gaia::{build_frame, build_packet, deframe_one, parse_packet, GaiaResponse};

/// The real, working classic-Bluetooth SDP UUID for the GAIA RFCOMM service,
/// confirmed live against HDB 630 (SDP response names the service "GAIA").
pub const GAIA_CLASSIC_UUID: &str = "a2129ff3-081b-4c45-8afe-469d9c4842ec";

/// Generic Qualcomm/CSR GAIA BLE GATT service UUID (unverified end-to-end -
/// see module docs).
pub const GAIA_BLE_SERVICE_UUID: &str = "0000eb10-d102-11e1-9b23-00025b00a5a5";
/// BLE GATT characteristic (write + indicate) carrying GAIA commands/responses.
pub const GAIA_BLE_CHARACTERISTIC_UUID: &str = "0000eb13-d102-11e1-9b23-00025b00a5a5";

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
/// Bounded so a subscriber that stops polling can't leak memory - just
/// starts missing old notifications (`RecvError::Lagged`), which is fine for
/// a live-status display that only cares about the latest value anyway. 64
/// comfortably covers the initial connect-time snapshot burst (~10-15
/// frames observed live).
const NOTIFICATION_CHANNEL_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum TransportKind {
    /// Classic Bluetooth RFCOMM, framed with the app's SPP header.
    Classic,
    /// BLE GATT write+indicate characteristic, no extra framing.
    Ble,
}

pub enum GaiaConnection {
    Classic {
        write_half: WriteHalf<RfcommStream>,
        /// Every frame the background reader task parses, for `send` to
        /// consume as a request's reply.
        responses: mpsc::UnboundedReceiver<GaiaResponse>,
        /// The same frames, fanned out for live-status subscribers. Kept as
        /// the `Sender` half (not a `Receiver`) so `subscribe` can be called
        /// any number of times.
        notifications: broadcast::Sender<GaiaResponse>,
        /// The `Receiver` paired with `notifications` at construction time -
        /// see its docs on [`GaiaConnection::subscribe`].
        initial_subscription: Option<broadcast::Receiver<GaiaResponse>>,
        /// Aborted on drop - see the [`Drop`] impl below.
        reader_task: tokio::task::JoinHandle<()>,
    },
    Ble {
        characteristic: Characteristic,
        responses: mpsc::UnboundedReceiver<GaiaResponse>,
        notifications: broadcast::Sender<GaiaResponse>,
        initial_subscription: Option<broadcast::Receiver<GaiaResponse>>,
        /// Aborted on drop - see the [`Drop`] impl below.
        reader_task: tokio::task::JoinHandle<()>,
    },
}

impl Drop for GaiaConnection {
    /// The background reader task owns the connection's read side for as
    /// long as it runs, which (for classic RFCOMM) is also what keeps the
    /// underlying socket open: `tokio::io::split` only actually closes the
    /// stream once *both* halves are dropped, and the read half lives
    /// inside the task's future, not in this struct, so just dropping
    /// `self` would otherwise leave the task blocked in `read().await`
    /// forever - and the socket open - even after the app considers this
    /// connection gone (e.g. "Disconnect" in the GUI, or switching
    /// transport/device and opening a new one). Aborting it here is what
    /// actually tears the connection down.
    fn drop(&mut self) {
        match self {
            Self::Classic { reader_task, .. } | Self::Ble { reader_task, .. } => reader_task.abort(),
        }
    }
}

impl GaiaConnection {
    pub async fn connect(address: Address, transport: TransportKind) -> Result<Self> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            match transport {
                TransportKind::Classic => Self::connect_classic(address).await,
                TransportKind::Ble => Self::connect_ble(address).await,
            }
        })
        .await
        .context("timed out opening the GAIA control channel")?
    }

    async fn connect_classic(address: Address) -> Result<Self> {
        let session = bluer::Session::new().await.context("failed to open a BlueZ session")?;
        let adapter = session.default_adapter().await.context("no default Bluetooth adapter")?;
        let device = adapter.device(address).context("device not known to BlueZ")?;

        let uuid = bluer::Uuid::from_str(GAIA_CLASSIC_UUID).expect("hardcoded UUID is valid");

        let mut handle = session
            .register_profile(Profile { uuid, ..Default::default() })
            .await
            .context("failed to register a BlueZ RFCOMM profile for the GAIA control channel")?;

        // Ask BlueZ to connect this profile's UUID on the device. BlueZ
        // performs SDP + RFCOMM connect and calls back into our profile
        // (delivered via `handle`) with the resulting socket. We race that
        // callback against the connect call itself: if BlueZ can't find a
        // matching SDP record (or anything else goes wrong), connect_profile
        // returns an error and no callback ever arrives, so waiting on the
        // callback alone would hang forever.
        let connect_task = tokio::spawn(async move { device.connect_profile(&uuid).await });

        let req = tokio::select! {
            req = handle.next() => {
                req.ok_or_else(|| anyhow!("BlueZ closed the profile registration before connecting"))?
            }
            res = connect_task => {
                match res {
                    Ok(Ok(())) => bail!("BlueZ reported success but never handed us a connection"),
                    Ok(Err(e)) => {
                        return Err(anyhow!(e)).context(
                            "BlueZ could not connect the GAIA RFCOMM channel - the device may not \
                             actually expose an SDP service record for this UUID"
                        )
                    }
                    Err(e) => return Err(anyhow!(e)).context("BlueZ connect task panicked"),
                }
            }
        };
        let stream = req.accept().context("failed to accept the incoming RFCOMM connection")?;
        let (read_half, write_half) = split(stream);

        let (responses_tx, responses) = mpsc::unbounded_channel();
        let (notifications, initial_subscription) = broadcast::channel(NOTIFICATION_CHANNEL_CAPACITY);
        let reader_task = tokio::spawn(Self::run_classic_reader(read_half, responses_tx, notifications.clone()));

        Ok(Self::Classic { write_half, responses, notifications, initial_subscription: Some(initial_subscription), reader_task })
    }

    /// Owns the RFCOMM socket's read half for the lifetime of the
    /// connection: accumulates bytes across reads, peels off complete GAIA
    /// SPP frames as they become available (a frame can arrive split across
    /// multiple socket reads, or several frames can arrive in one read - the
    /// classic transport is a byte stream, not message-framed like BLE), and
    /// fans each one out to both `send`'s response queue and the
    /// notification broadcast. Exits silently once the device closes the
    /// connection or a malformed frame desyncs the buffer - either way,
    /// subsequent `send` calls will simply time out, which already surfaces
    /// as an error there.
    async fn run_classic_reader(
        mut read_half: tokio::io::ReadHalf<RfcommStream>,
        responses_tx: mpsc::UnboundedSender<GaiaResponse>,
        notifications: broadcast::Sender<GaiaResponse>,
    ) {
        let mut buf = Vec::new();
        let mut read_buf = [0u8; 1024];
        loop {
            let n = match read_half.read(&mut read_buf).await {
                Ok(0) | Err(_) => return, // connection closed
                Ok(n) => n,
            };
            buf.extend_from_slice(&read_buf[..n]);

            loop {
                match deframe_one(&buf) {
                    Ok(Some((consumed, response))) => {
                        buf.drain(..consumed);
                        let _ = responses_tx.send(response.clone());
                        let _ = notifications.send(response);
                    }
                    Ok(None) => break, // need more bytes for a full frame
                    Err(_) => {
                        // Malformed/desynced frame - drop everything buffered
                        // so far and try to resync from the next SOF byte
                        // rather than getting stuck retrying the same bytes.
                        buf.clear();
                        break;
                    }
                }
            }
        }
    }

    async fn connect_ble(address: Address) -> Result<Self> {
        let session = bluer::Session::new().await.context("failed to open a BlueZ session")?;
        let adapter = session.default_adapter().await.context("no default Bluetooth adapter")?;
        let device = adapter.device(address).context("device not known to BlueZ")?;

        // Always call connect(), even if the device shows as already
        // "Connected" - on a dual-mode device that's usually just the
        // classic BR/EDR audio bearer. BlueZ's Connect() will bring up
        // whichever bearer (BR/EDR or LE) isn't already connected, and LE
        // is what's needed to resolve GATT services.
        device.connect().await.context("failed to connect to the device over Bluetooth")?;

        let service_uuid = bluer::Uuid::from_str(GAIA_BLE_SERVICE_UUID).expect("hardcoded UUID is valid");
        let char_uuid = bluer::Uuid::from_str(GAIA_BLE_CHARACTERISTIC_UUID).expect("hardcoded UUID is valid");

        let mut found = None;
        for service in device.services().await.context("failed to enumerate GATT services")? {
            if service.uuid().await? != service_uuid {
                continue;
            }
            for characteristic in service.characteristics().await? {
                if characteristic.uuid().await? == char_uuid {
                    found = Some(characteristic);
                    break;
                }
            }
        }
        let characteristic = found.ok_or_else(|| {
            anyhow!(
                "GATT characteristic {GAIA_BLE_CHARACTERISTIC_UUID} not found under service {GAIA_BLE_SERVICE_UUID} \
                 - this device may not expose GAIA over BLE"
            )
        })?;

        let raw_notifications = characteristic
            .notify()
            .await
            .context("failed to subscribe to notifications/indications on the GAIA BLE characteristic")?;

        let (responses_tx, responses) = mpsc::unbounded_channel();
        let (notifications, initial_subscription) = broadcast::channel(NOTIFICATION_CHANNEL_CAPACITY);
        let reader_task = tokio::spawn(Self::run_ble_reader(Box::pin(raw_notifications), responses_tx, notifications.clone()));

        Ok(Self::Ble { characteristic, responses, notifications, initial_subscription: Some(initial_subscription), reader_task })
    }

    /// Owns the GATT notification stream for the lifetime of the connection
    /// and fans each parsed packet out the same way [`Self::run_classic_reader`]
    /// does. Unlike classic RFCOMM, BLE notifications are already
    /// message-framed by the GATT layer (no SOF/length header, no partial
    /// reads to reassemble), so each item is just parsed directly.
    async fn run_ble_reader(
        mut raw_notifications: std::pin::Pin<Box<dyn futures_util::Stream<Item = Vec<u8>> + Send>>,
        responses_tx: mpsc::UnboundedSender<GaiaResponse>,
        notifications: broadcast::Sender<GaiaResponse>,
    ) {
        while let Some(data) = raw_notifications.next().await {
            let Ok(response) = parse_packet(&data) else { continue };
            let _ = responses_tx.send(response.clone());
            let _ = notifications.send(response);
        }
    }

    /// A live feed of every GAIA frame the device sends - both the reply to
    /// whatever `send` call is in flight and every unprompted push
    /// notification (see the module docs - these only start arriving once
    /// `commands::register_for_live_status` has been sent on this
    /// connection). Can be called any number of times.
    ///
    /// The first call returns the receiver that was paired with the
    /// notification channel at connect time (before any registration could
    /// possibly have been sent yet), so it's guaranteed to have every
    /// registration ack and the immediate state-snapshot push each one
    /// triggers sitting in its buffer, regardless of exactly when the caller
    /// gets around to calling this relative to registering. A plain
    /// `notifications.subscribe()` here would not offer that guarantee: a
    /// `broadcast` receiver only observes messages sent after it was
    /// created. Every later call gets a fresh receiver starting from "now".
    pub fn subscribe(&mut self) -> broadcast::Receiver<GaiaResponse> {
        match self {
            Self::Classic { notifications, initial_subscription, .. }
            | Self::Ble { notifications, initial_subscription, .. } => {
                initial_subscription.take().unwrap_or_else(|| notifications.subscribe())
            }
        }
    }

    /// Sends one GAIA command and waits for the device's response.
    ///
    /// Known limitation: GAIA has no request ID to correlate a reply with
    /// the command that triggered it - this (like the real app, going by the
    /// capture) just assumes the next frame to arrive after the write is the
    /// answer. Since the device can also push unprompted notifications at
    /// any time (see module docs), a push arriving in that exact window
    /// could in principle be misread as the reply. To reduce that window,
    /// any frames already queued *before* this call (i.e. unrelated
    /// notifications that arrived while nothing was sending) are discarded
    /// first.
    pub async fn send(&mut self, protocol_version: u8, vendor_id: u16, command_id: u16, payload: &[u8]) -> Result<GaiaResponse> {
        match self {
            Self::Classic { write_half, responses, .. } => {
                while responses.try_recv().is_ok() {}

                let frame = build_frame(protocol_version, vendor_id, command_id, payload)?;
                write_half.write_all(&frame).await.context("failed to write to the RFCOMM socket")?;

                tokio::time::timeout(RESPONSE_TIMEOUT, responses.recv())
                    .await
                    .context("timed out waiting for a response from the headset")?
                    .ok_or_else(|| anyhow!("RFCOMM connection closed by the device before it replied"))
            }
            Self::Ble { characteristic, responses, .. } => {
                while responses.try_recv().is_ok() {}

                let packet = build_packet(vendor_id, command_id, payload);
                characteristic
                    .write(&packet)
                    .await
                    .context("failed to write to the GAIA BLE characteristic")?;

                tokio::time::timeout(RESPONSE_TIMEOUT, responses.recv())
                    .await
                    .context("timed out waiting for a notification/indication response")?
                    .ok_or_else(|| anyhow!("notification stream ended without a response"))
            }
        }
    }
}
