pub mod device;
pub mod manage;
pub mod monitor;

pub use bluer::{Adapter, AdapterEvent, Address, Session};
pub use device::BlueZDevice;
pub use monitor::monitor_devices;
use anyhow::Result;
use device::is_fitness_device;
use tracing::{debug, info, warn};

pub async fn init_session() -> Result<Session> { Ok(Session::new().await?) }

/// The default adapter, powered on. Created once in `main` and shared with the
/// enumerator and the lifecycle monitor.
pub async fn init_adapter(session: &Session) -> Result<Adapter>
{
   let adapter = session.default_adapter().await?;
   adapter.set_powered(true).await?;
   Ok(adapter)
}

pub async fn enumerate_connected_devices(adapter: &Adapter) -> Result<Vec<BlueZDevice>>
{
   enumerate_connected_devices_impl(adapter, true).await
}

/// Like `enumerate_connected_devices`, but suppresses per-device progress logs. Used by
/// polling paths (waiting for configured devices to wake) that would otherwise repeat
/// the same lines every few seconds.
pub async fn enumerate_connected_devices_quiet(adapter: &Adapter) -> Result<Vec<BlueZDevice>>
{
   enumerate_connected_devices_impl(adapter, false).await
}

async fn enumerate_connected_devices_impl(adapter: &Adapter, log_progress: bool)
                                         -> Result<Vec<BlueZDevice>>
{
   let addrs = adapter.device_addresses().await?;
   let mut devices = Vec::new();

   for addr in addrs
   {
      let device = adapter.device(addr)?;

      match device.is_connected().await
      {
         | Ok(true) => {}
         | Ok(false) => continue,
         | Err(e) =>
         {
            if log_progress
            {
               warn!(%addr, err = %e, "could not check connection state");
            }
            else
            {
               debug!(%addr, err = %e, "could not check connection state while polling");
            }
            continue;
         }
      }

      match is_fitness_device(&device).await
      {
         | Ok(false) =>
         {
            if log_progress
            {
               let name = device.name().await.ok().flatten();
               info!(%addr, name = ?name, "skipping non-fitness device");
            }
            continue;
         }
         | Err(e) =>
         {
            if log_progress
            {
               warn!(%addr, err = %e, "could not read device UUIDs");
            }
            else
            {
               debug!(%addr, err = %e, "could not read device UUIDs while polling");
            }
            continue;
         }
         | Ok(true) => {}
      }

      match BlueZDevice::from_bluer_device(device).await
      {
         | Ok(d) =>
         {
            if log_progress
            {
               info!(%addr, name = ?d.name, chars = d.characteristics.len(),
                     "found connected BLE device");
            }
            devices.push(d);
         }
         | Err(e) =>
         {
            if log_progress
            {
               warn!(%addr, err = %e, "failed to enumerate GATT tree");
            }
            else
            {
               debug!(%addr, err = %e, "failed to enumerate GATT tree while polling");
            }
         }
      }
   }

   Ok(devices)
}
