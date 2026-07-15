use std::{collections::{HashMap, HashSet},
          sync::atomic::{AtomicU16, Ordering}};

use anyhow::{Result, anyhow};
use tokio::{sync::{Mutex, broadcast},
            task::JoinHandle};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{bluez::BlueZDevice, bridge::GattIndex};

/// Capacity of the shared notification channel. Sized for bursts across all
/// characteristics; a receiver that falls this far behind is lagging badly and
/// skipping stale fitness samples is the correct recovery.
const CHANNEL_CAPACITY: usize = 256;

struct HubState
{
   /// Every characteristic any client has ever subscribed to,
   /// Survives device disconnects so subscriptions can be re-armed on reconnect.
   wanted: HashSet<Uuid>,
   /// Characteristics with a live BlueZ `StartNotify` forwarding task.
   active: HashMap<Uuid, JoinHandle<()>>,
}

/// Fans BlueZ notifications out to all connected DIRCON clients.
///
/// One hub is shared by every client connection. `StartNotify()` is called at most
/// once per characteristic (as per wireshark capture); the per-characteristic BlueZ
/// forwarding task pushes `(uuid, value)` into a single broadcast channel and each
/// client filters against its own subscription set.
pub struct NotifyHub
{
   tx:    broadcast::Sender<(Uuid, Vec<u8>)>,
   state: Mutex<HubState>,
   /// Global notification sequence counter (wraps at u16::MAX per DIRCON wire format).
   seq:   AtomicU16,
}

impl NotifyHub
{
   pub fn new() -> Self
   {
      let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
      Self { tx,
             state: Mutex::new(HubState { wanted: HashSet::new(), active: HashMap::new() }),
             seq: AtomicU16::new(0) }
   }

   /// A new receiver for one client connection.
   pub fn subscribe_stream(&self) -> broadcast::Receiver<(Uuid, Vec<u8>)> { self.tx.subscribe() }

   /// The shared sender; used by tests to inject values without a BLE device.
   pub fn sender(&self) -> broadcast::Sender<(Uuid, Vec<u8>)> { self.tx.clone() }

   /// Next value of the global notification counter (wrapping).
   pub fn next_seq(&self) -> u16 { self.seq.fetch_add(1, Ordering::Relaxed) }

   /// Ensure a BlueZ `StartNotify` is active for `uuid`. Idempotent: only the first
   /// subscriber (across all clients) triggers the D-Bus call; later ones are no-ops.
   ///
   /// The UUID is recorded in the want-set even when the device is offline or the
   /// D-Bus call fails, so a later reconnect re-arms it. BlueZ `StopNotify` is
   /// deliberately never called on client disconnect - subscriptions stay warm.
   pub async fn ensure_notify(&self, uuid: Uuid, index: &dyn GattIndex) -> Result<()>
   {
      let mut state = self.state.lock().await;
      state.wanted.insert(uuid);

      // A finished handle means the forwarding task died (device dropped): re-arm.
      if let Some(handle) = state.active.get(&uuid)
         && !handle.is_finished()
      {
         return Ok(());
      }

      let device = index.lookup(uuid)
                        .ok_or_else(|| anyhow!("characteristic {uuid} not found on any device"))?;
      let handle = device.start_notify(uuid, self.tx.clone()).await?;
      state.active.insert(uuid, handle);
      info!(char = %uuid, device = %device.addr, "StartNotify armed");
      Ok(())
   }

   /// Drop the active forwarding tasks for the given characteristics (device
   /// disconnected). The want-set is untouched, so `rearm_device` can restore them.
   pub async fn detach_chars(&self, uuids: impl IntoIterator<Item = Uuid>)
   {
      let mut state = self.state.lock().await;
      for uuid in uuids
      {
         if let Some(handle) = state.active.remove(&uuid)
         {
            handle.abort();
         }
      }
   }

   /// Re-arm `StartNotify` on a freshly re-enumerated device for every wanted
   /// characteristic it owns. Returns the number of subscriptions re-armed.
   pub async fn rearm_device(&self, device: &BlueZDevice) -> usize
   {
      let mut state = self.state.lock().await;
      let uuids: Vec<Uuid> = device.characteristics
                                   .keys()
                                   .filter(|u| state.wanted.contains(u))
                                   .copied()
                                   .collect();
      let mut rearmed = 0;
      for uuid in uuids
      {
         match device.start_notify(uuid, self.tx.clone()).await
         {
            | Ok(handle) =>
            {
               if let Some(old) = state.active.insert(uuid, handle)
               {
                  old.abort();
               }
               rearmed += 1;
            }
            | Err(e) => warn!(char = %uuid, device = %device.addr, err = %e,
                              "failed to re-arm notification subscription"),
         }
      }
      rearmed
   }
}

impl Default for NotifyHub
{
   fn default() -> Self { Self::new() }
}

impl Drop for NotifyHub
{
   fn drop(&mut self)
   {
      for (_, handle) in self.state.get_mut().active.drain()
      {
         handle.abort();
      }
   }
}

#[cfg(test)]
mod tests
{
   use super::*;
   use crate::bridge::MergedGattIndex;

   const POWER_MEASUREMENT: u128 = 0x0000_2a63_0000_1000_8000_0080_5f9b_34fb;

   #[test]
   fn seq_counter_increments_and_wraps()
   {
      let hub = NotifyHub::new();
      assert_eq!(hub.next_seq(), 0);
      assert_eq!(hub.next_seq(), 1);

      hub.seq.store(0xFFFF, Ordering::Relaxed);
      assert_eq!(hub.next_seq(), 0xFFFF);
      assert_eq!(hub.next_seq(), 0);
   }

   #[tokio::test]
   async fn ensure_notify_records_wanted_even_when_device_offline()
   {
      let hub = NotifyHub::new();
      let index = MergedGattIndex::new(vec![]);
      let x = Uuid::from_u128(POWER_MEASUREMENT);

      assert!(hub.ensure_notify(x, &index).await.is_err());

      let state = hub.state.lock().await;
      assert!(state.wanted.contains(&x));
      assert!(state.active.is_empty());
   }

   #[tokio::test]
   async fn detach_chars_clears_active_but_keeps_wanted()
   {
      let hub = NotifyHub::new();
      let x = Uuid::from_u128(POWER_MEASUREMENT);

      {
         let mut state = hub.state.lock().await;
         state.wanted.insert(x);
         state.active.insert(x, tokio::spawn(async {}));
      }

      hub.detach_chars([x]).await;

      let state = hub.state.lock().await;
      assert!(state.active.is_empty());
      assert!(state.wanted.contains(&x));
   }
}
