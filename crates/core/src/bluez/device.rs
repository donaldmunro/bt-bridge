use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow};
use bluer::gatt::{CharacteristicFlags, remote::Characteristic};
use futures::StreamExt;
use tokio::{sync::broadcast, task::JoinHandle};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// DIRCON characteristic property flags (DiscoverChars response bitmask).
pub const FLAG_READ: u8 = 0x01;
pub const FLAG_WRITE: u8 = 0x02;
pub const FLAG_NOTIFY: u8 = 0x04;

/// Services used to identify whether a connected BLE device is a fitness device.
/// The daemon proxies ALL services it finds on the device - these UUIDs are only
/// for device-level filtering in `enumerate_connected_devices` and for display
/// names in the `list` command / UI output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitnessService
{
   FitnessMachine,      // 0x1826
   CyclingPower,        // 0x1818
   CyclingSpeedCadence, // 0x1816
   HeartRate,           // 0x180D
   UserData,            // 0x181C  (observed on KICKR)
}

impl FitnessService
{
   pub const ALL: [Self; 5] = [Self::FitnessMachine,
                               Self::CyclingPower,
                               Self::CyclingSpeedCadence,
                               Self::HeartRate,
                               Self::UserData];

   pub const fn uuid(&self) -> uuid::Uuid
   {
      match self
      {
         | Self::FitnessMachine      => uuid::Uuid::from_u128(0x0000_1826_0000_1000_8000_0080_5f9b_34fb),
         | Self::HeartRate           => uuid::Uuid::from_u128(0x0000_180d_0000_1000_8000_0080_5f9b_34fb),
         | Self::CyclingPower        => uuid::Uuid::from_u128(0x0000_1818_0000_1000_8000_0080_5f9b_34fb),
         | Self::CyclingSpeedCadence => uuid::Uuid::from_u128(0x0000_1816_0000_1000_8000_0080_5f9b_34fb),
         | Self::UserData            => uuid::Uuid::from_u128(0x0000_181c_0000_1000_8000_0080_5f9b_34fb),
      }
   }

   /// Bluetooth SIG service name, for the `list` command and UI display.
   pub const fn description(&self) -> &'static str
   {
      match self
      {
         | Self::FitnessMachine      => "Fitness Machine",
         | Self::HeartRate           => "Heart Rate",
         | Self::CyclingPower        => "Cycling Power",
         | Self::CyclingSpeedCadence => "Cycling Speed and Cadence",
         | Self::UserData            => "User Data",
      }
   }

   /// The recognised fitness service matching `uuid`, if any.
   pub fn from_uuid(uuid: Uuid) -> Option<Self>
   {
      Self::ALL.into_iter().find(|s| s.uuid() == uuid)
   }
}

/// Coarse device category, used by the experimental split mode to decide which devices
/// get their own DIRCON service (`Other` is skipped; see `split.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind
{
   SmartTrainer,
   PowerMeter,
   HeartRate,
   CadenceSensor,
   /// No recognised primary fitness service - not bridged in split mode.
   Other,
}

impl DeviceKind
{
   /// Short machine-readable kind tag (the `kind` field of `list --json`).
   pub const fn prefix(&self) -> &'static str
   {
      match self
      {
         | Self::SmartTrainer  => "st",
         | Self::PowerMeter    => "pm",
         | Self::HeartRate     => "hr",
         | Self::CadenceSensor => "cs",
         | Self::Other         => "dev",
      }
   }

   /// Human-readable kind label, for UI display.
   pub const fn description(&self) -> &'static str
   {
      match self
      {
         | Self::SmartTrainer  => "smart trainer",
         | Self::PowerMeter    => "power meter",
         | Self::HeartRate     => "heart rate",
         | Self::CadenceSensor => "speed/cadence",
         | Self::Other         => "other",
      }
   }

   /// Classify from a set of GATT service UUIDs - resolved services or the advertised
   /// UUIDs BlueZ caches for known-but-unconnected devices (`Device::uuids`).
   /// Precedence handles multi-role devices: FTMS beats Cycling Power beats Heart Rate
   /// beats Speed & Cadence, so a trainer that also reports power is a smart trainer
   /// and pedals that also report cadence are a power meter.
   pub fn from_service_uuids<'a, I>(uuids: I) -> Self
      where I: IntoIterator<Item = &'a Uuid>
   {
      let (mut ftms, mut power, mut heart, mut cadence) = (false, false, false, false);
      for uuid in uuids
      {
         match FitnessService::from_uuid(*uuid)
         {
            | Some(FitnessService::FitnessMachine)      => ftms = true,
            | Some(FitnessService::CyclingPower)        => power = true,
            | Some(FitnessService::HeartRate)           => heart = true,
            | Some(FitnessService::CyclingSpeedCadence) => cadence = true,
            | Some(FitnessService::UserData) | None => {}
         }
      }
      if ftms { Self::SmartTrainer }
      else if power { Self::PowerMeter }
      else if heart { Self::HeartRate }
      else if cadence { Self::CadenceSensor }
      else { Self::Other }
   }
}

/// Generic BLE infrastructure services present on virtually every device. They are
/// proxied like everything else but excluded from the mDNS `ble-service-uuids` TXT
/// value (see `mdns.rs`) to respect the 255-byte DNS TXT property limit 
pub const AUXILIARY_SERVICES: &[(u128, &str)] = &[
   (0x0000_1800_0000_1000_8000_0080_5f9b_34fb, "Generic Access"),
   (0x0000_1801_0000_1000_8000_0080_5f9b_34fb, "Generic Attribute"),
   (0x0000_1802_0000_1000_8000_0080_5f9b_34fb, "Immediate Alert"),
   (0x0000_1803_0000_1000_8000_0080_5f9b_34fb, "Link Loss"),
   (0x0000_180a_0000_1000_8000_0080_5f9b_34fb, "Device Information"),
   (0x0000_180f_0000_1000_8000_0080_5f9b_34fb, "Battery Service"),
   (0x0000_181c_0000_1000_8000_0080_5f9b_34fb, "User Data"),
];

/// Well-known GATT characteristic names, for the `list` command / UI display.
/// Cycling-relevant characteristics (Cycling Power, FTMS, Speed & Cadence, Heart Rate)
/// plus the common infrastructure characteristics observed on real devices.
pub const KNOWN_CHARACTERISTICS: &[(u128, &str)] = &[
   // Cycling Power (0x1818)
   (0x0000_2a63_0000_1000_8000_0080_5f9b_34fb, "Cycling Power Measurement"),
   (0x0000_2a64_0000_1000_8000_0080_5f9b_34fb, "Cycling Power Vector"),
   (0x0000_2a65_0000_1000_8000_0080_5f9b_34fb, "Cycling Power Feature"),
   (0x0000_2a66_0000_1000_8000_0080_5f9b_34fb, "Cycling Power Control Point"),
   (0x0000_2a5d_0000_1000_8000_0080_5f9b_34fb, "Sensor Location"),
   // Fitness Machine (0x1826)
   (0x0000_2acc_0000_1000_8000_0080_5f9b_34fb, "Fitness Machine Feature"),
   (0x0000_2ad2_0000_1000_8000_0080_5f9b_34fb, "Indoor Bike Data"),
   (0x0000_2ad3_0000_1000_8000_0080_5f9b_34fb, "Training Status"),
   (0x0000_2ad6_0000_1000_8000_0080_5f9b_34fb, "Supported Resistance Level Range"),
   (0x0000_2ad8_0000_1000_8000_0080_5f9b_34fb, "Supported Power Range"),
   (0x0000_2ad9_0000_1000_8000_0080_5f9b_34fb, "Fitness Machine Control Point"),
   (0x0000_2ada_0000_1000_8000_0080_5f9b_34fb, "Fitness Machine Status"),
   // Cycling Speed and Cadence (0x1816)
   (0x0000_2a5b_0000_1000_8000_0080_5f9b_34fb, "CSC Measurement"),
   (0x0000_2a5c_0000_1000_8000_0080_5f9b_34fb, "CSC Feature"),
   (0x0000_2a55_0000_1000_8000_0080_5f9b_34fb, "SC Control Point"),
   // Heart Rate (0x180D)
   (0x0000_2a37_0000_1000_8000_0080_5f9b_34fb, "Heart Rate Measurement"),
   (0x0000_2a38_0000_1000_8000_0080_5f9b_34fb, "Body Sensor Location"),
   (0x0000_2a39_0000_1000_8000_0080_5f9b_34fb, "Heart Rate Control Point"),
   // Battery (0x180F)
   (0x0000_2a19_0000_1000_8000_0080_5f9b_34fb, "Battery Level"),
   // User Data (0x181C)
   (0x0000_2a98_0000_1000_8000_0080_5f9b_34fb, "Weight"),
   // Device Information (0x180A)
   (0x0000_2a23_0000_1000_8000_0080_5f9b_34fb, "System ID"),
   (0x0000_2a24_0000_1000_8000_0080_5f9b_34fb, "Model Number String"),
   (0x0000_2a25_0000_1000_8000_0080_5f9b_34fb, "Serial Number String"),
   (0x0000_2a26_0000_1000_8000_0080_5f9b_34fb, "Firmware Revision String"),
   (0x0000_2a27_0000_1000_8000_0080_5f9b_34fb, "Hardware Revision String"),
   (0x0000_2a28_0000_1000_8000_0080_5f9b_34fb, "Software Revision String"),
   (0x0000_2a29_0000_1000_8000_0080_5f9b_34fb, "Manufacturer Name String"),
   // Generic Access (0x1800) / Generic Attribute (0x1801)
   (0x0000_2a00_0000_1000_8000_0080_5f9b_34fb, "Device Name"),
   (0x0000_2a01_0000_1000_8000_0080_5f9b_34fb, "Appearance"),
   (0x0000_2a04_0000_1000_8000_0080_5f9b_34fb, "Peripheral Preferred Connection Parameters"),
   (0x0000_2aa6_0000_1000_8000_0080_5f9b_34fb, "Central Address Resolution"),
   (0x0000_2a05_0000_1000_8000_0080_5f9b_34fb, "Service Changed"),
];

/// Human-readable name for a GATT service UUID, when known: recognised fitness
/// services first, then generic infrastructure services. `None` for vendor UUIDs.
pub fn service_description(uuid: Uuid) -> Option<&'static str>
{
   FitnessService::from_uuid(uuid)
      .map(|f| f.description())
      .or_else(|| lookup_name(AUXILIARY_SERVICES, uuid))
}

/// Human-readable name for a GATT characteristic UUID, when known.
/// `None` for vendor UUIDs.
pub fn characteristic_description(uuid: Uuid) -> Option<&'static str>
{
   lookup_name(KNOWN_CHARACTERISTICS, uuid)
}

fn lookup_name(table: &'static [(u128, &str)], uuid: Uuid) -> Option<&'static str>
{
   table.iter().find(|&&(u, _)| Uuid::from_u128(u) == uuid).map(|&(_, name)| name)
}

/// Returns `true` if the device advertises at least one recognised fitness service UUID.
/// Called by `enumerate_connected_devices` as a pre-filter; NOT called inside `from_bluer_device`.
pub async fn is_fitness_device(device: &bluer::Device) -> Result<bool>
{
   if let Some(uuids) = device.uuids().await?
   {
      for svc in FitnessService::ALL
      {
         if uuids.contains(&svc.uuid())
         {
            return Ok(true);
         }
      }
   }
   Ok(false)
}

/// A GATT service and the characteristics it contains.
pub struct GattService
{
   pub uuid: Uuid,
   pub characteristic_uuids: Vec<Uuid>,
   /// Sort priority for the mDNS `ble-service-uuids` TXT value: 2 = fitness,
   /// 1 = other/vendor, 0 = auxiliary infrastructure. Higher sorts first, so TXT
   /// truncation drops the least important services (see `mdns::format_ble_service_uuids`).
   pub priority: u32,
}

/// A characteristic on a proxied device: the bluer D-Bus proxy plus the DIRCON
/// property flags, both captured at enumeration time so DiscoverChars needs no D-Bus round-trip.
pub struct GattCharacteristic
{
   pub uuid:  Uuid,
   pub flags: u8,
   pub proxy: Characteristic,
}

pub struct BlueZDevice
{
   pub addr:     bluer::Address,
   pub name:     Option<String>,
   /// GATT services in discovery order, each with its owned characteristic UUIDs.
   pub services: Vec<GattService>,
   /// Flat UUID → characteristic map for request routing (ReadChar/WriteChar/Subscribe).
   pub characteristics:    HashMap<Uuid, GattCharacteristic>,
   /// True when the device exposes the Fitness Machine service (FTMS, 0x1826) - i.e. it
   /// is a smart trainer (also surfaced as a chip in the GUI device list).
   pub is_smart_trainer: bool,
}

impl BlueZDevice
{
   /// Build a `BlueZDevice` from an already-connected `bluer::Device`.
   /// Walks every service and collects all characteristics unconditionally -
   /// the daemon is a generic GATT proxy and doesn't filter by service type.
   ///
   /// A characteristic UUID repeated across services on one device keeps its first
   /// instance; later ones are skipped with a warning (DIRCON routes by UUID alone,
   /// so only one instance can ever be addressed).
   pub async fn from_bluer_device(device: bluer::Device) -> Result<Self>
   {
      let addr = device.address();
      let name = device.name().await.ok().flatten();
      let mut services = Vec::new();
      let mut characteristics: HashMap<Uuid, GattCharacteristic> = HashMap::new();
      let auxiliaries: HashSet<Uuid> =
         AUXILIARY_SERVICES.iter().map(|&(u, _)| Uuid::from_u128(u)).collect();

      for service in device.services().await?
      {
         let service_uuid = service.uuid().await?;
         let mut characteristic_uuids = Vec::new();

         for ch in service.characteristics().await?
         {
            let uuid = ch.uuid().await?;
            if characteristics.contains_key(&uuid)
            {
               warn!(%addr, char = %uuid, service = %service_uuid,
                     "duplicate characteristic UUID on device; keeping first instance");
               continue;
            }
            let flags = dircon_flags(&ch.flags().await?);
            characteristic_uuids.push(uuid);
            characteristics.insert(uuid, GattCharacteristic { uuid, flags, proxy: ch });
         }

         let priority: u32 = if FitnessService::from_uuid(service_uuid).is_some()
         {
            2
         }
         else if auxiliaries.contains(&service_uuid)
         {
            0
         }
         else
         {
            1
         };
         services.push(GattService { uuid: service_uuid, characteristic_uuids, priority });
      }

      let is_smart_trainer =
         services.iter().any(|s| s.uuid == FitnessService::FitnessMachine.uuid());

      Ok(Self { addr, name, services, characteristics, is_smart_trainer })
   }

   /// Classify the device from its resolved GATT services (see
   /// `DeviceKind::from_service_uuids` for the precedence rules).
   pub fn kind(&self) -> DeviceKind
   {
      DeviceKind::from_service_uuids(self.services.iter().map(|s| &s.uuid))
   }

   pub async fn read(&self, uuid: Uuid) -> Result<Vec<u8>>
   {
      let ch = self.characteristics
                   .get(&uuid)
                   .ok_or_else(|| anyhow!("characteristic {} not found", uuid))?;
      Ok(ch.proxy.read().await?)
   }

   pub async fn write(&self, uuid: Uuid, value: Vec<u8>) -> Result<()>
   {
      let ch = self.characteristics
                   .get(&uuid)
                   .ok_or_else(|| anyhow!("characteristic {} not found", uuid))?;
      Ok(ch.proxy.write(&value).await?)
   }

   /// Subscribes to notifications for `uuid` and forwards each value as `(uuid, bytes)` to `tx`.
   /// Returns the spawned forwarding task handle.
   pub async fn start_notify(&self, uuid: Uuid, tx: broadcast::Sender<(Uuid, Vec<u8>)>) -> Result<JoinHandle<()>>
   {
      let ch = self.characteristics
                   .get(&uuid)
                   .ok_or_else(|| anyhow!("characteristic {} not found", uuid))?;
      let stream = ch.proxy.notify().await?;
      let addr = self.addr;

      let handle = tokio::spawn(async move {
         tokio::pin!(stream);
         while let Some(value) = stream.next().await
         {
            if tx.send((uuid, value)).is_err()
            {
               // No DIRCON clients connected; re-armed by ensure_notify on next subscribe.
               debug!(%addr, char = %uuid, "all notification receivers dropped; forwarding stopped");
               return;
            }
         }
         info!(%addr, char = %uuid,
               "BLE notify stream ended (device dropped?); notification forwarding stopped");
      });

      Ok(handle)
   }
}

/// Names of the DIRCON property flags set in `flags`, for the `list` command / UI display.
pub fn flag_names(flags: u8) -> Vec<&'static str>
{
   let mut names = Vec::new();
   if flags & FLAG_READ != 0
   {
      names.push("read");
   }
   if flags & FLAG_WRITE != 0
   {
      names.push("write");
   }
   if flags & FLAG_NOTIFY != 0
   {
      names.push("notify");
   }
   names
}

/// Map BlueZ characteristic flags onto the 3-bit DIRCON property bitmask.
fn dircon_flags(f: &CharacteristicFlags) -> u8
{
   let mut flags = 0u8;
   if f.read
   {
      flags |= FLAG_READ;
   }
   if f.write || f.write_without_response
   {
      flags |= FLAG_WRITE;
   }
   if f.notify || f.indicate
   {
      flags |= FLAG_NOTIFY;
   }
   flags
}

#[cfg(test)]
mod tests
{
   use super::*;

   fn device_with_services(service_uuids: &[u128]) -> BlueZDevice
   {
      BlueZDevice { addr:     bluer::Address::from([0u8; 6]),
                    name:     None,
                    services: service_uuids.iter()
                                           .map(|&u| GattService { uuid: Uuid::from_u128(u),
                                                                   characteristic_uuids: vec![],
                                                                   priority: 1 })
                                           .collect(),
                    characteristics:  HashMap::new(),
                    is_smart_trainer: false }
   }

   #[test]
   fn device_kind_precedence()
   {
      const FTMS: u128 = 0x0000_1826_0000_1000_8000_0080_5f9b_34fb;
      const CYCLING_POWER: u128 = 0x0000_1818_0000_1000_8000_0080_5f9b_34fb;
      const HEART_RATE: u128 = 0x0000_180d_0000_1000_8000_0080_5f9b_34fb;
      const CSC: u128 = 0x0000_1816_0000_1000_8000_0080_5f9b_34fb;
      const USER_DATA: u128 = 0x0000_181c_0000_1000_8000_0080_5f9b_34fb;

      // A trainer also exposing power/cadence is still a smart trainer.
      let trainer = device_with_services(&[CYCLING_POWER, CSC, FTMS]);
      assert_eq!(trainer.kind(), DeviceKind::SmartTrainer);
      assert_eq!(trainer.kind().prefix(), "st");

      // Pedals: power + cadence, no FTMS.
      let pedals = device_with_services(&[CYCLING_POWER, CSC]);
      assert_eq!(pedals.kind(), DeviceKind::PowerMeter);
      assert_eq!(pedals.kind().prefix(), "pm");

      assert_eq!(device_with_services(&[HEART_RATE]).kind(), DeviceKind::HeartRate);
      assert_eq!(device_with_services(&[CSC]).kind(), DeviceKind::CadenceSensor);

      // A fitness device only via User Data has no primary role.
      assert_eq!(device_with_services(&[USER_DATA]).kind(), DeviceKind::Other);
      assert_eq!(device_with_services(&[]).kind(), DeviceKind::Other);
   }

   #[test]
   fn fitness_service_from_uuid_and_description()
   {
      let ftms = Uuid::from_u128(0x0000_1826_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(FitnessService::from_uuid(ftms), Some(FitnessService::FitnessMachine));
      assert_eq!(FitnessService::FitnessMachine.description(), "Fitness Machine");

      // Generic Access is deliberately not a fitness service.
      let generic_access = Uuid::from_u128(0x0000_1800_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(FitnessService::from_uuid(generic_access), None);
   }

   #[test]
   fn service_description_covers_fitness_and_auxiliary()
   {
      let ftms = Uuid::from_u128(0x0000_1826_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(service_description(ftms), Some("Fitness Machine"));

      let generic_access = Uuid::from_u128(0x0000_1800_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(service_description(generic_access), Some("Generic Access"));

      let battery = Uuid::from_u128(0x0000_180f_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(service_description(battery), Some("Battery Service"));

      let vendor: Uuid = "a026ee0b-0a7d-4ab3-97fa-f1500f9feb8b".parse().unwrap();
      assert_eq!(service_description(vendor), None);
   }

   #[test]
   fn characteristic_description_names_cycling_characteristics()
   {
      let indoor_bike = Uuid::from_u128(0x0000_2ad2_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(characteristic_description(indoor_bike), Some("Indoor Bike Data"));

      let power = Uuid::from_u128(0x0000_2a63_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(characteristic_description(power), Some("Cycling Power Measurement"));

      let vendor: Uuid = "a026e005-0a7d-4ab3-97fa-f1500f9feb8b".parse().unwrap();
      assert_eq!(characteristic_description(vendor), None);
   }
}
