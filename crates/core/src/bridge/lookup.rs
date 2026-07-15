use std::{collections::{HashMap, hash_map::Entry},
          sync::{Arc, RwLock}};

use tracing::warn;
use uuid::Uuid;

use crate::bluez::BlueZDevice;

/// Routes a characteristic UUID to the device that owns it.
///
/// In split mode (the only mode) every index holds exactly one device, so routing is
/// trivially unambiguous: the DIRCON wire format carries only the characteristic UUID -
/// no device selector - which is why cross-device sharing of an endpoint was abandoned.
pub trait GattIndex: Send + Sync
{
   fn lookup(&self, uuid: Uuid) -> Option<Arc<BlueZDevice>>;
   /// The device whose characteristic list represents `uuid` in DiscoverChars replies.
   fn device_for_service(&self, uuid: Uuid) -> Option<Arc<BlueZDevice>>;
   fn all_devices(&self) -> Vec<Arc<BlueZDevice>>;
}

struct Inner
{
   devices: Vec<Arc<BlueZDevice>>,
   route:   HashMap<Uuid, Arc<BlueZDevice>>,
   /// Which device answers DiscoverChars for a service UUID.
   service_route: HashMap<Uuid, Arc<BlueZDevice>>,
}

/// GATT index with a precomputed characteristic → device routing table.
///
/// Internally mutable: the device lifecycle monitor removes devices on BLE disconnect
/// and re-adds them (with a fresh GATT tree) on reconnect while the server keeps
/// routing through the read-only `GattIndex` trait.
pub struct MergedGattIndex
{
   inner: RwLock<Inner>,
}

impl MergedGattIndex
{
   pub fn new(devices: Vec<BlueZDevice>) -> Self
   {
      let devices: Vec<Arc<BlueZDevice>> = devices.into_iter().map(Arc::new).collect();
      let (route, service_route) = build_routes(&devices);
      Self { inner: RwLock::new(Inner { devices, route, service_route }) }
   }

   /// Remove a device on BLE disconnect. Returns the removed device, if it was present.
   pub fn remove_device(&self, addr: bluer::Address) -> Option<Arc<BlueZDevice>>
   {
      let mut inner = self.inner.write().unwrap();
      let pos = inner.devices.iter().position(|d| d.addr == addr)?;
      let removed = inner.devices.remove(pos);
      inner.rebuild();
      Some(removed)
   }

   /// Add a device (or replace an existing one with the same address) - BLE reconnect
   /// with a freshly enumerated GATT tree. Returns the stored `Arc`.
   pub fn add_device(&self, device: BlueZDevice) -> Arc<BlueZDevice>
   {
      let device = Arc::new(device);
      let mut inner = self.inner.write().unwrap();
      inner.devices.retain(|d| d.addr != device.addr);
      inner.devices.push(device.clone());
      inner.rebuild();
      device
   }
}

impl Inner
{
   fn rebuild(&mut self)
   {
      let (route, service_route) = build_routes(&self.devices);
      self.route = route;
      self.service_route = service_route;
   }
}

impl GattIndex for MergedGattIndex
{
   fn lookup(&self, uuid: Uuid) -> Option<Arc<BlueZDevice>>
   {
      self.inner.read().unwrap().route.get(&uuid).cloned()
   }

   fn device_for_service(&self, uuid: Uuid) -> Option<Arc<BlueZDevice>>
   {
      self.inner.read().unwrap().service_route.get(&uuid).cloned()
   }

   fn all_devices(&self) -> Vec<Arc<BlueZDevice>> { self.inner.read().unwrap().devices.clone() }
}

/// Build the characteristic → device and service → device routing tables. Uses the
/// per-service characteristic lists (plain data, kept consistent with the proxy map by
/// `from_bluer_device`) rather than the proxy map itself. On a duplicate UUID the
/// first-registered entry wins - with a single device per index (split mode) a
/// duplicate can only come from the device itself and is warned about once.
fn build_routes(devices: &[Arc<BlueZDevice>])
                -> (HashMap<Uuid, Arc<BlueZDevice>>, HashMap<Uuid, Arc<BlueZDevice>>)
{
   let mut route: HashMap<Uuid, Arc<BlueZDevice>> = HashMap::new();
   let mut service_route: HashMap<Uuid, Arc<BlueZDevice>> = HashMap::new();

   for device in devices
   {
      for service in &device.services
      {
         service_route.entry(service.uuid).or_insert_with(|| device.clone());

         for &uuid in &service.characteristic_uuids
         {
            match route.entry(uuid)
            {
               | Entry::Vacant(e) =>
               {
                  e.insert(device.clone());
               }
               | Entry::Occupied(e) =>
               {
                  warn!(char = %uuid, winner = %e.get().addr, shadowed = %device.addr,
                        "duplicate characteristic UUID; routing to the first-registered copy");
               }
            }
         }
      }
   }

   (route, service_route)
}

#[cfg(test)]
mod tests
{
   use super::*;
   use crate::bluez::device::GattService;

   const POWER_MEASUREMENT: u128 = 0x0000_2a63_0000_1000_8000_0080_5f9b_34fb;
   const HEART_RATE_MEASUREMENT: u128 = 0x0000_2a37_0000_1000_8000_0080_5f9b_34fb;

   fn fake_device(addr_byte: u8, char_uuids: &[u128]) -> BlueZDevice
   {
      BlueZDevice { addr:     bluer::Address::from([addr_byte; 6]),
                    name:     None,
                    services: vec![GattService { uuid:       Uuid::from_u128(0x1800),
                                                 characteristic_uuids: char_uuids.iter()
                                                                       .map(|&u| Uuid::from_u128(u))
                                                                       .collect(),
                                                 priority:   0 }],
                    characteristics:    std::collections::HashMap::new(),
                    is_smart_trainer:   false }
   }

   #[test]
   fn lookup_routes_to_the_owning_device()
   {
      let index = MergedGattIndex::new(vec![fake_device(1, &[POWER_MEASUREMENT])]);
      assert_eq!(index.lookup(Uuid::from_u128(POWER_MEASUREMENT)).unwrap().addr,
                 bluer::Address::from([1u8; 6]));
      assert_eq!(index.device_for_service(Uuid::from_u128(0x1800)).unwrap().addr,
                 bluer::Address::from([1u8; 6]));
      assert!(index.lookup(Uuid::from_u128(HEART_RATE_MEASUREMENT)).is_none());
   }

   #[test]
   fn remove_device_stops_routing()
   {
      let index = MergedGattIndex::new(vec![fake_device(1, &[POWER_MEASUREMENT])]);
      let x = Uuid::from_u128(POWER_MEASUREMENT);
      assert!(index.lookup(x).is_some());

      let removed = index.remove_device(bluer::Address::from([1u8; 6]));
      assert!(removed.is_some());
      assert!(index.lookup(x).is_none());
      assert!(index.all_devices().is_empty());

      // Removing an unknown address is a no-op.
      assert!(index.remove_device(bluer::Address::from([9u8; 6])).is_none());
   }

   #[test]
   fn add_device_restores_routing_and_replaces_same_addr()
   {
      let index = MergedGattIndex::new(vec![]);
      let x = Uuid::from_u128(POWER_MEASUREMENT);

      index.add_device(fake_device(1, &[POWER_MEASUREMENT]));
      assert_eq!(index.lookup(x).unwrap().addr, bluer::Address::from([1u8; 6]));

      // Re-adding the same address (reconnect with a fresh GATT tree) replaces, not duplicates.
      index.add_device(fake_device(1, &[HEART_RATE_MEASUREMENT]));
      assert_eq!(index.all_devices().len(), 1);
      assert!(index.lookup(x).is_none());
      assert!(index.lookup(Uuid::from_u128(HEART_RATE_MEASUREMENT)).is_some());
   }
}
