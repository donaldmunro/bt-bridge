use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StatusEvent
{
   DeviceFound { addr: String, name: Option<String> },
   DeviceConnected { addr: String },
   ClientConnected { peer: String },
   ClientDisconnected { peer: String },
   NotificationForwarded { uuid: String, len: usize },
   Error { msg: String },
}
