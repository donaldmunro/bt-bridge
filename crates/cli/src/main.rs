use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use bt_bridge_core::{bluez::{Adapter, Address, BlueZDevice,
                                 device::{characteristic_description, flag_names,
                                          service_description},
                                 enumerate_connected_devices, init_adapter, init_session},
                         mdns::format_uuid,
                         split::{SplitConfig, instance_names, start_split_bridge}};
use tokio::signal;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "bt-bridge", about = "Bridges BLE fitness devices to the DIRCON protocol")]
struct Args
{
   /// Base TCP listener port. The first device listens on this port and subsequent services increment the 
   /// last pused port and then listen on this port or the next free port if not available.
   #[arg(long, default_value = "35100")]
   port: u16,

   /// Restrict to a single BLE device by address (e.g. AA:BB:CC:DD:EE:FF)
   #[arg(long, global = true)]
   device: Option<String>,

   /// Bridge all connected BLE devices (this is the default; cannot be combined with --device)
   #[arg(long, conflicts_with = "device")]
   all_devices: bool,

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

   let session = init_session().await?;
   let adapter = init_adapter(&session).await?;
   let mut devices = enumerate_connected_devices(&adapter).await?;

   if let Some(addr) = allowed
   {
      devices.retain(|d| d.addr == addr);
      if devices.is_empty()
      {
         bail!("no connected BLE device found with address {addr}");
      }
   }

   if devices.is_empty()
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
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) =>
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

   let config = bt_bridge_core::config::load()?;
   let reconnect = args.reconnect || config.reconnect;

   serve_split(&args, adapter, devices, reconnect).await
}

/// One DIRCON service per device, each with its own TCP port and mDNS instance, so
/// training apps list the devices separately.
async fn serve_split(args: &Args, adapter: Adapter, devices: Vec<BlueZDevice>, reconnect: bool)
                     -> Result<()>
{
   info!(count = devices.len(), "starting one DIRCON service per device");
   let handle = start_split_bridge(&adapter, devices,
                                   SplitConfig { base_port: args.port, reconnect }, None).await?;
   for service in &handle.services
   {
      info!(instance = %service.instance_name, port = service.port, device = %service.addr,
            "DIRCON service ready");
   }

   signal::ctrl_c().await?;
   info!("received SIGINT; shutting down");
   handle.stop().await;
   Ok(())
}

/// Enumerate connected fitness devices (same --device handling as the daemon) and
/// report each with the mDNS instance name it would get.
async fn list(args: &Args, json: bool) -> Result<()>
{
   let allowed = parse_allowed(args)?;

   let session = init_session().await?;
   let adapter = init_adapter(&session).await?;
   let mut devices = enumerate_connected_devices(&adapter).await?;
   if let Some(addr) = allowed
   {
      devices.retain(|d| d.addr == addr);
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

fn parse_allowed(args: &Args) -> Result<Option<Address>>
{
   match &args.device
   {
      | Some(s) => Ok(Some(s.parse()
                            .map_err(|e| anyhow::anyhow!("invalid --device address {s:?}: {e}"))?)),
      | None => Ok(None),
   }
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
