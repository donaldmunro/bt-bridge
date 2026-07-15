use std::{collections::HashSet, sync::Arc};

use bytes::{BufMut, Bytes, BytesMut};
use tokio::{io::{AsyncReadExt, AsyncWriteExt},
            net::{TcpListener, TcpStream},
            sync::{broadcast::error::RecvError, mpsc::UnboundedSender, watch},
            task::JoinSet};
use tracing::{debug, info, warn};

use crate::{bridge::GattIndex,
            bluez::device::{service_description, characteristic_description},
            dircon::{frame::{DirconFrame, MsgType, uuid_from_bytes, uuid_to_bytes},
                     notify::NotifyHub},
            status::StatusEvent};

/// Options for `serve_with`. `shutdown` and `status` exist for embedders (the GUI):
/// the headless daemon runs with both `None` and behaves exactly as before.
pub struct ServeOptions
{
   pub hub: Arc<NotifyHub>,
   /// Flipping the watched value to `true` (or dropping the sender) stops the accept
   /// loop and aborts all in-flight client tasks.
   pub shutdown: Option<watch::Receiver<bool>>,
   /// Receives `ClientConnected` / `ClientDisconnected` events (e.g. for a GUI badge).
   pub status: Option<UnboundedSender<StatusEvent>>,
}

pub async fn serve(listener: TcpListener, index: Arc<dyn GattIndex>) -> anyhow::Result<()>
{
   serve_with(listener, index,
              ServeOptions { hub: Arc::new(NotifyHub::new()), shutdown: None, status: None }).await
}

/// Like `serve`, but with a caller-supplied `NotifyHub` - lets tests inject
/// notification values without a BLE device.
pub async fn serve_with_hub(listener: TcpListener, index: Arc<dyn GattIndex>, hub: Arc<NotifyHub>)
                            -> anyhow::Result<()>
{
   serve_with(listener, index, ServeOptions { hub, shutdown: None, status: None }).await
}

pub async fn serve_with(listener: TcpListener, index: Arc<dyn GattIndex>, opts: ServeOptions)
                        -> anyhow::Result<()>
{
   let ServeOptions { hub, mut shutdown, status } = opts;
   info!(addr = %listener.local_addr()?, "DIRCON server listening");
   let mut clients: JoinSet<()> = JoinSet::new();
   loop
   {
      tokio::select!
      {
         accepted = listener.accept() =>
         {
            let (stream, peer) = accepted?;
            info!(%peer, "client connected");
            if let Some(tx) = &status
            {
               let _ = tx.send(StatusEvent::ClientConnected { peer: peer.to_string() });
            }
            let index = index.clone();
            let hub = hub.clone();
            let status = status.clone();
            clients.spawn(async move {
               let peer = peer.to_string();
               if let Err(e) = handle_client(stream, peer.clone(), index, hub).await
               {
                  warn!(%peer, err = %e, "client error");
               }
               if let Some(tx) = &status
               {
                  let _ = tx.send(StatusEvent::ClientDisconnected { peer });
               }
            });
         }
         _ = wait_for_shutdown(&mut shutdown) =>
         {
            info!(clients = clients.len(), "DIRCON server shutting down");
            clients.shutdown().await;
            return Ok(());
         }
         // Reap finished client tasks so the JoinSet doesn't accumulate them.
         Some(_) = clients.join_next(), if !clients.is_empty() => {}
      }
   }
}

/// Resolves when the shutdown signal fires; pends forever when there is none.
/// A dropped sender counts as shutdown - the embedder that could stop us is gone.
async fn wait_for_shutdown(shutdown: &mut Option<watch::Receiver<bool>>)
{
   match shutdown
   {
      | Some(rx) =>
      {
         while !*rx.borrow_and_update()
         {
            if rx.changed().await.is_err()
            {
               return;
            }
         }
      }
      | None => std::future::pending::<()>().await,
   }
}

async fn handle_client(stream: TcpStream, peer: String, index: Arc<dyn GattIndex>, hub: Arc<NotifyHub>)
                       -> anyhow::Result<()>
{
   let (mut reader, mut writer) = stream.into_split();
   let mut buf: Vec<u8> = Vec::with_capacity(4096);
   let mut tmp = [0u8; 4096];
   let mut rx = hub.subscribe_stream();
   let mut subs: HashSet<uuid::Uuid> = HashSet::new();

   loop
   {
      tokio::select!
      {
         n = reader.read(&mut tmp) =>
         {
            let n = n?;
            if n == 0
            {
               info!(%peer, "client disconnected");
               return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);

            // Drain all complete frames currently in the buffer.
            let mut pos = 0;
            loop
            {
               match DirconFrame::parse(&buf[pos..])
               {
                  | Ok(Some((frame, consumed))) =>
                  {
                     pos += consumed;
                     debug!(%peer, seq = frame.seq, msg_type = ?frame.msg_type, "frame received");
                     if let Some(reply) = dispatch(frame, &*index, &hub, &mut subs).await
                     {
                        writer.write_all(&reply.serialize()).await?;
                     }
                  }
                  | Ok(None) => break, // incomplete frame: read more
                  | Err(e) =>
                  {
                     // The stream cannot be resynchronised after a framing violation.
                     warn!(%peer, err = %e, "DIRCON protocol violation; closing connection");
                     return Ok(());
                  }
               }
            }
            buf.drain(..pos);
         }
         result = rx.recv() =>
         {
            match result
            {
               | Ok((char_uuid, value)) =>
               {
                  if subs.contains(&char_uuid)
                  {
                     let mut payload = BytesMut::with_capacity(16 + value.len());
                     payload.put_slice(&uuid_to_bytes(char_uuid));
                     payload.put_slice(&value);
                     let frame = DirconFrame::notification(hub.next_seq(), payload.freeze());
                     writer.write_all(&frame.serialize()).await?;
                  }
               }
               | Err(RecvError::Lagged(missed)) =>
               {
                  warn!(%peer, missed, "notification receiver lagged; skipping missed values");
               }
               // The hub outlives every client task (each holds an Arc), so Closed
               // cannot occur while this loop runs; bail rather than spin if it does.
               | Err(RecvError::Closed) =>
               {
                  debug!(%peer, "notification channel closed; ending client task");
                  return Ok(());
               }
            }
         }
      }
   }
}

async fn dispatch(frame: DirconFrame, index: &dyn GattIndex, hub: &NotifyHub,
                  subs: &mut HashSet<uuid::Uuid>)
                  -> Option<DirconFrame>
{
   match frame.msg_type
   {
      | MsgType::DiscoverServices => Some(discover_services(index)),
      | MsgType::DiscoverChars => Some(discover_characteristics(&frame.payload, index)),
      | MsgType::ReadChar => Some(read_characteristic(&frame.payload, index).await),
      | MsgType::WriteChar => Some(write_characteristic(&frame.payload, index).await),
      | MsgType::Subscribe => Some(subscribe_characteristic(&frame.payload, index, hub, subs).await),
      | MsgType::Notification =>
      {
         // Notifications flow server → client only; receiving one from a client is unexpected
         warn!("unexpected Notification frame from client");
         None
      }
   }
}

/// DiscoverServices (0x01): request payload is empty; response payload is the
/// concatenated 16-byte UUIDs of every service in the merged GATT tree,
/// deduplicated across devices in registration order.
fn discover_services(index: &dyn GattIndex) -> DirconFrame
{
   let mut seen = HashSet::new();
   let mut payload = BytesMut::new();

   for device in index.all_devices()
   {
      for service in &device.services
      {
         if seen.insert(service.uuid)
         {
            payload.put_slice(&uuid_to_bytes(service.uuid));
            info!(%service.uuid, device = %device.addr, name=service_description(service.uuid),
                  "DiscoverServices");
         }
      }
   }

   info!(services = seen.len(), "DiscoverServices reply");
   DirconFrame::reply(MsgType::DiscoverServices, payload.freeze())
}

/// DiscoverChars (0x02): request payload is a service UUID (16 bytes); response payload
/// echoes the service UUID followed by `UUID(16) + flags(1)` per characteristic.
fn discover_characteristics(payload: &[u8], index: &dyn GattIndex) -> DirconFrame
{
   let Some(service_uuid) = uuid_from_bytes(payload)
   else
   {
      warn!(len = payload.len(), "DiscoverChars request payload too short for a UUID");
      return DirconFrame::reply(MsgType::DiscoverChars, Bytes::new());
   };

   let mut out = BytesMut::new();
   out.put_slice(&uuid_to_bytes(service_uuid));

   let found = index.device_for_service(service_uuid)
                    .and_then(|d| d.services.iter().position(|s| s.uuid == service_uuid).map(|i| (d, i)));

   match found
   {
      | Some((device, i)) =>
      {
         let service = &device.services[i];
         for &char_uuid in &service.characteristic_uuids
         {
            let flags = device.characteristics.get(&char_uuid).map(|c| c.flags).unwrap_or(0);
            out.put_slice(&uuid_to_bytes(char_uuid));
            out.put_u8(flags);
            info!(%char_uuid, device = %device.addr, description = characteristic_description(char_uuid), 
                  "DiscoverChars reply entry");
         }
         info!(%service_uuid, device = %device.addr, chars = service.characteristic_uuids.len(), "DiscoverChars reply");
      }
      | None => warn!(%service_uuid, "DiscoverChars for unknown service; replying with no characteristics"),
   }

   DirconFrame::reply(MsgType::DiscoverChars, out.freeze())
}

/// ReadChar (0x03): request payload is a characteristic UUID (16 bytes); response payload
/// echoes the UUID followed by the value bytes. On an unknown UUID or a BlueZ read
/// failure the value is empty - DIRCON has no error frame, so the warn log is the
/// only diagnostic.
async fn read_characteristic(payload: &[u8], index: &dyn GattIndex) -> DirconFrame
{
   let Some(char_uuid) = uuid_from_bytes(payload)
   else
   {
      warn!(len = payload.len(), "ReadChar request payload too short for a UUID");
      return DirconFrame::reply(MsgType::ReadChar, Bytes::new());
   };

   let mut out = BytesMut::new();
   out.put_slice(&uuid_to_bytes(char_uuid));

   match index.lookup(char_uuid)
   {
      | Some(device) => match device.read(char_uuid).await
      {
         | Ok(value) =>
         {
            debug!(char = %char_uuid, device = %device.addr, len = value.len(), "ReadChar reply");
            out.put_slice(&value);
         }
         | Err(e) =>
         {
            warn!(char = %char_uuid, device = %device.addr, err = %e,
                  "ReadChar failed; replying with empty value");
         }
      },
      | None => warn!(char = %char_uuid, "ReadChar for unknown characteristic"),
   }

   DirconFrame::reply(MsgType::ReadChar, out.freeze())
}

/// WriteChar (0x04): request payload is a characteristic UUID (16 bytes) followed by the
/// value to write; response payload echoes the UUID as acknowledgement. The ack is sent
/// even when the write fails (with a warn log) - DIRCON has no error frame.
async fn write_characteristic(payload: &[u8], index: &dyn GattIndex) -> DirconFrame
{
   let Some(char_uuid) = uuid_from_bytes(payload)
   else
   {
      warn!(len = payload.len(), "WriteChar request payload too short for a UUID");
      return DirconFrame::reply(MsgType::WriteChar, Bytes::new());
   };
   let value = payload[16..].to_vec();

   let mut out = BytesMut::new();
   out.put_slice(&uuid_to_bytes(char_uuid));

   match index.lookup(char_uuid)
   {
      | Some(device) => match device.write(char_uuid, value).await
      {
         | Ok(()) => debug!(char = %char_uuid, device = %device.addr, "WriteChar ack"),
         | Err(e) => warn!(characteristic = %char_uuid, device = %device.addr, err = %e, "WriteChar failed"),
      },
      | None => warn!(char = %char_uuid, "WriteChar for unknown characteristic"),
   }

   DirconFrame::reply(MsgType::WriteChar, out.freeze())
}

/// Subscribe (0x05): request payload is a characteristic UUID (16 bytes); response payload
/// echoes the UUID as acknowledgement. Triggers BlueZ `StartNotify` via the hub (once per
/// characteristic globally) and registers the UUID in this client's subscription set.
///
/// The UUID is added to the client's set even when `StartNotify` fails - no values will
/// flow either way, and the ack keeps the client's request pipeline moving (DIRCON has
/// no error frame). A tolerant extension: a 17-byte payload whose final byte is `0x00`
/// is treated as unsubscribe (some clients toggle); a 16-byte payload always subscribes.
async fn subscribe_characteristic(payload: &[u8], index: &dyn GattIndex, hub: &NotifyHub,
                        subs: &mut HashSet<uuid::Uuid>)
                        -> DirconFrame
{
   let Some(char_uuid) = uuid_from_bytes(payload)
   else
   {
      warn!(len = payload.len(), "Subscribe request payload too short for a UUID");
      return DirconFrame::reply(MsgType::Subscribe, Bytes::new());
   };

   let mut out = BytesMut::new();
   out.put_slice(&uuid_to_bytes(char_uuid));

   if payload.len() >= 17 && payload[16] == 0x00
   {
      subs.remove(&char_uuid);
      info!(char = %char_uuid, "client unsubscribed");
      return DirconFrame::reply(MsgType::Subscribe, out.freeze());
   }

   subs.insert(char_uuid);
   match hub.ensure_notify(char_uuid, index).await
   {
      | Ok(()) => info!(characteristic = %char_uuid, description = characteristic_description(char_uuid), "client subscribed"),
      | Err(e) => warn!(char = %char_uuid, err = %e, "Subscribe: StartNotify failed; acking anyway"),
   }

   DirconFrame::reply(MsgType::Subscribe, out.freeze())
}

#[cfg(test)]
mod tests
{
   use uuid::Uuid;

   use super::*;
   use crate::{bluez::{BlueZDevice, device::GattService},
               bridge::MergedGattIndex};

   const CYCLING_POWER: u128 = 0x0000_1818_0000_1000_8000_0080_5f9b_34fb;
   const HEART_RATE: u128 = 0x0000_180d_0000_1000_8000_0080_5f9b_34fb;
   const POWER_MEASUREMENT: u128 = 0x0000_2a63_0000_1000_8000_0080_5f9b_34fb;
   const SENSOR_LOCATION: u128 = 0x0000_2a5d_0000_1000_8000_0080_5f9b_34fb;

   fn fake_device(addr_byte: u8, services: Vec<GattService>) -> BlueZDevice
   {
      BlueZDevice { addr: bluer::Address::from([addr_byte; 6]),
                    name: None,
                    services,
                    characteristics: std::collections::HashMap::new(),
                    is_smart_trainer: false }
   }

   fn service(uuid: u128, char_uuids: &[u128]) -> GattService
   {
      GattService { uuid:       Uuid::from_u128(uuid),
                    characteristic_uuids: char_uuids.iter().map(|&u| Uuid::from_u128(u)).collect(),
                    priority:   1 }
   }

   #[test]
   fn discover_services_dedupes_across_devices()
   {
      let index = MergedGattIndex::new(vec![fake_device(1, vec![service(CYCLING_POWER, &[])]),
                                            fake_device(2,
                                                        vec![service(CYCLING_POWER, &[]),
                                                             service(HEART_RATE, &[])])]);
      let reply = discover_services(&index);

      assert_eq!(reply.msg_type, MsgType::DiscoverServices);
      assert_eq!(reply.payload.len(), 32); // 2 unique services × 16 bytes
      assert_eq!(uuid_from_bytes(&reply.payload[0..16]).unwrap(), Uuid::from_u128(CYCLING_POWER));
      assert_eq!(uuid_from_bytes(&reply.payload[16..32]).unwrap(), Uuid::from_u128(HEART_RATE));
   }

   #[test]
   fn test_discover_characteristics_layout()
   {
      let index =
         MergedGattIndex::new(vec![fake_device(1, vec![service(CYCLING_POWER, &[POWER_MEASUREMENT, SENSOR_LOCATION])])]);
      let request = uuid_to_bytes(Uuid::from_u128(CYCLING_POWER));
      let reply = discover_characteristics(&request, &index);

      assert_eq!(reply.msg_type, MsgType::DiscoverChars);
      assert_eq!(reply.payload.len(), 16 + 2 * 17); // service UUID + 2 × (UUID + flags)
      assert_eq!(uuid_from_bytes(&reply.payload[0..16]).unwrap(), Uuid::from_u128(CYCLING_POWER));
      assert_eq!(uuid_from_bytes(&reply.payload[16..32]).unwrap(), Uuid::from_u128(POWER_MEASUREMENT));
      assert_eq!(uuid_from_bytes(&reply.payload[33..49]).unwrap(), Uuid::from_u128(SENSOR_LOCATION));
   }

   #[test]
   fn test_discover_characteristics_unknown_service_echoes_uuid_only()
   {
      let index = MergedGattIndex::new(vec![fake_device(1, vec![service(CYCLING_POWER, &[])])]);
      let request = uuid_to_bytes(Uuid::from_u128(HEART_RATE));
      let reply = discover_characteristics(&request, &index);

      assert_eq!(reply.payload.len(), 16);
      assert_eq!(uuid_from_bytes(&reply.payload[..]).unwrap(), Uuid::from_u128(HEART_RATE));
   }

   #[test]
   fn test_discover_characteristics_short_payload_replies_empty()
   {
      let index = MergedGattIndex::new(vec![]);
      let reply = discover_characteristics(&[0u8; 4], &index);
      assert!(reply.payload.is_empty());
   }

   #[tokio::test]
   async fn test_read_characteristic_unknown_uuid_echoes_uuid_with_empty_value()
   {
      let index = MergedGattIndex::new(vec![fake_device(1, vec![service(CYCLING_POWER, &[])])]);
      let request = uuid_to_bytes(Uuid::from_u128(POWER_MEASUREMENT));
      let reply = read_characteristic(&request, &index).await;

      assert_eq!(reply.msg_type, MsgType::ReadChar);
      assert_eq!(reply.payload.len(), 16); // UUID echo, no value
      assert_eq!(uuid_from_bytes(&reply.payload[..]).unwrap(), Uuid::from_u128(POWER_MEASUREMENT));
   }

   #[tokio::test]
   async fn test_read_characteristic_short_payload_replies_empty()
   {
      let index = MergedGattIndex::new(vec![]);
      let reply = read_characteristic(&[0u8; 7], &index).await;
      assert_eq!(reply.msg_type, MsgType::ReadChar);
      assert!(reply.payload.is_empty());
   }

   #[tokio::test]
   async fn test_write_characteristic_unknown_uuid_still_acks()
   {
      let index = MergedGattIndex::new(vec![]);
      let mut request = uuid_to_bytes(Uuid::from_u128(POWER_MEASUREMENT)).to_vec();
      request.extend_from_slice(&[0x42, 0x00]); // ERG target payload
      let reply = write_characteristic(&request, &index).await;

      assert_eq!(reply.msg_type, MsgType::WriteChar);
      assert_eq!(reply.payload.len(), 16); // ack = UUID echo only
      assert_eq!(uuid_from_bytes(&reply.payload[..]).unwrap(), Uuid::from_u128(POWER_MEASUREMENT));
   }

   #[tokio::test]
   async fn test_write_characteristic_short_payload_replies_empty()
   {
      let index = MergedGattIndex::new(vec![]);
      let reply = write_characteristic(&[0u8; 15], &index).await;
      assert_eq!(reply.msg_type, MsgType::WriteChar);
      assert!(reply.payload.is_empty());
   }

   #[tokio::test]
   async fn test_subscribe_short_payload_replies_empty()
   {
      let index = MergedGattIndex::new(vec![]);
      let hub = NotifyHub::new();
      let mut subs = HashSet::new();
      let reply = subscribe_characteristic(&[0u8; 10], &index, &hub, &mut subs).await;
      assert_eq!(reply.msg_type, MsgType::Subscribe);
      assert!(reply.payload.is_empty());
      assert!(subs.is_empty());
   }

   #[tokio::test]
   async fn test_subscribe_unknown_uuid_still_acks_and_registers()
   {
      let index = MergedGattIndex::new(vec![]);
      let hub = NotifyHub::new();
      let mut subs = HashSet::new();
      let x = Uuid::from_u128(POWER_MEASUREMENT);
      let reply = subscribe_characteristic(&uuid_to_bytes(x), &index, &hub, &mut subs).await;

      assert_eq!(reply.msg_type, MsgType::Subscribe);
      assert_eq!(uuid_from_bytes(&reply.payload[..]).unwrap(), x);
      assert!(subs.contains(&x));
   }

   #[tokio::test]
   async fn test_subscribe_with_disable_byte_unsubscribes()
   {
      let index = MergedGattIndex::new(vec![]);
      let hub = NotifyHub::new();
      let x = Uuid::from_u128(POWER_MEASUREMENT);
      let mut subs = HashSet::from([x]);

      let mut request = uuid_to_bytes(x).to_vec();
      request.push(0x00);
      let reply = subscribe_characteristic(&request, &index, &hub, &mut subs).await;

      assert_eq!(uuid_from_bytes(&reply.payload[..]).unwrap(), x);
      assert!(subs.is_empty());
   }

   /// Reads whole DIRCON frames off a client TcpStream, carrying partial data between calls.
   struct FrameReader
   {
      stream: tokio::net::TcpStream,
      buf:    Vec<u8>,
   }

   impl FrameReader
   {
      async fn next(&mut self) -> DirconFrame
      {
         let mut tmp = [0u8; 1024];
         loop
         {
            if let Some((frame, consumed)) = DirconFrame::parse(&self.buf).unwrap()
            {
               self.buf.drain(..consumed);
               return frame;
            }
            let n = self.stream.read(&mut tmp).await.unwrap();
            assert!(n > 0, "server closed the connection");
            self.buf.extend_from_slice(&tmp[..n]);
         }
      }
   }

   #[tokio::test]
   async fn test_notification_fanout_end_to_end()
   {
      use std::time::Duration;

      let index: Arc<dyn GattIndex> = Arc::new(MergedGattIndex::new(vec![]));
      let hub = Arc::new(NotifyHub::new());
      let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let addr = listener.local_addr().unwrap();
      tokio::spawn(serve_with_hub(listener, index, hub.clone()));

      let mut client = FrameReader { stream: tokio::net::TcpStream::connect(addr).await.unwrap(),
                                     buf:    Vec::new() };
      let x = Uuid::from_u128(POWER_MEASUREMENT);
      let y = Uuid::from_u128(SENSOR_LOCATION);

      // 1. Subscribe to X → UUID-echo ack (also proves the subscription is registered,
      //    because the ack is written after the subs set is updated).
      let request = DirconFrame::request(MsgType::Subscribe, uuid_to_bytes(x).to_vec());
      client.stream.write_all(&request.serialize()).await.unwrap();
      let ack = client.next().await;
      assert_eq!(ack.msg_type, MsgType::Subscribe);
      assert_eq!(uuid_from_bytes(&ack.payload[..]).unwrap(), x);

      // 2. Inject a value for X (bypassing BlueZ) → Notification frame, seq 0.
      hub.sender().send((x, vec![0xAA, 0xBB])).unwrap();
      let notif = client.next().await;
      assert_eq!(notif.msg_type, MsgType::Notification);
      assert_eq!(notif.seq, 0);
      assert_eq!(uuid_from_bytes(&notif.payload[0..16]).unwrap(), x);
      assert_eq!(&notif.payload[16..], &[0xAA, 0xBB]);

      // 3. A value for un-subscribed Y must not be forwarded.
      hub.sender().send((y, vec![0x01])).unwrap();
      let no_frame = tokio::time::timeout(Duration::from_millis(100), client.next()).await;
      assert!(no_frame.is_err(), "received a notification for an un-subscribed characteristic");

      // 4. Next X value gets seq 1 (Y did not consume a sequence number).
      hub.sender().send((x, vec![0xCC])).unwrap();
      let notif = client.next().await;
      assert_eq!(notif.seq, 1);
      assert_eq!(&notif.payload[16..], &[0xCC]);
   }

   #[tokio::test]
   async fn test_protocol_violation_closes_connection()
   {
      let index: Arc<dyn GattIndex> = Arc::new(MergedGattIndex::new(vec![]));
      let hub = Arc::new(NotifyHub::new());
      let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let addr = listener.local_addr().unwrap();
      tokio::spawn(serve_with_hub(listener, index, hub));

      let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
      stream.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00]).await.unwrap();

      // The server must close the connection (read returns 0) instead of wedging.
      let mut tmp = [0u8; 64];
      let n = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut tmp))
         .await
         .expect("server did not close the connection")
         .unwrap();
      assert_eq!(n, 0);
   }

   #[tokio::test]
   async fn test_shutdown_stops_server_and_emits_client_events()
   {
      use std::time::Duration;

      use crate::status::StatusEvent;

      let index: Arc<dyn GattIndex> = Arc::new(MergedGattIndex::new(vec![]));
      let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
      let addr = listener.local_addr().unwrap();

      let (shutdown_tx, shutdown_rx) = watch::channel(false);
      let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel();
      let server = tokio::spawn(serve_with(listener, index,
                                           ServeOptions { hub:      Arc::new(NotifyHub::new()),
                                                          shutdown: Some(shutdown_rx),
                                                          status:   Some(status_tx) }));

      // Connecting emits ClientConnected...
      let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
      match tokio::time::timeout(Duration::from_secs(2), status_rx.recv()).await.unwrap().unwrap()
      {
         | StatusEvent::ClientConnected { .. } => {}
         | other => panic!("expected ClientConnected, got {other:?}"),
      }

      // ...a clean client close emits ClientDisconnected...
      drop(stream);
      match tokio::time::timeout(Duration::from_secs(2), status_rx.recv()).await.unwrap().unwrap()
      {
         | StatusEvent::ClientDisconnected { .. } => {}
         | other => panic!("expected ClientDisconnected, got {other:?}"),
      }

      // ...and shutdown ends serve_with cleanly, closing an in-flight client.
      stream = tokio::net::TcpStream::connect(addr).await.unwrap();
      let _ = status_rx.recv().await; // its ClientConnected
      shutdown_tx.send(true).unwrap();
      tokio::time::timeout(Duration::from_secs(2), server)
         .await
         .expect("server did not shut down")
         .unwrap()
         .unwrap();
      let mut tmp = [0u8; 8];
      let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut tmp))
         .await
         .expect("client socket was not closed on shutdown")
         .unwrap();
      assert_eq!(n, 0);
   }
}
