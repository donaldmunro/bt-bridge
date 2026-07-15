//! The tokio-side half of the GUI: owns the bluer session, the discovery scan, and the
//! running `SplitHandle`, executes `Command`s from the UI, and publishes `SharedState`
//! snapshots (requesting a repaint after every write).
//!
//! BlueZ operations that can take seconds (connect retries, pairing) run in spawned
//! tasks - one in flight per device - and report back over an internal `BtEvent`
//! channel so the command loop never blocks.

use std::{collections::HashMap, time::Duration};

use bt_bridge_core::{bluez::{Adapter, AdapterEvent, Address, enumerate_connected_devices,
                                 init_adapter, init_session, manage},
                         config,
                         split::{SplitConfig, SplitHandle, start_split_bridge},
                         status::StatusEvent};
use eframe::egui;
use futures::StreamExt;
use tokio::{sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
            task::JoinHandle};
use tracing::{error, info, warn};

use crate::shared::{BridgedInfo, Command, DaemonState, DeviceRow, Shared};

pub async fn run(shared: Shared, ctx: egui::Context, rx: UnboundedReceiver<Command>)
{
   let (session, adapter) = match async { Ok::<_, anyhow::Error>({
                                     let session = init_session().await?;
                                     let adapter = init_adapter(&session).await?;
                                     (session, adapter)
                                  }) }.await
   {
      | Ok(pair) => pair,
      | Err(e) =>
      {
         error!(err = %e, "cannot connect to BlueZ; is the bluetooth service running?");
         ctx.request_repaint();
         return;
      }
   };
   let _session = session; // keep the D-Bus connection alive for the app lifetime

   let (status_tx, status_rx) = unbounded_channel();
   let (bt_tx, bt_rx) = unbounded_channel();

   // Seed the reconnect toggle from the config file shared with the CLI.
   let reconnect = match config::load()
   {
      | Ok(cfg) => cfg.reconnect,
      | Err(e) =>
      {
         warn!(err = %e, "cannot read config file; ignoring saved settings");
         false
      }
   };
   {
      let mut state = shared.lock().unwrap();
      state.reconnect = reconnect;
      state.reconnect_saved = reconnect;
   }

   let mut controller = Controller { shared,
                                     ctx,
                                     adapter,
                                     status_tx,
                                     bt_tx,
                                     handle: None,
                                     waiting: false,
                                     scan_task: None,
                                     busy: HashMap::new(),
                                     reconnect };
   controller.refresh_rows().await;
   controller.run(rx, status_rx, bt_rx).await;
}

/// Completion report of a spawned BlueZ operation.
struct BtEvent
{
   addr:  Address,
   op:    &'static str,
   error: Option<String>,
}

struct Controller
{
   shared:    Shared,
   ctx:       egui::Context,
   adapter:   Adapter,
   status_tx: UnboundedSender<StatusEvent>,
   bt_tx:     UnboundedSender<BtEvent>,
   handle:    Option<SplitHandle>,
   /// Start was requested but no fitness device was connected yet.
   waiting:   bool,
   /// The discovery scan; aborting drops the event stream, which stops discovery.
   scan_task: Option<JoinHandle<()>>,
   /// BlueZ operations in flight, one per device (label shown in the row).
   busy:      HashMap<Address, &'static str>,
   /// Active reconnect for the next bridge start (seeded from the config file,
   /// toggled by the UI, persisted on `SaveReconnect`).
   reconnect: bool,
}

impl Controller
{
   async fn run(&mut self, mut rx: UnboundedReceiver<Command>,
                mut status_rx: UnboundedReceiver<StatusEvent>,
                mut bt_rx: UnboundedReceiver<BtEvent>)
   {
      let mut tick = tokio::time::interval(Duration::from_secs(3));
      tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
      loop
      {
         tokio::select!
         {
            command = rx.recv() =>
            {
               match command
               {
                  | Some(command) => self.handle_command(command).await,
                  | None => return, // UI gone
               }
            }
            // Cannot end: we hold senders for the lifetime of the controller.
            Some(event) = status_rx.recv() => self.handle_status(event),
            Some(event) = bt_rx.recv() => self.handle_bt_event(event).await,
            _ = tick.tick() => self.tick().await,
         }
      }
   }

   async fn handle_command(&mut self, command: Command)
   {
      match command
      {
         | Command::Refresh => self.refresh_rows().await,
         | Command::Start => self.start().await,
         | Command::Stop => self.stop().await,
         | Command::RestartBridge => self.restart().await,
         | Command::StartScan => self.start_scan().await,
         | Command::StopScan => self.stop_scan(),
         | Command::SetReconnect(value) =>
         {
            self.reconnect = value;
            self.shared.lock().unwrap().reconnect = value;
            if self.handle.is_some() || self.waiting
            {
               info!(reconnect = value, "reconnect setting applies at the next bridge start");
            }
            self.ctx.request_repaint();
         }
         | Command::SaveReconnect =>
         {
            let mut cfg = self.config();
            cfg.reconnect = self.reconnect;
            match config::save(&cfg)
            {
               | Ok(()) =>
               {
                  info!(reconnect = self.reconnect, "reconnect setting saved to the config file");
                  self.shared.lock().unwrap().reconnect_saved = self.reconnect;
               }
               | Err(e) => error!(err = %e, "cannot save the config file"),
            }
            self.ctx.request_repaint();
         }
         | Command::Connect(addr) =>
         {
            // Scanning while connecting interferes on many controllers.
            self.stop_scan();
            self.spawn_op(addr, "connecting…", |adapter, addr| async move {
               manage::connect_with_retry(&adapter, addr).await
            });
         }
         | Command::Disconnect(addr) =>
         {
            self.spawn_op(addr, "disconnecting…", |adapter, addr| async move {
               manage::disconnect(&adapter, addr).await
            });
         }
         | Command::SetTrusted(addr, trusted) =>
         {
            self.spawn_op(addr, "updating trust…", move |adapter, addr| async move {
               manage::set_trusted(&adapter, addr, trusted).await
            });
         }
         | Command::Pair(addr) =>
         {
            self.stop_scan();
            self.spawn_op(addr, "pairing…", |adapter, addr| async move {
               manage::pair(&adapter, addr).await
            });
         }
         | Command::Forget(addr) =>
         {
            self.spawn_op(addr, "removing…", |adapter, addr| async move {
               manage::forget(&adapter, addr).await
            });
         }
      }
   }

   /// Run one BlueZ operation for `addr` in the background; completion arrives as a
   /// `BtEvent`. At most one operation per device is in flight.
   fn spawn_op<F, Fut>(&mut self, addr: Address, label: &'static str, op: F)
      where F: FnOnce(Adapter, Address) -> Fut + Send + 'static,
            Fut: Future<Output = anyhow::Result<()>> + Send
   {
      if self.busy.contains_key(&addr)
      {
         return;
      }
      self.busy.insert(addr, label);
      self.publish_rows_sync();

      let adapter = self.adapter.clone();
      let tx = self.bt_tx.clone();
      tokio::spawn(async move {
         let error = op(adapter, addr).await.err().map(|e| format!("{e:#}"));
         let _ = tx.send(BtEvent { addr, op: label, error });
      });
   }

   async fn handle_bt_event(&mut self, event: BtEvent)
   {
      self.busy.remove(&event.addr);
      match &event.error
      {
         | Some(e) => error!(addr = %event.addr, op = event.op, "{e}"),
         | None => info!(addr = %event.addr, op = event.op, "done"),
      }

      // A device connected while the bridge runs: its DIRCON service is missing
      // (split-mode services are created at start). Restart transparently while no
      // training app is attached; otherwise let the user decide.
      if event.op == "connecting…" && event.error.is_none() && self.handle.is_some()
         && !self.is_bridged(event.addr)
      {
         let clients = self.shared.lock().unwrap().clients;
         if clients == 0
         {
            info!(addr = %event.addr, "restarting the bridge to include the new device");
            self.restart().await;
         }
         else
         {
            self.shared.lock().unwrap().restart_offer = true;
         }
      }

      self.refresh_rows().await;
   }

   fn is_bridged(&self, addr: Address) -> bool
   {
      self.handle
          .as_ref()
          .is_some_and(|h| h.services.iter().any(|s| s.addr == addr))
   }

   async fn start_scan(&mut self)
   {
      if self.scan_task.is_some()
      {
         return;
      }
      let stream = match manage::scan(&self.adapter).await
      {
         | Ok(stream) => stream,
         | Err(e) =>
         {
            error!(err = %e, "cannot start Bluetooth scan");
            return;
         }
      };
      info!("Bluetooth LE scan started");

      // Forward discovery events as no-op BtEvents: each one triggers a row refresh,
      // so found devices appear as they advertise.
      let tx = self.bt_tx.clone();
      self.scan_task = Some(tokio::spawn(async move {
         tokio::pin!(stream);
         while let Some(event) = stream.next().await
         {
            if let AdapterEvent::DeviceAdded(addr) | AdapterEvent::DeviceRemoved(addr) = event
            {
               let _ = tx.send(BtEvent { addr, op: "scan", error: None });
            }
         }
      }));
      self.shared.lock().unwrap().scanning = true;
      self.refresh_rows().await;
   }

   fn stop_scan(&mut self)
   {
      if let Some(task) = self.scan_task.take()
      {
         // Aborting drops the discovery stream, which stops the BlueZ scan.
         task.abort();
         info!("Bluetooth LE scan stopped");
      }
      self.shared.lock().unwrap().scanning = false;
      self.ctx.request_repaint();
   }

   fn handle_status(&mut self, event: StatusEvent)
   {
      {
         let mut state = self.shared.lock().unwrap();
         match event
         {
            | StatusEvent::ClientConnected { .. } => state.clients += 1,
            | StatusEvent::ClientDisconnected { .. } =>
            {
               state.clients = state.clients.saturating_sub(1);
            }
            | _ => {}
         }
      }
      self.ctx.request_repaint();
   }

   async fn tick(&mut self)
   {
      if self.waiting
      {
         match enumerate_connected_devices(&self.adapter).await
         {
            | Ok(devices) if !devices.is_empty() =>
            {
               self.waiting = false;
               self.start_with(devices).await;
            }
            | Ok(_) => {}
            | Err(e) => warn!(err = %e, "device enumeration failed while waiting"),
         }
      }
      // Keeps RSSI fresh while scanning and picks up state changed behind our back
      // (bluetoothctl, the lifecycle monitor, devices powering off).
      self.refresh_rows().await;
   }

   async fn start(&mut self)
   {
      if self.handle.is_some() || self.waiting
      {
         return;
      }
      self.set_state(DaemonState::Starting);
      match enumerate_connected_devices(&self.adapter).await
      {
         | Ok(devices) if devices.is_empty() =>
         {
            info!("no fitness devices connected; waiting for one to appear");
            self.waiting = true;
            self.set_state(DaemonState::WaitingForDevices);
         }
         | Ok(devices) => self.start_with(devices).await,
         | Err(e) =>
         {
            error!(err = %e, "device enumeration failed");
            self.set_state(DaemonState::Stopped);
         }
      }
   }

   async fn start_with(&mut self, devices: Vec<bt_bridge_core::bluez::BlueZDevice>)
   {
      let config = SplitConfig { reconnect: self.reconnect,
                                 ..SplitConfig::default() };
      match start_split_bridge(&self.adapter, devices, config,
                               Some(self.status_tx.clone())).await
      {
         | Ok(handle) =>
         {
            self.shared.lock().unwrap().services = handle.services.len();
            self.handle = Some(handle);
            self.set_state(DaemonState::Running);
            self.refresh_rows().await;
         }
         | Err(e) =>
         {
            error!(err = %e, "failed to start the bridge");
            self.set_state(DaemonState::Stopped);
         }
      }
   }

   async fn stop(&mut self)
   {
      self.waiting = false;
      if let Some(handle) = self.handle.take()
      {
         self.set_state(DaemonState::Stopping);
         handle.stop().await;
      }
      {
         let mut state = self.shared.lock().unwrap();
         state.daemon_state = DaemonState::Stopped;
         state.clients = 0;
         state.services = 0;
         state.restart_offer = false;
      }
      self.refresh_rows().await;
   }

   /// Stop and start the bridge so devices connected since Start get their own
   /// DIRCON service.
   async fn restart(&mut self)
   {
      if self.handle.is_none()
      {
         return;
      }
      self.stop().await;
      self.start().await;
   }

   /// The shared config file, or the default when unreadable (the GUI has no CLI flags).
   fn config(&self) -> config::Config
   {
      match config::load()
      {
         | Ok(cfg) => cfg,
         | Err(e) =>
         {
            warn!(err = %e, "cannot read config file; ignoring saved settings");
            config::Config::default()
         }
      }
   }

   fn set_state(&self, daemon_state: DaemonState)
   {
      self.shared.lock().unwrap().daemon_state = daemon_state;
      self.ctx.request_repaint();
   }

   /// Re-read every known device from BlueZ and publish the row list.
   async fn refresh_rows(&self)
   {
      let statuses = match manage::known_devices(&self.adapter).await
      {
         | Ok(statuses) => statuses,
         | Err(e) =>
         {
            warn!(err = %e, "cannot read Bluetooth device list");
            return;
         }
      };

      let bridged: HashMap<Address, BridgedInfo> =
         self.handle
             .as_ref()
             .map(|h| {
                h.services
                 .iter()
                 .map(|s| (s.addr, BridgedInfo { instance: s.instance_name.clone(),
                                                 port:     s.port }))
                 .collect()
             })
             .unwrap_or_default();

      let mut rows: Vec<DeviceRow> =
         statuses.into_iter()
                 .map(|s| DeviceRow { addr:      s.addr,
                                      name:      s.name,
                                      rssi:      s.rssi,
                                      paired:    s.paired,
                                      trusted:   s.trusted,
                                      connected: s.connected,
                                      kind:      s.kind,
                                      bridged:   bridged.get(&s.addr).cloned(),
                                      busy:      self.busy.get(&s.addr).copied() })
                 .collect();
      crate::shared::sort_rows(&mut rows);

      self.shared.lock().unwrap().devices = rows;
      self.ctx.request_repaint();
   }

   /// Like `refresh_rows`, but only re-stamps the busy labels onto the current rows -
   /// used right after spawning an operation so the spinner appears immediately.
   fn publish_rows_sync(&self)
   {
      {
         let mut state = self.shared.lock().unwrap();
         for row in &mut state.devices
         {
            row.busy = self.busy.get(&row.addr).copied();
         }
      }
      self.ctx.request_repaint();
   }
}
