//! BLE connection management for GUI front-ends: scan, connect, trust, pair, forget.
//!
//! The daemon itself never initiates connections (see CLAUDE.md - pairing and
//! connection stay external), but "external" can be the GUI: general-purpose Bluetooth
//! managers handle LE fitness devices badly - KDE's bluedevil-wizard insists on
//! *bonding*, which most fitness devices don't support, so connecting always fails.
//! What works is a plain GATT connect (what `bluetoothctl connect` issues) plus
//! trusting the device; `connect_with_retry` does exactly that and nothing more.
//! `Device::pair()` is only ever called from the explicit `pair` wrapper.

use std::time::Duration;

use anyhow::{Context, Result};
use bluer::{Adapter, AdapterEvent, Address, DiscoveryFilter, DiscoveryTransport};
use futures::Stream;
use tracing::{info, warn};

use super::device::DeviceKind;

/// Attempts made by `connect_with_retry`: LE connects fail sporadically
/// (`le-connection-abort-by-local`, timeouts), and a second or third attempt usually
/// lands.
pub const CONNECT_ATTEMPTS: u32 = 3;
const CONNECT_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Everything a device list row needs, readable for connected *and* merely known
/// (paired/trusted/cached) devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStatus
{
   pub addr:      Address,
   pub name:      Option<String>,
   /// Only present while the device is advertising during a scan.
   pub rssi:      Option<i16>,
   pub paired:    bool,
   pub trusted:   bool,
   pub connected: bool,
   /// From the advertised/cached service UUIDs, so it is known before the first
   /// connect for most devices.
   pub kind:      DeviceKind,
}

/// Start an LE discovery scan. The returned stream yields
/// `AdapterEvent::DeviceAdded`/`DeviceRemoved`; dropping it stops the scan.
pub async fn scan(adapter: &Adapter) -> Result<impl Stream<Item = AdapterEvent> + use<>>
{
   adapter.set_discovery_filter(DiscoveryFilter { transport: DiscoveryTransport::Le,
                                                  ..Default::default() })
          .await
          .context("cannot set LE discovery filter")?;
   adapter.discover_devices().await.context("cannot start discovery")
}

/// Status of every device BlueZ knows about (paired, trusted, cached, or currently
/// discovered) - the GUI's device list.
pub async fn known_devices(adapter: &Adapter) -> Result<Vec<DeviceStatus>>
{
   let mut devices = Vec::new();
   for addr in adapter.device_addresses().await?
   {
      match device_status(adapter, addr).await
      {
         | Ok(status) => devices.push(status),
         | Err(e) => warn!(%addr, err = %e, "cannot read device status"),
      }
   }
   Ok(devices)
}

pub async fn device_status(adapter: &Adapter, addr: Address) -> Result<DeviceStatus>
{
   let device = adapter.device(addr)?;
   let kind = match device.uuids().await
   {
      | Ok(Some(uuids)) => DeviceKind::from_service_uuids(uuids.iter()),
      | _ => DeviceKind::Other,
   };
   Ok(DeviceStatus { addr,
                     name: device.name().await.ok().flatten(),
                     rssi: device.rssi().await.ok().flatten(),
                     paired: device.is_paired().await.unwrap_or(false),
                     trusted: device.is_trusted().await.unwrap_or(false),
                     connected: device.is_connected().await.unwrap_or(false),
                     kind })
}

/// Connect without pairing (the flow that works for LE fitness devices), retrying a
/// few times, then mark the device trusted so BlueZ accepts its future
/// device-initiated reconnects. Never bonds.
///
/// The caller should stop any running discovery first - scanning while connecting
/// interferes on many controllers.
pub async fn connect_with_retry(adapter: &Adapter, addr: Address) -> Result<()>
{
   let device = adapter.device(addr)?;

   if !device.is_connected().await.unwrap_or(false)
   {
      let mut last_err = None;
      for attempt in 1..=CONNECT_ATTEMPTS
      {
         match device.connect().await
         {
            | Ok(()) =>
            {
               last_err = None;
               break;
            }
            | Err(e) =>
            {
               warn!(%addr, attempt, err = %e, "connect attempt failed");
               last_err = Some(e);
               if attempt < CONNECT_ATTEMPTS
               {
                  tokio::time::sleep(CONNECT_RETRY_DELAY).await;
               }
            }
         }
      }
      if let Some(e) = last_err
      {
         return Err(e).with_context(|| format!("cannot connect to {addr} \
                                                after {CONNECT_ATTEMPTS} attempts"));
      }
   }

   if let Err(e) = device.set_trusted(true).await
   {
      // Non-fatal: the connection is up; only auto-reconnect acceptance is affected.
      warn!(%addr, err = %e, "connected, but could not mark the device trusted");
   }
   info!(%addr, "device connected");
   Ok(())
}

pub async fn disconnect(adapter: &Adapter, addr: Address) -> Result<()>
{
   adapter.device(addr)?.disconnect().await.with_context(|| format!("cannot disconnect {addr}"))
}

pub async fn set_trusted(adapter: &Adapter, addr: Address, trusted: bool) -> Result<()>
{
   adapter.device(addr)?
          .set_trusted(trusted)
          .await
          .with_context(|| format!("cannot set trusted={trusted} on {addr}"))
}

/// Explicit bonding, for the rare device that requires it. Uses BlueZ's default agent
/// capabilities ("Just Works" when the device needs no input); there is deliberately
/// no passkey UI.
pub async fn pair(adapter: &Adapter, addr: Address) -> Result<()>
{
   adapter.device(addr)?.pair().await.with_context(|| format!("pairing with {addr} failed"))
}

/// Remove the device from BlueZ entirely (unpair + forget cached data).
pub async fn forget(adapter: &Adapter, addr: Address) -> Result<()>
{
   adapter.remove_device(addr).await.with_context(|| format!("cannot remove device {addr}"))
}
