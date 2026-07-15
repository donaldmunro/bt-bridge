pub mod device;
pub mod manage;
pub mod monitor;

pub use bluer::{Adapter, AdapterEvent, Address};
pub use device::BlueZDevice;
pub use monitor::monitor_devices;
use anyhow::Result;
use bluer::Session;
use device::is_fitness_device;
use tracing::{info, warn};

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
            warn!(%addr, err = %e, "could not check connection state");
            continue;
         }
      }

      match is_fitness_device(&device).await
      {
         | Ok(false) =>
         {
            let name = device.name().await.ok().flatten();
            info!(%addr, name = ?name, "skipping non-fitness device");
            continue;
         }
         | Err(e) =>
         {
            warn!(%addr, err = %e, "could not read device UUIDs");
            continue;
         }
         | Ok(true) => {}
      }

      match BlueZDevice::from_bluer_device(device).await
      {
         | Ok(d) =>
         {
            info!(%addr, name = ?d.name, chars = d.characteristics.len(), "found connected BLE device");
            devices.push(d);
         }
         | Err(e) =>
         {
            warn!(%addr, err = %e, "failed to enumerate GATT tree");
         }
      }
   }

   Ok(devices)
}
