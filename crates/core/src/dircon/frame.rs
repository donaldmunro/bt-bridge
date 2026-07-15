use bytes::{BufMut, Bytes, BytesMut};

/// Real DIRCON wire format (observed from Wahoo KICKR Direct Connect capture):
///   version(1=0x01) | msgType(1) | seq_LE(2) | paylen_BE(2) | payload(paylen)
///
/// The seq field is a notification counter (LE u16, increments per notification;
/// zero for all other message types). paylen is the exact payload byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType
{
   DiscoverServices = 0x01,
   DiscoverChars = 0x02,
   ReadChar = 0x03,
   WriteChar = 0x04,
   Subscribe = 0x05,
   Notification = 0x06,
}

impl TryFrom<u8> for MsgType
{
   type Error = u8;

   fn try_from(v: u8) -> Result<Self, u8>
   {
      match v
      {
         | 0x01 => Ok(MsgType::DiscoverServices),
         | 0x02 => Ok(MsgType::DiscoverChars),
         | 0x03 => Ok(MsgType::ReadChar),
         | 0x04 => Ok(MsgType::WriteChar),
         | 0x05 => Ok(MsgType::Subscribe),
         | 0x06 => Ok(MsgType::Notification),
         | other => Err(other),
      }
   }
}

/// Characteristic property flags as used in DiscoverChars responses.
pub mod char_flags
{
   pub const READ: u8 = 0x01;
   pub const WRITE: u8 = 0x02;
   pub const NOTIFY: u8 = 0x04;
}

/// Frame-level protocol violation. Unlike an incomplete frame, the byte stream cannot be
/// resynchronised after one of these - the caller should close the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError
{
   BadVersion(u8),
   UnknownMsgType(u8),
}

impl std::fmt::Display for FrameError
{
   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
   {
      match self
      {
         | Self::BadVersion(v) => write!(f, "unsupported DIRCON version byte 0x{v:02X}"),
         | Self::UnknownMsgType(t) => write!(f, "unknown DIRCON message type 0x{t:02X}"),
      }
   }
}

impl std::error::Error for FrameError {}

#[derive(Debug, Clone)]
pub struct DirconFrame
{
   pub msg_type: MsgType,
   /// Notification sequence counter (LE u16). Zero for non-notification frames.
   pub seq:      u16,
   pub payload:  Bytes,
}

impl DirconFrame
{
   pub const HEADER_LEN: usize = 6;
   const VERSION: u8 = 0x01;

   /// Parse one frame from the front of `buf`.
   /// Returns `Ok(Some((frame, bytes_consumed)))` on a complete frame, `Ok(None)` if the
   /// buffer holds less than a complete frame (read more and retry), or `Err` on a
   /// protocol violation (close the connection).
   pub fn parse(buf: &[u8]) -> Result<Option<(DirconFrame, usize)>, FrameError>
   {
      if buf.len() < Self::HEADER_LEN
      {
         return Ok(None);
      }
      if buf[0] != Self::VERSION
      {
         return Err(FrameError::BadVersion(buf[0]));
      }
      let msg_type = MsgType::try_from(buf[1]).map_err(FrameError::UnknownMsgType)?;
      let seq = u16::from_le_bytes([buf[2], buf[3]]);
      let paylen = u16::from_be_bytes([buf[4], buf[5]]) as usize;
      let total = Self::HEADER_LEN + paylen;
      if buf.len() < total
      {
         return Ok(None);
      }
      let payload = Bytes::copy_from_slice(&buf[Self::HEADER_LEN..total]);
      Ok(Some((DirconFrame { msg_type, seq, payload }, total)))
   }

   pub fn serialize(&self) -> Bytes
   {
      let paylen = self.payload.len();
      let mut buf = BytesMut::with_capacity(Self::HEADER_LEN + paylen);
      buf.put_u8(Self::VERSION);
      buf.put_u8(self.msg_type as u8);
      buf.put_u16_le(self.seq);
      buf.put_u16(paylen as u16); // big-endian
      buf.put_slice(&self.payload);
      buf.freeze()
   }

   pub fn request(msg_type: MsgType, payload: impl Into<Bytes>) -> Self
   {
      Self { msg_type,
             seq: 0,
             payload: payload.into() }
   }

   pub fn reply(msg_type: MsgType, payload: impl Into<Bytes>) -> Self
   {
      Self { msg_type,
             seq: 0,
             payload: payload.into() }
   }

   pub fn notification(seq: u16, payload: impl Into<Bytes>) -> Self
   {
      Self { msg_type: MsgType::Notification,
             seq,
             payload: payload.into() }
   }
}

/// Build the 16-byte Bluetooth Base UUID form of a 16-bit UUID.
pub fn uuid16_to_bytes(uuid16: u16) -> [u8; 16]
{
   let mut b = [0u8; 16];
   b[2] = (uuid16 >> 8) as u8;
   b[3] = uuid16 as u8;
   // Bluetooth Base UUID suffix: 0000-1000-8000-00805f9b34fb
   b[4..6].copy_from_slice(&[0x00, 0x00]);
   b[6..8].copy_from_slice(&[0x10, 0x00]);
   b[8..10].copy_from_slice(&[0x80, 0x00]);
   b[10..16].copy_from_slice(&[0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb]);
   b
}

/// Convert a `uuid::Uuid` to the 16-byte wire representation used in DIRCON.
pub fn uuid_to_bytes(uuid: uuid::Uuid) -> [u8; 16] { *uuid.as_bytes() }

/// Parse a 16-byte UUID from a DIRCON payload offset.
pub fn uuid_from_bytes(b: &[u8]) -> Option<uuid::Uuid>
{
   let arr: [u8; 16] = b.get(..16)?.try_into().ok()?;
   Some(uuid::Uuid::from_bytes(arr))
}

#[cfg(test)]
mod tests
{
   use super::*;

   #[test]
   fn round_trip_empty_payload()
   {
      let frame = DirconFrame::request(MsgType::DiscoverServices, Bytes::new());
      let bytes = frame.serialize();
      let (parsed, consumed) = DirconFrame::parse(&bytes).unwrap().unwrap();
      assert_eq!(consumed, bytes.len());
      assert_eq!(parsed.msg_type, MsgType::DiscoverServices);
      assert_eq!(parsed.seq, 0);
      assert!(parsed.payload.is_empty());
   }

   #[test]
   fn round_trip_with_payload()
   {
      let frame = DirconFrame::notification(42, vec![0xDE, 0xAD, 0xBE, 0xEF]);
      let bytes = frame.serialize();
      let (parsed, consumed) = DirconFrame::parse(&bytes).unwrap().unwrap();
      assert_eq!(consumed, bytes.len());
      assert_eq!(parsed.msg_type, MsgType::Notification);
      assert_eq!(parsed.seq, 42);
      assert_eq!(&parsed.payload[..], &[0xDE, 0xAD, 0xBE, 0xEF]);
   }

   #[test]
   fn partial_buffer_returns_none()
   {
      let frame = DirconFrame::notification(1, vec![1, 2, 3]);
      let bytes = frame.serialize();
      assert!(DirconFrame::parse(&bytes[..bytes.len() - 1]).unwrap().is_none());
   }

   #[test]
   fn bad_version_is_a_protocol_violation()
   {
      let raw = [0xFF, 0x01, 0x00, 0x00, 0x00, 0x00];
      assert!(matches!(DirconFrame::parse(&raw), Err(FrameError::BadVersion(0xFF))));
   }

   #[test]
   fn unknown_msg_type_is_a_protocol_violation()
   {
      let raw = [0x01, 0x99, 0x00, 0x00, 0x00, 0x00];
      assert!(matches!(DirconFrame::parse(&raw), Err(FrameError::UnknownMsgType(0x99))));
   }

   #[test]
   fn real_capture_discover_services_request()
   {
      // Frame 8 from Wahoo KICKR capture
      let raw = bytes::Bytes::from_static(&[0x01, 0x01, 0x00, 0x00, 0x00, 0x00]);
      let (frame, consumed) = DirconFrame::parse(&raw).unwrap().unwrap();
      assert_eq!(consumed, 6);
      assert_eq!(frame.msg_type, MsgType::DiscoverServices);
      assert_eq!(frame.seq, 0);
      assert!(frame.payload.is_empty());
   }

   #[test]
   fn real_capture_read_request()
   {
      // Frame 32: ReadChar for 0x2A29 (Manufacturer Name)
      let raw: &[u8] =
         &[0x01, 0x03, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x2a, 0x29, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb];
      let (frame, consumed) = DirconFrame::parse(raw).unwrap().unwrap();
      assert_eq!(consumed, raw.len());
      assert_eq!(frame.msg_type, MsgType::ReadChar);
      assert_eq!(frame.payload.len(), 16);
   }

   #[test]
   fn notification_seq_counter()
   {
      let f0 = DirconFrame::notification(0, vec![1]);
      let f1 = DirconFrame::notification(1, vec![2]);
      let b0 = f0.serialize();
      let b1 = f1.serialize();
      let (p0, _) = DirconFrame::parse(&b0).unwrap().unwrap();
      let (p1, _) = DirconFrame::parse(&b1).unwrap().unwrap();
      assert_eq!(p0.seq, 0);
      assert_eq!(p1.seq, 1);
   }
}
