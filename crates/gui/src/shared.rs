//! State shared between the egui thread and the tokio controller task, plus the
//! command channel type the UI uses to drive the controller.

use std::{collections::VecDeque,
          sync::{Arc, Mutex}};

use bt_bridge_core::bluez::{Address, device::DeviceKind};

pub const LOG_CAPACITY: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState
{
   Stopped,
   Starting,
   WaitingForDevices,
   Running,
   Stopping,
}

/// UI → controller commands.
#[derive(Debug)]
pub enum Command
{
   /// Manual re-read of the device list; it otherwise refreshes on a timer.
   Refresh,
   Start,
   Stop,
   /// Stop and start the bridge so devices connected after Start get their own
   /// DIRCON service (split-mode services are created at start only).
   RestartBridge,
   StartScan,
   StopScan,
   /// Toggle active reconnect for this session (used at the next bridge start).
   SetReconnect(bool),
   /// Persist the current reconnect choice to the config file.
   SaveReconnect,
   /// Plain GATT connect + trust - never pairs (see `bluez::manage`).
   Connect(Address),
   Disconnect(Address),
   SetTrusted(Address, bool),
   /// Explicit bonding for the rare device that requires it.
   Pair(Address),
   /// Remove the device from BlueZ (unpair + forget).
   Forget(Address),
}

/// The running DIRCON service backing a connected device (split mode: one per device).
#[derive(Debug, Clone)]
pub struct BridgedInfo
{
   pub instance: String,
   pub port:     u16,
}

/// One row of the Bluetooth device list: everything BlueZ knows about the device plus
/// this daemon's view of it.
#[derive(Debug, Clone)]
pub struct DeviceRow
{
   pub addr:      Address,
   pub name:      Option<String>,
   /// Present while the device is advertising during a scan.
   pub rssi:      Option<i16>,
   pub paired:    bool,
   pub trusted:   bool,
   pub connected: bool,
   pub kind:      DeviceKind,
   /// `Some` when the running bridge serves this device.
   pub bridged:   Option<BridgedInfo>,
   /// Label of the BlueZ operation in flight ("connecting…" …); buttons are disabled
   /// while set.
   pub busy:      Option<&'static str>,
}

impl DeviceRow
{
   pub fn display_name(&self) -> String
   {
      self.name.clone().unwrap_or_else(|| self.addr.to_string())
   }
}

/// Device list order: connected devices first, then named before anonymous,
/// alphabetical within each group.
pub fn sort_rows(rows: &mut [DeviceRow])
{
   rows.sort_by(|a, b| {
          (!a.connected, a.name.is_none(), a.display_name().to_lowercase())
             .cmp(&(!b.connected, b.name.is_none(), b.display_name().to_lowercase()))
       });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel
{
   Debug,
   Info,
   Warn,
   Error,
}

#[derive(Debug, Clone)]
pub struct LogLine
{
   pub time:    String,
   pub level:   LogLevel,
   pub message: String,
}

pub struct SharedState
{
   pub daemon_state: DaemonState,
   pub devices:      Vec<DeviceRow>,
   pub scanning:     bool,
   /// Running DIRCON services (split mode: one per bridged device).
   pub services:     usize,
   /// Connected DIRCON clients (from `StatusEvent`s).
   pub clients:      usize,
   /// A fitness device connected while the bridge runs with DIRCON clients attached;
   /// the UI offers a bridge restart to include it (set by the controller, cleared by
   /// the UI when answered).
   pub restart_offer: bool,
   /// Actively reconnect dropped BLE devices (`--reconnect`); applies at bridge start.
   pub reconnect:       bool,
   /// The `reconnect` value in the config file - when it differs from `reconnect`,
   /// the UI offers to save.
   pub reconnect_saved: bool,
   pub logs:         VecDeque<LogLine>,
}

impl SharedState
{
   pub fn new() -> Self
   {
      Self { daemon_state:  DaemonState::Stopped,
             devices:       Vec::new(),
             scanning:      false,
             services:      0,
             clients:       0,
             restart_offer: false,
             reconnect:       false,
             reconnect_saved: false,
             logs:          VecDeque::new() }
   }

   pub fn push_log(&mut self, line: LogLine)
   {
      if self.logs.len() >= LOG_CAPACITY
      {
         self.logs.pop_front();
      }
      self.logs.push_back(line);
   }
}

pub type Shared = Arc<Mutex<SharedState>>;

#[cfg(test)]
mod tests
{
   use super::*;

   fn row(addr_byte: u8, name: Option<&str>, connected: bool) -> DeviceRow
   {
      DeviceRow { addr:      Address::from([0, 0, 0, 0, 0, addr_byte]),
                  name:      name.map(str::to_string),
                  rssi:      None,
                  paired:    false,
                  trusted:   false,
                  connected,
                  kind:      DeviceKind::Other,
                  bridged:   None,
                  busy:      None }
   }

   #[test]
   fn rows_sort_connected_then_named_then_alphabetical()
   {
      let mut rows = [row(1, None, false),                 // anonymous, disconnected
                      row(2, Some("TICKR"), false),        // named, disconnected
                      row(3, Some("KICKR CORE"), true),    // connected
                      row(4, Some("assioma"), true)];      // connected, case-insensitive
      sort_rows(&mut rows);
      let order: Vec<Option<&str>> = rows.iter().map(|r| r.name.as_deref()).collect();
      assert_eq!(order, [Some("assioma"), Some("KICKR CORE"), Some("TICKR"), None]);
   }
}
