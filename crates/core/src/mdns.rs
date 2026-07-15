use std::{collections::HashSet,
          net::UdpSocket,
          sync::Arc,
          time::{Duration, Instant}};

use anyhow::{Context, Result};
use mdns_sd::{DaemonEvent, Receiver, ServiceDaemon, ServiceInfo};
use tracing::{info, warn};
use uuid::Uuid;

use crate::bluez::BlueZDevice;

const SERVICE_TYPE: &str = "_wahoo-fitness-tnp._tcp.local.";

/// `wahoo-fitness-tnp` is 17 bytes - the real Wahoo hardware violates the RFC 6763
/// 15-byte service name limit that mdns-sd enforces by default. mdns-sd caps the
/// override at 30.
const SERVICE_NAME_LEN_MAX: u8 = 30;

/// How long to wait for the daemon's background thread to confirm (announce) the
/// registration. Validation and the first announcement happen synchronously when the
/// register command is processed, so this is generous.
const REGISTER_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

/// Max bytes allowed for the `ble-service-uuids` TXT value.
/// DNS TXT property limit: key (17) + '=' (1) + value ≤ 255.
const BLE_UUIDS_VALUE_MAX: usize = 255 - 17 - 1;

/// Bluetooth Base UUID suffix bytes (positions 4–15 of a 16-byte UUID).
/// Any UUID matching this suffix with bytes[0..2] == 0x0000 is a 16-bit standard UUID.
const BT_BASE_SUFFIX: [u8; 12] = [0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb];

/// Everything needed to rebuild the `ServiceInfo` except the (mutable) BLE service
/// UUID list - captured once at registration so TXT updates re-announce the same
/// instance/host/IP/port.
struct ServiceTemplate
{
   instance_name: String,
   host_name:     String,
   ip:            String,
   mac:           String,
   port:          u16,
}

/// A shared mDNS service daemon: one daemon can carry any number of service
/// registrations (split mode registers one instance per device on it). Also owns the
/// single daemon monitor channel - `daemon.monitor()` adds a channel to the daemon
/// permanently, so it must be created exactly once. Shuts the daemon down on drop
/// (after all `MdnsResponder`s holding it have unregistered).
pub struct MdnsDaemon
{
   daemon: ServiceDaemon,
   /// Shared by all responders on this daemon; held across each register/confirm
   /// sequence so concurrent responders cannot consume each other's verdicts.
   events: std::sync::Mutex<Receiver<DaemonEvent>>,
}

impl MdnsDaemon
{
   pub fn new() -> Result<Arc<Self>>
   {
      let daemon = ServiceDaemon::new().context("failed to create mDNS daemon")?;
      daemon.set_service_name_len_max(SERVICE_NAME_LEN_MAX)
            .context("failed to raise mDNS service name length limit")?;
      // Subscribe before anything registers so an announce/error event can't be
      // missed. Both commands go through the same queue, so ordering is guaranteed.
      let events = daemon.monitor().context("failed to monitor mDNS daemon")?;
      Ok(Arc::new(Self { daemon, events: std::sync::Mutex::new(events) }))
   }
}

impl Drop for MdnsDaemon
{
   fn drop(&mut self)
   {
      if let Err(e) = self.daemon.shutdown()
      {
         warn!(err = %e, "mDNS daemon shutdown failed");
      }
   }
}

/// One registered service instance; unregisters on drop. The daemon it lives on shuts
/// down when the last `Arc<MdnsDaemon>` drops.
pub struct MdnsResponder
{
   daemon:   Arc<MdnsDaemon>,
   fullname: String,
   template: ServiceTemplate,
   /// Last advertised `ble-service-uuids` value; updates that would not change it are
   /// skipped so same-services reconnects cause no mDNS churn.
   last_uuids: std::sync::Mutex<String>,
}

impl MdnsResponder
{
   /// Advertise one DIRCON service instance on a shared daemon (one instance per
   /// device). `mac` overrides the advertised `mac-address` TXT property - split mode
   /// passes the BLE device address so apps that de-duplicate DIRCON adapters by MAC
   /// see distinct adapters; `None` advertises the host NIC MAC.
   pub fn register_on(daemon: Arc<MdnsDaemon>, devices: &[Arc<BlueZDevice>], port: u16,
                      instance_name: &str, mac: Option<String>)
                      -> Result<Self>
   {
      let template = ServiceTemplate { instance_name: instance_name.to_string(),
                                       host_name:     format!("{}.local.", local_hostname()),
                                       ip:            local_ipv4(),
                                       mac:           mac.unwrap_or_else(host_mac),
                                       port };
      let ble_uuids = format_ble_service_uuids(devices);
      let info = build_service_info(&template, &ble_uuids)?;

      let fullname = info.get_fullname().to_string();
      info!(instance = instance_name, port, ip = %template.ip, host = %template.host_name,
            mac = %template.mac, uuids = %ble_uuids, "registering mDNS service");

      {
         // Hold the monitor channel across register + confirm so a concurrent
         // registration on the same daemon cannot consume this one's verdict.
         let events = daemon.events.lock().unwrap();
         daemon.daemon.register(info).context("mDNS register failed")?;

         // `register` only queues the command; validation happens in the daemon's
         // background thread. Wait for its verdict so a rejection isn't silent.
         confirm_registration(&events, &fullname)?;
      }

      info!(instance = instance_name, port, ip = %template.ip, uuids = %ble_uuids,
            "mDNS service registered");

      Ok(Self { daemon, fullname, template, last_uuids: std::sync::Mutex::new(ble_uuids) })
   }

   /// Re-announce the service when the merged BLE service list changes (device
   /// hot-plug / removal). Re-registering the same fullname replaces the records inside
   /// mdns-sd and sends a fresh announce - no goodbye packet, so clients never see the
   /// service flap. Returns whether a re-announce was issued.
   ///
   /// May block up to `REGISTER_CONFIRM_TIMEOUT` waiting for the daemon's verdict -
   /// call via `spawn_blocking` from async code. Failure is non-fatal (logged): the
   /// stale TXT record still routes clients to the daemon.
   pub fn update_devices(&self, devices: &[Arc<BlueZDevice>]) -> bool
   {
      // All devices dropped: keep the last advertisement rather than blanking the TXT
      // record - the TCP port is still open and the device(s) are expected back.
      if devices.is_empty()
      {
         return false;
      }

      let ble_uuids = format_ble_service_uuids(devices);
      if *self.last_uuids.lock().unwrap() == ble_uuids
      {
         return false;
      }

      // Hold the monitor channel for the whole drain/register/confirm sequence (see
      // `register_on`); drain stale events (e.g. late announces from a previous
      // registration) so confirm_registration sees only this update's verdict.
      let events = self.daemon.events.lock().unwrap();
      while events.try_recv().is_ok() {}

      let info = match build_service_info(&self.template, &ble_uuids)
      {
         | Ok(info) => info,
         | Err(e) =>
         {
            warn!(err = %e, "mDNS TXT update failed to build service info");
            return false;
         }
      };
      if let Err(e) = self.daemon.daemon.register(info)
      {
         warn!(err = %e, "mDNS TXT update register failed");
         return false;
      }
      match confirm_registration(&events, &self.fullname)
      {
         | Ok(()) =>
         {
            info!(uuids = %ble_uuids, "mDNS TXT record updated for changed device set");
            *self.last_uuids.lock().unwrap() = ble_uuids;
            true
         }
         | Err(e) =>
         {
            warn!(err = %e, "mDNS TXT update rejected");
            false
         }
      }
   }
}

fn build_service_info(template: &ServiceTemplate, ble_uuids: &str) -> Result<ServiceInfo>
{
   let properties = [("serial-number", template.instance_name.as_str()),
                     ("mac-address", template.mac.as_str()),
                     ("ble-service-uuids", ble_uuids)];

   ServiceInfo::new(SERVICE_TYPE, &template.instance_name, &template.host_name,
      template.ip.as_str(), template.port, &properties[..])
      .context("failed to build ServiceInfo")
}

/// Block until the mDNS background thread announces `fullname`, reports an error, or
/// `REGISTER_CONFIRM_TIMEOUT` elapses. A timeout is non-fatal (announcements only
/// fire on interfaces that accepted the packet), but is logged loudly.
fn confirm_registration(events: &Receiver<DaemonEvent>, fullname: &str) -> Result<()>
{
   let deadline = Instant::now() + REGISTER_CONFIRM_TIMEOUT;
   loop
   {
      match events.recv_deadline(deadline)
      {
         | Ok(DaemonEvent::Announce(name, addrs)) if name == fullname =>
         {
            info!(%name, %addrs, "mDNS registration announced");
            return Ok(());
         }
         | Ok(DaemonEvent::Error(e)) => anyhow::bail!("mDNS registration rejected: {e}"),
         | Ok(_) => continue,
         | Err(_) =>
         {
            warn!(%fullname, timeout = ?REGISTER_CONFIRM_TIMEOUT,
                  "no mDNS announce confirmation; service may not be discoverable");
            return Ok(());
         }
      }
   }
}

impl Drop for MdnsResponder
{
   fn drop(&mut self)
   {
      if let Err(e) = self.daemon.daemon.unregister(&self.fullname)
      {
         warn!(err = %e, "mDNS unregister failed");
      }
      // The daemon itself shuts down when the last Arc<MdnsDaemon> drops.
   }
}

/// Collect the fitness-relevant service UUIDs from every proxied device and format them as the
/// `ble-service-uuids` TXT value: `0xXXXX` for 16-bit standard UUIDs, full uppercase
/// hyphenated form for 128-bit vendor UUIDs, comma-separated.
///
/// Generic infrastructure services (`bluez::device::AUXILIARY_SERVICES`) are excluded -
/// the real Wahoo KICKR Direct Connect adapter does the same. Entries are ordered by
/// `GattService::priority` (fitness services first), so the last-resort truncation -
/// which removes trailing entries and warns if the value still exceeds 237 bytes -
/// drops the least important services.
fn format_ble_service_uuids(devices: &[Arc<BlueZDevice>]) -> String
{
   let auxiliary: HashSet<Uuid> = crate::bluez::device::AUXILIARY_SERVICES
      .iter()
      .map(|&(u, _)| Uuid::from_u128(u))
      .collect();
   let mut seen = HashSet::new();
   let mut advertised: Vec<(u32, String)> = Vec::new();

   for device in devices
   {
      for service in &device.services
      {
         if !auxiliary.contains(&service.uuid) && seen.insert(service.uuid)
         {
            advertised.push((service.priority, format_uuid(service.uuid)));
         }
      }
   }

   // Stable sort: discovery order is preserved within a priority band.
   advertised.sort_by_key(|&(priority, _)| std::cmp::Reverse(priority));
   let mut value =
      advertised.iter().map(|(_, uuid)| uuid.as_str()).collect::<Vec<&str>>().join(",");

   if value.len() > BLE_UUIDS_VALUE_MAX
   {
      warn!(value = %value, len = value.len(), max = BLE_UUIDS_VALUE_MAX, 
            "ble-service-uuids TXT value too long; truncating trailing entries");
      let mut truncated = Vec::new();
      while value.len() > BLE_UUIDS_VALUE_MAX
      {
         match value.rfind(',')
         {
            | Some(pos) =>
            {
               truncated.push(value[pos + 1..].to_string());
               value.truncate(pos);
            }
            | None =>
            {
               if !value.is_empty()
               {
                  truncated.push(std::mem::take(&mut value));
               }
               value.clear();
               break;
            }
         }
      }
      truncated.reverse();
      warn!(value = %value, len = value.len(), truncated = %truncated.join(","),
            "ble-service-uuids TXT value truncated to fit");
   }

   value
}

/// Format a single UUID in DIRCON TXT-record style: `0xXXXX` for 16-bit standard UUIDs,
/// full uppercase hyphenated form otherwise. Also the display form used by the `list`
/// command.
pub fn format_uuid(uuid: Uuid) -> String
{
   let b = uuid.as_bytes();
   if b[0] == 0x00 && b[1] == 0x00 && b[4..16] == BT_BASE_SUFFIX
   {
      format!("0x{:04X}", u16::from_be_bytes([b[2], b[3]]))
   }
   else
   {
      uuid.hyphenated().to_string().to_uppercase()
   }
}

/// Determine the outbound IPv4 address by opening a connected UDP socket.
/// No packets are sent - the OS route table decides which interface is used.
fn local_ipv4() -> String
{
   UdpSocket::bind("0.0.0.0:0")
      .and_then(|s| {
         s.connect("8.8.8.8:53")?;
         s.local_addr()
      })
      .map(|a| a.ip().to_string())
      .unwrap_or_else(|e| {
         warn!(err = %e,
               "could not determine outbound IPv4; advertising 0.0.0.0 (apps will not find the daemon)");
         "0.0.0.0".to_string()
      })
}

/// Read the system hostname from the kernel.
fn local_hostname() -> String
{
   std::fs::read_to_string("/proc/sys/kernel/hostname")
      .unwrap_or_else(|_| "bt-bridge\n".to_string())
      .trim()
      .to_string()
}

/// Return the MAC address for the TXT record in `XX-XX-XX-XX-XX-XX` form.
///
/// Prefers the interface carrying the default route so the advertised MAC describes
/// the same interface as the advertised IP (`local_ipv4` also follows the default
/// route). Falls back to the first non-loopback interface on multihome-less hosts
/// where `/proc/net/route` is unreadable.
fn host_mac() -> String
{
   if let Some(iface) = default_route_iface()
      && let Some(mac) = iface_mac(&iface)
   {
      return mac;
   }

   let Ok(dir) = std::fs::read_dir("/sys/class/net") else { return "00-00-00-00-00-00".to_string() };

   for entry in dir.flatten()
   {
      let name = entry.file_name();
      let Some(name) = name.to_str() else { continue };
      if name == "lo" { continue; }

      if let Some(mac) = iface_mac(name)
      {
         return mac;
      }
   }

   "00-00-00-00-00-00".to_string()
}

/// Read `/sys/class/net/<iface>/address` and format it as `XX-XX-XX-XX-XX-XX`.
fn iface_mac(iface: &str) -> Option<String>
{
   let mac = std::fs::read_to_string(format!("/sys/class/net/{iface}/address")).ok()?;
   let mac = mac.trim();
   if mac.len() == 17 && mac != "00:00:00:00:00:00"
   {
      Some(mac.replace(':', "-").to_uppercase())
   }
   else
   {
      None
   }
}

/// Name of the interface carrying the IPv4 default route, from `/proc/net/route`
/// (columns: Iface Destination Gateway ...; destination `00000000` = default).
fn default_route_iface() -> Option<String>
{
   let content = std::fs::read_to_string("/proc/net/route").ok()?;
   for line in content.lines().skip(1)
   {
      let mut fields = line.split_whitespace();
      if let (Some(iface), Some(dest)) = (fields.next(), fields.next())
         && dest == "00000000"
      {
         return Some(iface.to_string());
      }
   }
   None
}

#[cfg(test)]
mod tests
{
   use super::*;

   #[test]
   fn format_standard_uuid()
   {
      // 0x1818 = Cycling Power
      let uuid = Uuid::from_u128(0x0000_1818_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(format_uuid(uuid), "0x1818");
   }

   #[test]
   fn format_standard_uuid_1826()
   {
      let uuid = Uuid::from_u128(0x0000_1826_0000_1000_8000_0080_5f9b_34fb);
      assert_eq!(format_uuid(uuid), "0x1826");
   }

   #[test]
   fn format_vendor_uuid()
   {
      // Wahoo FE-C service
      let uuid: Uuid = "a026ee0b-0a7d-4ab3-97fa-f1500f9feb8b".parse().unwrap();
      let s = format_uuid(uuid);
      assert!(s.starts_with("A026EE0B"), "got: {s}");
      assert!(s.contains('-'));
   }

   #[test]
   fn local_ipv4_is_not_loopback()
   {
      let ip = local_ipv4();
      assert_ne!(ip, "0.0.0.0");
      assert!(!ip.starts_with("127."), "expected non-loopback, got {ip}");
   }

   #[test]
   fn auxiliary_services_excluded()
   {
      // Build fake devices with generic + fitness services
      let service = |u: u128| crate::bluez::device::GattService {
         uuid: Uuid::from_u128(u),
         characteristic_uuids: vec![],
         priority: 1,
      };
      let device = Arc::new(crate::bluez::BlueZDevice {
         addr:     bluer::Address::from([0u8; 6]),
         name:     None,
         services: vec![
            service(0x0000_1800_0000_1000_8000_0080_5f9b_34fb), // Generic Access - excluded
            service(0x0000_180a_0000_1000_8000_0080_5f9b_34fb), // Device Information - excluded
            service(0x0000_180f_0000_1000_8000_0080_5f9b_34fb), // Battery - excluded
            service(0x0000_1818_0000_1000_8000_0080_5f9b_34fb), // Cycling Power - included
            service(0x0000_1826_0000_1000_8000_0080_5f9b_34fb), // FTMS - included
         ],
         characteristics:    std::collections::HashMap::new(),
         is_smart_trainer:   false,
      });
      let result = format_ble_service_uuids(&[device]);
      assert_eq!(result, "0x1818,0x1826");
      assert!(result.len() <= BLE_UUIDS_VALUE_MAX);
   }

   /// Live test against a real ServiceDaemon (like `local_ipv4_is_not_loopback`, this
   /// needs a working network stack): an unchanged device set must not re-announce;
   /// a changed one must, updating the remembered TXT value.
   #[test]
   fn update_devices_skips_unchanged_and_reannounces_changed()
   {
      let device = |svcs: &[u128]| {
         Arc::new(crate::bluez::BlueZDevice {
            addr:     bluer::Address::from([0u8; 6]),
            name:     None,
            services: svcs.iter()
                          .map(|&u| crate::bluez::device::GattService {
                             uuid: Uuid::from_u128(u),
                             characteristic_uuids: vec![],
                             priority: 1,
                          })
                          .collect(),
            characteristics: std::collections::HashMap::new(),
            is_smart_trainer: false,
         })
      };
      const CYCLING_POWER: u128 = 0x0000_1818_0000_1000_8000_0080_5f9b_34fb;
      const FTMS: u128 = 0x0000_1826_0000_1000_8000_0080_5f9b_34fb;

      let responder = MdnsResponder::register_on(MdnsDaemon::new().expect("mDNS daemon"),
                                                 &[device(&[CYCLING_POWER])], 45999,
                                                 "bt-bridge-test-hotplug", None)
         .expect("mDNS registration failed");
      assert_eq!(*responder.last_uuids.lock().unwrap(), "0x1818");

      // Same service set → no re-announce.
      assert!(!responder.update_devices(&[device(&[CYCLING_POWER])]));
      assert_eq!(*responder.last_uuids.lock().unwrap(), "0x1818");

      // A second device adds FTMS → TXT re-announced and remembered.
      assert!(responder.update_devices(&[device(&[CYCLING_POWER]), device(&[FTMS])]));
      assert_eq!(*responder.last_uuids.lock().unwrap(), "0x1818,0x1826");
   }

   /// Live test (real ServiceDaemon): two instances registered on one shared daemon
   /// must both confirm, carry their own MAC override, and unregister independently.
   #[test]
   fn shared_daemon_carries_multiple_instances()
   {
      let device = |svc: u128| {
         Arc::new(crate::bluez::BlueZDevice {
            addr:     bluer::Address::from([0u8; 6]),
            name:     None,
            services: vec![crate::bluez::device::GattService {
                              uuid: Uuid::from_u128(svc),
                              characteristic_uuids: vec![],
                              priority: 2,
                           }],
            characteristics:  std::collections::HashMap::new(),
            is_smart_trainer: false,
         })
      };
      const CYCLING_POWER: u128 = 0x0000_1818_0000_1000_8000_0080_5f9b_34fb;
      const HEART_RATE: u128 = 0x0000_180d_0000_1000_8000_0080_5f9b_34fb;

      let daemon = MdnsDaemon::new().expect("mDNS daemon");
      let pm = MdnsResponder::register_on(daemon.clone(), &[device(CYCLING_POWER)], 45901,
                                          "dircon-test pm", Some("F2-B4-F0-14-33-C3".into()))
         .expect("first registration failed");
      let hr = MdnsResponder::register_on(daemon.clone(), &[device(HEART_RATE)], 45902,
                                          "dircon-test hr", Some("AA-BB-CC-DD-EE-FF".into()))
         .expect("second registration on shared daemon failed");

      assert_eq!(*pm.last_uuids.lock().unwrap(), "0x1818");
      assert_eq!(*hr.last_uuids.lock().unwrap(), "0x180D");
      assert_eq!(pm.template.mac, "F2-B4-F0-14-33-C3");
      assert_ne!(pm.fullname, hr.fullname);

      // Dropping one responder must not take the shared daemon down: the survivor can
      // still re-announce.
      drop(pm);
      assert!(hr.update_devices(&[device(HEART_RATE), device(CYCLING_POWER)]));
   }

   #[test]
   fn update_devices_with_empty_list_keeps_last_advertisement()
   {
      let device = Arc::new(crate::bluez::BlueZDevice {
         addr:     bluer::Address::from([0u8; 6]),
         name:     None,
         services: vec![crate::bluez::device::GattService {
                           uuid: Uuid::from_u128(0x0000_1818_0000_1000_8000_0080_5f9b_34fb),
                           characteristic_uuids: vec![],
                           priority: 2,
                        }],
         characteristics:  std::collections::HashMap::new(),
         is_smart_trainer: false,
      });
      let responder = MdnsResponder::register_on(MdnsDaemon::new().expect("mDNS daemon"),
                                                 &[device], 45903, "dircon-test-empty", None)
         .expect("mDNS registration failed");

      // The only device dropped: no re-announce, TXT value retained for its return.
      assert!(!responder.update_devices(&[]));
      assert_eq!(*responder.last_uuids.lock().unwrap(), "0x1818");
   }

   #[test]
   fn value_fits_in_txt_limit_and_truncation_drops_low_priority_first()
   {
      // Priorities as `from_bluer_device` assigns them: fitness 2, vendor 1.
      let service = |u: u128| {
         let uuid = Uuid::from_u128(u);
         crate::bluez::device::GattService {
            uuid,
            characteristic_uuids: vec![],
            priority: if crate::bluez::device::FitnessService::from_uuid(uuid).is_some()
            {
               2
            }
            else
            {
               1
            },
         }
      };
      // Many vendor UUIDs to force the truncation path, discovered *before* the
      // fitness services - priority ordering must still put fitness first.
      let mut services = Vec::new();
      for i in 0u128..20
      {
         services.push(service(0xa026_ee00_0000_0000_0000_0000_0000_0000 | i));
      }
      services.push(service(0x0000_1818_0000_1000_8000_0080_5f9b_34fb)); // Cycling Power
      services.push(service(0x0000_1826_0000_1000_8000_0080_5f9b_34fb)); // FTMS
      let device = Arc::new(crate::bluez::BlueZDevice {
         addr: bluer::Address::from([0u8; 6]),
         name: None,
         services,
         characteristics: std::collections::HashMap::new(),
         is_smart_trainer: false,
      });
      let result = format_ble_service_uuids(&[device]);
      assert!(result.len() <= BLE_UUIDS_VALUE_MAX, "value length {} exceeds {}", result.len(), BLE_UUIDS_VALUE_MAX);
      // Truncation dropped trailing vendor entries, never the fitness services.
      assert!(result.starts_with("0x1818,0x1826,A026EE00"), "got: {result}");
   }
}
