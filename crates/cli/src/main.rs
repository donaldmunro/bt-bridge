use std::{collections::HashSet, time::Duration};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use bt_bridge_core::{bluez::{Adapter, Address, BlueZDevice, Session,
                                 device::{characteristic_description, flag_names,
                                          service_description},
                                 enumerate_connected_devices,
                                 enumerate_connected_devices_quiet, init_adapter,
                                 init_session},
                         mdns::format_uuid,
                         split::{SplitConfig, instance_names, start_split_bridge}};
use tokio::signal;
use tracing::{debug, info, warn};

#[derive(Parser, Debug)]
#[command(name = "bt-bridge", about = "Bridges BLE fitness devices to the DIRCON protocol")]
struct Args
{
   /// Base TCP listener port. The first device listens on this port and subsequent services increment the 
   /// last pused port and then listen on this port or the next free port if not available.
   #[arg(long, default_value = "35100")]
   port: u16,

   /// Restrict to one or more BLE devices by address (e.g. AA:BB:CC:DD:EE:FF).
   /// Repeat the flag or pass a comma-separated list.
   #[arg(long, global = true, value_delimiter = ',')]
   device: Vec<String>,

   /// Bridge all connected BLE devices (this is the default; cannot be combined with --device)
   #[arg(long, conflicts_with = "device")]
   all_devices: bool,

   /// Start with the configured --device addresses that are already connected, then keep
   /// retrying the others and restart the bridge when one appears. Intended for the
   /// systemd service.
   #[arg(long, visible_alias = "wait")]
   wait_for_devices: bool,

   /// Automatically reconnect a bridged device that drops its BLE connection, retrying
   /// with backoff until it returns (BlueZ does not reconnect LE devices on its own).
   #[arg(long)]
   reconnect: bool,

   /// Enable verbose (DEBUG) logging
   #[arg(long, short, global = true)]
   verbose: bool,

   /// Emit logs as JSON lines
   #[arg(long)]
   json_log: bool,

   #[command(subcommand)]
   command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command
{
   /// List system Bluez connected fitness devices, then exit. Honours --device
   List
   {
      /// Output in JSON
      #[arg(long)]
      json: bool,
   },
}

#[tokio::main]
async fn main() -> Result<()>
{
   let args = Args::parse();

   if let Some(Command::List { json }) = args.command
   {
      init_list_tracing(args.verbose);
      return list(&args, json).await;
   }

   init_tracing(&args);

   let allowed = parse_allowed(&args)?;
   let wait = args.wait_for_devices;
   let config = bt_bridge_core::config::load()?;
   let reconnect = args.reconnect || config.reconnect;

   let (_session, adapter) = init_session_and_adapter(wait).await?;
   let mut devices = enumerate_connected_devices(&adapter).await?;

   if !allowed.is_empty()
   {
      devices.retain(|d| allowed.contains(&d.addr));
      if devices.is_empty() && !wait
      {
         bail!("no connected BLE device found with addresses {allowed:?}; \
                use --wait-for-devices to wait for them");
      }
   }

   if devices.is_empty() && !wait
   {
      info!("no fitness devices found in initial scan; waiting for a device to connect (Ctrl-C to exit)");
      loop
      {
         tokio::select!
         {
            _ = signal::ctrl_c() =>
            {
               info!("received SIGINT while waiting for devices; exiting");
               return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) =>
            {
               devices = enumerate_connected_devices(&adapter).await?;
               if !devices.is_empty()
               {
                  break;
               }
            }
         }
      }
   }

   serve_split(&args, adapter, devices, reconnect, wait, allowed).await
}

/// Bring up the BlueZ session/adapter. With `wait`, transient failures (Bluetooth/DBus
/// not ready yet at boot) are retried instead of aborting - the systemd service starts
/// before the adapter may be usable.
async fn init_session_and_adapter(wait: bool) -> Result<(Session, Adapter)>
{
   const DELAYS_SECS: &[u64] = &[2, 5, 10, 30];

   let mut attempt = 0usize;
   loop
   {
      let result = async {
         let session = init_session().await?;
         let adapter = init_adapter(&session).await?;
         Ok::<_, anyhow::Error>((session, adapter))
      }.await;

      match result
      {
         | Ok(pair) => return Ok(pair),
         | Err(e) if wait =>
         {
            let delay = DELAYS_SECS[attempt.min(DELAYS_SECS.len() - 1)];
            attempt += 1;
            warn!(err = %e, retry_in_secs = delay,
                  "Bluetooth adapter not ready; retrying");
            tokio::time::sleep(Duration::from_secs(delay)).await;
         }
         | Err(e) => return Err(e),
      }
   }
}

/// One DIRCON service per device, each with its own TCP port and mDNS instance, so
/// training apps list the devices separately.
///
/// In `--wait-for-devices` mode the bridge starts with whatever is available and is
/// restarted when another configured device connects, so that device gets its own
/// service too.
async fn serve_split(args: &Args, adapter: Adapter, mut devices: Vec<BlueZDevice>,
                     reconnect: bool, wait: bool, allowed: Vec<Address>) -> Result<()>
{
   if wait
   {
      devices = bridgeable_devices(devices);
   }

   loop
   {
      if wait && devices.is_empty()
      {
         info!(?allowed,
               "no configured devices connected; waiting for one to appear (Ctrl-C to exit)");
         devices = tokio::select!
         {
            _ = signal::ctrl_c() =>
            {
               info!("received SIGINT while waiting for devices; exiting");
               return Ok(());
            }
            found = wait_for_new_devices(&adapter, &allowed, &HashSet::new()) => found,
         };
      }

      info!(count = devices.len(), "starting one DIRCON service per device");
      let handle = start_split_bridge(&adapter, devices,
                                      SplitConfig { base_port: args.port, reconnect },
                                      None).await?;
      for service in &handle.services
      {
         info!(instance = %service.instance_name, port = service.port, device = %service.addr,
               "DIRCON service ready");
      }

      // Without an explicit --device list there are no "configured" addresses to wait
      // for, so keep the historical behaviour (start once, then wait for Ctrl-C).
      if !wait || allowed.is_empty()
      {
         signal::ctrl_c().await?;
         info!("received SIGINT; shutting down");
         handle.stop().await;
         return Ok(());
      }

      let bridged: HashSet<Address> = handle.services.iter().map(|s| s.addr).collect();
      let missing: Vec<_> = allowed.iter().filter(|a| !bridged.contains(*a)).collect();

      // All configured devices are bridged; no restart is possible until one of them
      // drops, which the per-device monitor/reconnect path handles.
      if missing.is_empty()
      {
         signal::ctrl_c().await?;
         info!("received SIGINT; shutting down");
         handle.stop().await;
         return Ok(());
      }

      info!(missing = ?missing,
            "waiting for configured device(s); the bridge will restart when they connect");
      tokio::select!
      {
         _ = signal::ctrl_c() =>
         {
            info!("received SIGINT; shutting down");
            handle.stop().await;
            return Ok(());
         }
         next = wait_for_new_devices(&adapter, &allowed, &bridged) =>
         {
            let addrs: Vec<_> = next.iter().map(|d| d.addr).collect();
            info!(devices = ?addrs,
                  "new configured device(s) connected; restarting bridge to include them");
            handle.stop().await;
            devices = next;
         }
      }
   }
}

/// How often to poll for configured devices that are not connected/bridged yet.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Upper bound on one best-effort connection attempt so a sleeping device cannot stall
/// the poll loop.
const DEVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll until a configured device that is not in `current` is connected and bridgeable,
/// then return all currently connected/bridgeable configured devices.
async fn wait_for_new_devices(adapter: &Adapter, allowed: &[Address],
                              current: &HashSet<Address>) -> Vec<BlueZDevice>
{
   loop
   {
      try_connect_missing(adapter, allowed, current).await;

      match enumerate_connected_devices_quiet(adapter).await
      {
         | Ok(devices) =>
         {
            let devices = configured_bridgeable_devices(devices, allowed);
            if devices.iter().any(|d| !current.contains(&d.addr))
            {
               return devices;
            }
         }
         | Err(e) => debug!(err = %e, "could not poll for configured BLE devices"),
      }

      tokio::time::sleep(DEVICE_POLL_INTERVAL).await;
   }
}

/// Best-effort wake/connect for configured addresses that are not bridged yet. BlueZ
/// does not initiate LE reconnection itself, so a sleeping device otherwise stays
/// invisible until something calls connect.
async fn try_connect_missing(adapter: &Adapter, allowed: &[Address], current: &HashSet<Address>)
{
   for &addr in allowed
   {
      if current.contains(&addr)
      {
         continue;
      }

      let Ok(device) = adapter.device(addr) else { continue };
      if device.is_connected().await.unwrap_or(false)
      {
         continue;
      }

      match tokio::time::timeout(DEVICE_CONNECT_TIMEOUT, device.connect()).await
      {
         | Ok(Ok(())) => info!(%addr, "connected to configured device"),
         | Ok(Err(e)) => debug!(%addr, err = %e, "connect attempt failed; will retry"),
         | Err(_) =>
         {
            debug!(%addr, timeout = ?DEVICE_CONNECT_TIMEOUT,
                   "connect attempt timed out; will retry");
         }
      }
   }
}

/// Restrict an enumeration result to the configured (`--device`) devices split mode can
/// actually bridge. An empty `allowed` means every bridgeable fitness device.
fn configured_bridgeable_devices(devices: Vec<BlueZDevice>, allowed: &[Address])
                                 -> Vec<BlueZDevice>
{
   let devices: Vec<_> = if allowed.is_empty()
   {
      devices
   }
   else
   {
      devices.into_iter().filter(|d| allowed.contains(&d.addr)).collect()
   };
   bridgeable_devices(devices)
}

/// Drop devices with no recognised primary fitness service - split mode cannot create a
/// service for them, so they must not trigger a restart loop. `instance_names` returns
/// `None` for exactly those devices.
fn bridgeable_devices(devices: Vec<BlueZDevice>) -> Vec<BlueZDevice>
{
   let names = instance_names(devices.iter());
   devices.into_iter()
          .zip(names)
          .filter_map(|(device, name)| name.map(|_| device))
          .collect()
}

/// Enumerate connected fitness devices (same --device handling as the daemon) and
/// report each with the mDNS instance name it would get.
async fn list(args: &Args, json: bool) -> Result<()>
{
   let allowed = parse_allowed(args)?;

   let session = init_session().await?;
   let adapter = init_adapter(&session).await?;
   let mut devices = enumerate_connected_devices(&adapter).await?;
   if !allowed.is_empty()
   {
      devices.retain(|d| allowed.contains(&d.addr));
   }

   // The mDNS instance name each device gets (None: not bridged).
   let instances = instance_names(devices.iter());

   if json
   {
      println!("{}", serde_json::to_string_pretty(&json_report(&devices, &instances))?);
   }
   else
   {
      print_report(&devices, &instances);
   }
   Ok(())
}

fn json_report(devices: &[BlueZDevice], instances: &[Option<String>]) -> serde_json::Value
{
   let devices: Vec<_> =
      devices.iter()
             .zip(instances)
             .map(|(d, instance)| {
                let services: Vec<_> =
                   d.services
                    .iter()
                    .map(|s| {
                       let chars: Vec<_> =
                          s.characteristic_uuids
                           .iter()
                           .map(|u| {
                              let flags = d.characteristics.get(u).map(|c| c.flags).unwrap_or(0);
                              serde_json::json!({ "uuid": format_uuid(*u),
                                                  "description": characteristic_description(*u),
                                                  "flags": flag_names(flags) })
                           })
                           .collect();
                       serde_json::json!({ "uuid": format_uuid(s.uuid),
                                           "description": service_description(s.uuid),
                                           "characteristics": chars })
                    })
                    .collect();
                serde_json::json!({ "address": d.addr.to_string(),
                                    "name": d.name,
                                    "kind": d.kind().prefix(),
                                    "instance": instance,
                                    "services": services })
             })
             .collect();

   serde_json::json!({ "devices": devices })
}

fn print_report(devices: &[BlueZDevice], instances: &[Option<String>])
{
   println!("Connected fitness devices: {}", devices.len());
   for (d, instance) in devices.iter().zip(instances)
   {
      println!();
      println!("{}  {}", d.addr, d.name.as_deref().unwrap_or("(unnamed)"));
      match instance
      {
         | Some(name) => println!("   mDNS instance: {name}"),
         | None => println!("   mDNS instance: (none - no recognised primary fitness service, not bridged)"),
      }
      if d.services.is_empty()
      {
         println!("   (no GATT services - the device may not have resolved services yet)");
      }
      for s in &d.services
      {
         let description = service_description(s.uuid)
                              .map(|name| format!(" ({name})"))
                              .unwrap_or_default();
         println!("   service {}{description}", format_uuid(s.uuid));
         for u in &s.characteristic_uuids
         {
            let flags = d.characteristics.get(u).map(|c| c.flags).unwrap_or(0);
            let label = characteristic_label(*u);
            println!("      {label:<49} [{}]", flag_names(flags).join(","));
         }
      }
   }

}

/// `0x2A63 (Cycling Power Measurement)` when the characteristic name is known,
/// bare `0x2A63` / full vendor UUID otherwise.
fn characteristic_label(uuid: uuid::Uuid) -> String
{
   match characteristic_description(uuid)
   {
      | Some(name) => format!("{} ({name})", format_uuid(uuid)),
      | None => format_uuid(uuid),
   }
}

fn parse_allowed(args: &Args) -> Result<Vec<Address>>
{
   let mut out = Vec::new();
   for s in &args.device
   {
      let addr = s.parse()
                  .map_err(|e| anyhow::anyhow!("invalid --device address {s:?}: {e}"))?;
      if !out.contains(&addr)
      {
         out.push(addr);
      }
   }
   Ok(out)
}

fn init_tracing(args: &Args)
{
   use tracing_subscriber::{EnvFilter, fmt};

   let filter = if args.verbose
   {
      EnvFilter::new("debug")
   }
   else
   {
      EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
   };

   if args.json_log
   {
      fmt().json().with_env_filter(filter).init();
   }
   else
   {
      fmt().with_env_filter(filter).init();
   }
}

/// Tracing for `list`: logs go to stderr so stdout stays clean for the report (vital
/// for --json consumers), and default to warnings only - the report itself is the output.
fn init_list_tracing(verbose: bool)
{
   use tracing_subscriber::{EnvFilter, fmt};

   let filter = if verbose { EnvFilter::new("debug") } else { EnvFilter::new("warn") };
   fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

#[cfg(test)]
mod tests
{
   use super::*;
   use bt_bridge_core::bluez::device::GattService;
   use uuid::Uuid;

   const FTMS: u128 = 0x0000_1826_0000_1000_8000_0080_5f9b_34fb;
   const USER_DATA: u128 = 0x0000_181c_0000_1000_8000_0080_5f9b_34fb;

   fn device(addr_byte: u8, service: u128) -> BlueZDevice
   {
      BlueZDevice { addr: Address::from([0, 0, 0, 0, 0, addr_byte]),
                    name: Some(format!("device-{addr_byte}")),
                    services: vec![GattService { uuid: Uuid::from_u128(service),
                                                  characteristic_uuids: vec![],
                                                  priority: 2 }],
                    characteristics: std::collections::HashMap::new(),
                    is_smart_trainer: service == FTMS }
   }

   #[test]
   fn bridgeable_devices_drops_devices_without_a_primary_fitness_service()
   {
      let devices = bridgeable_devices(vec![device(1, FTMS), device(2, USER_DATA)]);
      assert_eq!(devices.len(), 1);
      assert_eq!(devices[0].addr, Address::from([0, 0, 0, 0, 0, 1]));
   }

   #[test]
   fn configured_bridgeable_devices_filters_to_the_requested_addresses()
   {
      let allowed = [Address::from([0, 0, 0, 0, 0, 1])];
      let devices =
         configured_bridgeable_devices(vec![device(1, FTMS), device(2, FTMS)], &allowed);
      assert_eq!(devices.len(), 1);
      assert_eq!(devices[0].addr, allowed[0]);
   }
}
