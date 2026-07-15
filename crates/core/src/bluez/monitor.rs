use std::{collections::HashSet, sync::Arc, time::Duration};

use bluer::{Adapter, AdapterEvent, Address, Device, DeviceEvent, DeviceProperty};
use futures::StreamExt;
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{debug, info, warn};

use crate::{bluez::{BlueZDevice, device::is_fitness_device},
            bridge::{GattIndex, MergedGattIndex},
            dircon::notify::NotifyHub,
            mdns::MdnsResponder};

/// Addresses that already have a `watch_device` task, so the initial device listing
/// and later `DeviceAdded` events never double-watch a device.
type Watched = Arc<Mutex<HashSet<Address>>>;

/// Watch the BlueZ adapter for device lifecycle events and keep the GATT index,
/// notification hub, and mDNS advertisement in sync:
/// - a bridged device disconnecting is removed from routing and its forwarding tasks
///   dropped - DIRCON clients see notifications pause;
/// - any device (hot-plugged, reconnecting, or newly paired via `DeviceAdded`) is
///   bridged when `ServicesResolved(true)` fires: GATT tree enumerated fresh (old D-Bus
///   proxies are stale after reconnect), fitness-filtered, wanted subscriptions re-armed;
/// - whenever the merged service list changes, the mDNS TXT record is re-announced.
///
/// `allowed` restricts monitoring to a single address (`--device`).
///
/// With `reconnect`, a device that was bridged this session and drops its BLE link is
/// actively reconnected (`Device::connect()` with backoff) instead of waiting for the
/// user or an external tool to reconnect it - BlueZ does not initiate LE reconnection
/// on its own. Devices never bridged this session (including fitness devices that were
/// off at startup) are left alone: new connections are left to the user.
pub async fn monitor_devices(adapter: Adapter, index: Arc<MergedGattIndex>, hub: Arc<NotifyHub>,
                             mdns: Option<Arc<MdnsResponder>>, allowed: Option<Address>,
                             reconnect: bool)
{
   let watched: Watched = Arc::new(Mutex::new(HashSet::new()));

   // Subscribe before listing known devices: a device appearing in between is seen by
   // both paths and deduplicated by `watched`, instead of being missed by both.
   let mut events = match adapter.events().await
   {
      | Ok(s) => s,
      | Err(e) =>
      {
         warn!(err = %e, "cannot subscribe to adapter events; device hot-plug disabled");
         return;
      }
   };

   for addr in adapter.device_addresses().await.unwrap_or_default()
   {
      spawn_watcher(&adapter, addr, &index, &hub, &mdns, allowed, reconnect, &watched);
   }

   while let Some(event) = events.next().await
   {
      if let AdapterEvent::DeviceAdded(addr) = event
      {
         spawn_watcher(&adapter, addr, &index, &hub, &mdns, allowed, reconnect, &watched);
      }
      // DeviceRemoved needs no handling here: the removed device's own event stream
      // ends and its watcher cleans up after itself.
   }

   warn!("adapter event stream ended; device lifecycle monitoring stopped");
}

#[allow(clippy::too_many_arguments)]
fn spawn_watcher(adapter: &Adapter, addr: Address, index: &Arc<MergedGattIndex>,
                 hub: &Arc<NotifyHub>, mdns: &Option<Arc<MdnsResponder>>,
                 allowed: Option<Address>, reconnect: bool, watched: &Watched)
{
   if allowed.is_some_and(|a| a != addr)
   {
      return;
   }
   tokio::spawn(watch_device(adapter.clone(), addr, index.clone(), hub.clone(), mdns.clone(),
                             reconnect, watched.clone()));
}

#[allow(clippy::too_many_arguments)]
async fn watch_device(adapter: Adapter, addr: Address, index: Arc<MergedGattIndex>,
                      hub: Arc<NotifyHub>, mdns: Option<Arc<MdnsResponder>>, reconnect: bool,
                      watched: Watched)
{
   if !watched.lock().await.insert(addr)
   {
      return;
   }

   let device = match adapter.device(addr)
   {
      | Ok(d) => d,
      | Err(e) =>
      {
         warn!(%addr, err = %e, "cannot watch device for lifecycle events");
         watched.lock().await.remove(&addr);
         return;
      }
   };
   let mut events = match device.events().await
   {
      | Ok(s) => s,
      | Err(e) =>
      {
         warn!(%addr, err = %e, "cannot subscribe to device property events");
         watched.lock().await.remove(&addr);
         return;
      }
   };

   let mut bridged = index.all_devices().iter().any(|d| d.addr == addr);

   // A device that connected between enumeration and monitor start (or that was
   // already connected when a `DeviceAdded` fired) is bridged now, not on the next
   // property change.
   if !bridged
      && device.is_connected().await.unwrap_or(false)
      && device.is_services_resolved().await.unwrap_or(false)
   {
      bridged = try_bridge(&adapter, addr, &index, &hub, &mdns).await;
   }

   // Bridged at least once this session - the only devices `reconnect` chases.
   let mut wanted = bridged;
   let mut reconnect_task: Option<JoinHandle<()>> = None;

   debug!(%addr, bridged, "lifecycle monitoring started");

   while let Some(DeviceEvent::PropertyChanged(prop)) = events.next().await
   {
      match prop
      {
         | DeviceProperty::Connected(false) =>
         {
            if bridged
            {
               bridged = false;
               on_disconnect(addr, &index, &hub, &mdns).await;
            }
            // Also covers a link that dropped after `connect()` succeeded but before
            // ServicesResolved fired: the old task already exited, start a fresh one.
            if reconnect && wanted
            {
               abort_reconnect(&mut reconnect_task);
               reconnect_task = Some(tokio::spawn(reconnect_loop(device.clone(), addr)));
            }
         }
         // Bridge on ServicesResolved(true), not Connected(true): the GATT tree is not
         // walkable until BlueZ finishes service resolution.
         | DeviceProperty::ServicesResolved(true) if !bridged =>
         {
            bridged = try_bridge(&adapter, addr, &index, &hub, &mdns).await;
            if bridged
            {
               wanted = true;
               abort_reconnect(&mut reconnect_task);
            }
         }
         | _ => {}
      }
   }

   abort_reconnect(&mut reconnect_task);

   // Event stream ended: device removed from BlueZ (unpaired). A later re-pairing
   // raises DeviceAdded, which spawns a fresh watcher.
   if bridged
   {
      warn!(%addr, "device removed from BlueZ; lifecycle monitoring stopped");
      on_disconnect(addr, &index, &hub, &mdns).await;
   }
   else
   {
      debug!(%addr, "device removed from BlueZ; lifecycle monitoring stopped");
   }
   watched.lock().await.remove(&addr);
}

fn abort_reconnect(task: &mut Option<JoinHandle<()>>)
{
   if let Some(task) = task.take()
   {
      task.abort();
   }
}

/// Re-establish the BLE link to a device the bridge was proxying. BLE is
/// central-initiated: the device just advertises when it wakes, and BlueZ will not
/// call Connect on its own. Retries with backoff until the connection is back (the
/// watcher then bridges on ServicesResolved and aborts this task) or the task is
/// aborted (device bridged, removed from BlueZ, or monitor stopped).
async fn reconnect_loop(device: Device, addr: Address)
{
   const DELAYS_SECS: &[u64] = &[2, 5, 10, 20, 30];

   info!(%addr, "device dropped; trying to reconnect");
   for attempt in 0usize..
   {
      let delay = DELAYS_SECS[attempt.min(DELAYS_SECS.len() - 1)];
      tokio::time::sleep(Duration::from_secs(delay)).await;

      // Reconnected by other means (the user, bluetoothctl, ...) while we slept.
      if device.is_connected().await.unwrap_or(false)
      {
         debug!(%addr, "already reconnected; stopping reconnect attempts");
         return;
      }
      match device.connect().await
      {
         | Ok(()) =>
         {
            info!(%addr, attempt, "reconnected");
            return;
         }
         | Err(e) => debug!(%addr, attempt, err = %e, "reconnect attempt failed"),
      }
   }
}

async fn on_disconnect(addr: Address, index: &MergedGattIndex, hub: &NotifyHub,
                       mdns: &Option<Arc<MdnsResponder>>)
{
   if let Some(device) = index.remove_device(addr)
   {
      hub.detach_chars(device.characteristics.keys().copied()).await;
      info!(%addr, name = ?device.name,
            "device disconnected; DIRCON clients will see notifications pause");
      refresh_mdns(mdns, index).await;
   }
}

/// Bridge a device whose GATT tree just became walkable: fitness-filter it, enumerate
/// the tree, add it to routing, re-arm wanted subscriptions, and refresh the mDNS TXT
/// record. Returns whether the device is now bridged.
async fn try_bridge(adapter: &Adapter, addr: Address, index: &MergedGattIndex, hub: &NotifyHub,
                    mdns: &Option<Arc<MdnsResponder>>)
                    -> bool
{
   let device = match adapter.device(addr)
   {
      | Ok(d) => d,
      | Err(e) =>
      {
         warn!(%addr, err = %e, "services resolved but device handle unavailable");
         return false;
      }
   };

   // Hot-plug watches every BlueZ device object, so non-fitness devices (phones,
   // keyboards, ...) land here too; skip them quietly.
   match is_fitness_device(&device).await
   {
      | Ok(true) => {}
      | Ok(false) =>
      {
         debug!(%addr, "ignoring non-fitness device");
         return false;
      }
      | Err(e) =>
      {
         warn!(%addr, err = %e, "could not read device UUIDs");
         return false;
      }
   }

   match BlueZDevice::from_bluer_device(device).await
   {
      | Ok(d) =>
      {
         let device = index.add_device(d);
         let rearmed = hub.rearm_device(&device).await;
         info!(%addr, name = ?device.name, rearmed,
               "device bridged; notification subscriptions re-armed");
         refresh_mdns(mdns, index).await;
         true
      }
      | Err(e) =>
      {
         warn!(%addr, err = %e, "failed to enumerate GATT tree");
         false
      }
   }
}

/// Re-announce the mDNS TXT record for the current device set. `update_devices` skips
/// the announce when the merged service list is unchanged, and can block up to its
/// confirm timeout, so it runs on the blocking pool.
async fn refresh_mdns(mdns: &Option<Arc<MdnsResponder>>, index: &MergedGattIndex)
{
   let Some(mdns) = mdns else { return };
   let mdns = mdns.clone();
   let devices = index.all_devices();
   if let Err(e) = tokio::task::spawn_blocking(move || mdns.update_devices(&devices)).await
   {
      warn!(err = %e, "mDNS refresh task panicked");
   }
}
