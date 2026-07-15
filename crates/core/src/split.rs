//! Split mode: one DIRCON service per fitness device - own TCP port, own mDNS
//! instance. To a training app the daemon looks like N physical Wahoo Direct Connect
//! adapters on the LAN, so devices appear separately in the pairing screen and
//! cross-device UUID collisions cannot arise: each per-device index holds exactly one
//! device.
//!
//! Instance names are the BLE device names (`KICKR CORE 1234`, `ASSIOMA`, `TICKR`)
//! because apps display the mDNS instance name verbatim in their pairing screens.
//! Services are created for the devices present at startup only; disconnect/reconnect
//! of those devices is handled (each keeps its port and mDNS instance), but a device
//! never seen at startup needs a daemon restart.

use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, bail};
use bluer::Adapter;
use tokio::{net::TcpListener,
            sync::{mpsc::UnboundedSender, watch},
            task::JoinHandle};
use tracing::{info, warn};

use crate::{bluez::{Address, BlueZDevice, device::DeviceKind, monitor_devices},
            bridge::{GattIndex, MergedGattIndex},
            dircon::{notify::NotifyHub,
                     server::{ServeOptions, serve_with}},
            mdns::{MdnsDaemon, MdnsResponder},
            status::StatusEvent};

/// How many ports past `base_port` to probe before giving up (a stray listener on one
/// port must not kill startup).
const PORT_SCAN_RANGE: u16 = 32;

pub struct SplitConfig
{
   /// First TCP port to try; each device's server takes the next free port from here.
   /// Apps learn the actual port from the SRV record, so the numbering is cosmetic.
   pub base_port: u16,
   /// Actively reconnect bridged devices that drop their BLE link (`--reconnect`).
   pub reconnect: bool,
}

impl Default for SplitConfig
{
   fn default() -> Self { Self { base_port: 35100, reconnect: false } }
}

/// One device's running DIRCON service (listener, mDNS instance, lifecycle monitor).
pub struct DeviceService
{
   pub addr: Address,
   pub instance_name: String,
   /// The port actually bound (base port + offset, skipping busy ports).
   pub port: u16,
   /// Single-device index; mutated by this device's lifecycle monitor on
   /// disconnect/reconnect exactly as in merged mode.
   pub index: Arc<MergedGattIndex>,
   pub hub:   Arc<NotifyHub>,
   serve_task:   JoinHandle<()>,
   monitor_task: JoinHandle<()>,
   mdns: Arc<MdnsResponder>,
}

/// All running per-device services. Dropping without `stop` leaves the tasks running
/// (process lifetime); `stop` shuts every service down and unregisters mDNS.
pub struct SplitHandle
{
   pub services: Vec<DeviceService>,
   shutdown: watch::Sender<bool>,
   /// Shared mDNS daemon carrying every instance; shuts down when dropped last.
   daemon: Arc<MdnsDaemon>,
}

/// Start one DIRCON service per recognised device: single-device routing index, own
/// TCP listener, own mDNS instance on a shared daemon, own lifecycle monitor
/// (`monitor_devices` restricted to the device's address). Devices of unrecognised
/// kind are skipped with a warning.
pub async fn start_split_bridge(adapter: &Adapter, devices: Vec<BlueZDevice>,
                                config: SplitConfig,
                                status: Option<UnboundedSender<StatusEvent>>)
                                -> Result<SplitHandle>
{
   let daemon = MdnsDaemon::new()?;
   let (shutdown_tx, shutdown_rx) = watch::channel(false);
   let names = instance_names(devices.iter());
   let mut services = Vec::new();
   let mut next_port = config.base_port;

   for (device, instance_name) in devices.into_iter().zip(names)
   {
      let Some(instance_name) = instance_name
      else
      {
         warn!(addr = %device.addr, name = ?device.name,
               "split mode: no recognised primary fitness service; device not bridged");
         continue;
      };
      let addr = device.addr;

      let index = Arc::new(MergedGattIndex::new(vec![device]));
      let listener = bind_next_free(&mut next_port, config.base_port).await?;
      let port = listener.local_addr()?.port();

      // The BLE device address as mac-address: unique and stable per instance, so apps
      // that de-duplicate DIRCON adapters by MAC see N distinct adapters.
      let mac = addr.to_string().replace(':', "-");
      let mdns = Arc::new(MdnsResponder::register_on(daemon.clone(), &index.all_devices(),
                                                     port, &instance_name, Some(mac))?);

      let hub = Arc::new(NotifyHub::new());
      let monitor_task = tokio::spawn(monitor_devices(adapter.clone(), index.clone(),
                                                      hub.clone(), Some(mdns.clone()),
                                                      Some(addr), config.reconnect));

      let serve_index: Arc<dyn GattIndex> = index.clone();
      let serve_hub = hub.clone();
      let serve_status = status.clone();
      let serve_shutdown = shutdown_rx.clone();
      let serve_instance = instance_name.clone();
      let serve_task = tokio::spawn(async move {
         if let Err(e) = serve_with(listener, serve_index,
                                    ServeOptions { hub:      serve_hub,
                                                   shutdown: Some(serve_shutdown),
                                                   status:   serve_status }).await
         {
            warn!(instance = %serve_instance, err = %e, "DIRCON server exited with error");
         }
      });

      info!(instance = %instance_name, port, device = %addr, "device DIRCON service started");
      services.push(DeviceService { addr, instance_name, port, index, hub,
                                    serve_task, monitor_task, mdns });
   }

   if services.is_empty()
   {
      bail!("split mode: no device with a recognised primary fitness service \
             (smart trainer, power meter, heart rate, cadence)");
   }
   Ok(SplitHandle { services, shutdown: shutdown_tx, daemon })
}

impl SplitHandle
{
   /// Orderly shutdown of every device service: stop accepting, abort in-flight
   /// clients, stop the lifecycle monitors, unregister all mDNS instances, and shut
   /// the shared daemon down.
   pub async fn stop(self)
   {
      let Self { services, shutdown, daemon } = self;
      let _ = shutdown.send(true);

      for service in services
      {
         let DeviceService { instance_name, port, serve_task, monitor_task, mdns, .. } = service;
         if serve_task.await.is_err()
         {
            warn!(instance = %instance_name, "DIRCON server task panicked during shutdown");
         }
         monitor_task.abort();
         let _ = monitor_task.await;

         // Last strong reference (the monitor's clone died with the task): unregisters
         // this device's mDNS instance.
         drop(mdns);
         info!(instance = %instance_name, port, "device DIRCON service stopped");
      }
      drop(daemon);
      info!("split bridge stopped");
   }
}

/// The mDNS instance name each device would get: the BLE device name (apps display it
/// as-is in their pairing screens, so no kind prefix is added), falling back to
/// `"dircon-{last two address octets}"` for unnamed devices; duplicate results get a
/// ` 2`, ` 3`, ... suffix. `None` for `DeviceKind::Other` devices, which split mode
/// does not bridge. Also used by the CLI `list` command to preview the names.
pub fn instance_names<'a, I>(devices: I) -> Vec<Option<String>>
   where I: IntoIterator<Item = &'a BlueZDevice>
{
   let mut taken: HashMap<String, u32> = HashMap::new();
   devices.into_iter()
          .map(|device| {
             if device.kind() == DeviceKind::Other
             {
                return None;
             }
             let base = match device.name.as_deref().map(str::trim)
             {
                | Some(name) if !name.is_empty() => name.to_string(),
                | _ =>
                {
                   let b = device.addr.0;
                   format!("dircon-{:02X}{:02X}", b[4], b[5])
                }
             };
             let n = taken.entry(base.clone()).and_modify(|n| *n += 1).or_insert(1);
             Some(if *n == 1 { base } else { format!("{base} {n}") })
          })
          .collect()
}

/// Bind the first free port at or after `*next_port`, leaving `*next_port` past the
/// bound one. Ports in use are skipped (bounded scan from `base`); other bind errors
/// are fatal.
async fn bind_next_free(next_port: &mut u16, base: u16) -> Result<TcpListener>
{
   loop
   {
      let port = *next_port;
      if port >= base.saturating_add(PORT_SCAN_RANGE)
      {
         bail!("no free TCP port in {base}..{} for a device DIRCON service",
               base.saturating_add(PORT_SCAN_RANGE));
      }
      *next_port += 1;

      match TcpListener::bind(("0.0.0.0", port)).await
      {
         | Ok(listener) => return Ok(listener),
         | Err(e) if e.kind() == std::io::ErrorKind::AddrInUse =>
         {
            warn!(port, "port in use; trying the next one");
         }
         | Err(e) => return Err(e.into()),
      }
   }
}

#[cfg(test)]
mod tests
{
   use super::*;
   use crate::bluez::device::GattService;
   use uuid::Uuid;

   const FTMS: u128 = 0x0000_1826_0000_1000_8000_0080_5f9b_34fb;
   const CYCLING_POWER: u128 = 0x0000_1818_0000_1000_8000_0080_5f9b_34fb;
   const HEART_RATE: u128 = 0x0000_180d_0000_1000_8000_0080_5f9b_34fb;
   const USER_DATA: u128 = 0x0000_181c_0000_1000_8000_0080_5f9b_34fb;

   fn device(addr_byte: u8, name: Option<&str>, service_uuids: &[u128]) -> BlueZDevice
   {
      BlueZDevice { addr:     bluer::Address::from([0, 0, 0, 0, 0x14, addr_byte]),
                    name:     name.map(str::to_string),
                    services: service_uuids.iter()
                                           .map(|&u| GattService { uuid: Uuid::from_u128(u),
                                                                   characteristic_uuids: vec![],
                                                                   priority: 2 })
                                           .collect(),
                    characteristics:  std::collections::HashMap::new(),
                    is_smart_trainer: service_uuids.contains(&FTMS) }
   }

   #[test]
   fn instance_names_are_the_device_names()
   {
      let devices = [device(1, Some("KICKR CORE 1234"), &[FTMS, CYCLING_POWER]),
                     device(2, Some("ASSIOMA"), &[CYCLING_POWER]),
                     device(3, Some("TICKR"), &[HEART_RATE])];
      assert_eq!(instance_names(devices.iter()),
                 vec![Some("KICKR CORE 1234".to_string()),
                      Some("ASSIOMA".to_string()),
                      Some("TICKR".to_string())]);
   }

   #[test]
   fn instance_names_fall_back_to_address_and_disambiguate_duplicates()
   {
      let devices = [device(0xC3, None, &[CYCLING_POWER]),          // unnamed → addr suffix
                     device(2, Some("  ASSIOMA "), &[CYCLING_POWER]), // trimmed
                     device(3, Some("ASSIOMA"), &[CYCLING_POWER]),    // duplicate → " 2"
                     device(4, Some("phone"), &[USER_DATA])];         // Other → skipped
      assert_eq!(instance_names(devices.iter()),
                 vec![Some("dircon-14C3".to_string()),
                      Some("ASSIOMA".to_string()),
                      Some("ASSIOMA 2".to_string()),
                      None]);
   }

   #[tokio::test]
   async fn bind_next_free_skips_an_occupied_port()
   {
      // Occupy an OS-assigned port, then start the scan on it: the blocker must be
      // skipped and the next port bound.
      let blocker = TcpListener::bind(("0.0.0.0", 0)).await.unwrap();
      let base = blocker.local_addr().unwrap().port();

      let mut next_port = base;
      let listener = bind_next_free(&mut next_port, base).await.unwrap();
      let bound = listener.local_addr().unwrap().port();
      assert!(bound > base, "expected a port after the occupied {base}, got {bound}");
      assert_eq!(next_port, bound + 1);
   }
}
