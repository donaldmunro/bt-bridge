use std::{collections::{BTreeMap, HashMap, HashSet},
          io::{Read, Write},
          net::{Ipv4Addr, SocketAddrV4, TcpStream, UdpSocket},
          time::{Duration, Instant}};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use uuid::Uuid;

const SERVICE_TYPE: &str = "_wahoo-fitness-tnp._tcp.local";
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

fn main() -> Result<()>
{
   let args = Args::parse();
   run(args)
}

#[derive(Parser, Debug)]
#[command(name = "dircon-sim",
          about = "Networking-only Wahoo Direct Connect simulator client for bt-bridge")]
struct Args
{
   /// mDNS browse timeout in milliseconds.
   #[arg(long, default_value_t = 1500)]
   mdns_timeout_ms: u64,

   /// Bind the mDNS UDP browse socket to this local IPv4 address.
   #[arg(long)]
   bind: Option<Ipv4Addr>,

   /// Choose a discovered instance by case-insensitive match on the instance name.
   #[arg(long)]
   instance: Option<String>,

   /// Choose a discovered instance by zero-based index from the browse output.
   #[arg(long, default_value_t = 0)]
   index: usize,

   /// Read every readable characteristic after discovery.
   #[arg(long)]
   read_all: bool,

   /// Subscribe to this characteristic UUID (repeatable).
   #[arg(long = "subscribe")]
   subscribe: Vec<String>,

   /// Subscribe to every notifiable characteristic discovered on the selected device.
   #[arg(long)]
   subscribe_all: bool,

   /// How long to wait for notifications after subscribing.
   #[arg(long, default_value_t = 10)]
   listen_seconds: u64,
}

fn run(args: Args) -> Result<()>
{
   let timeout = Duration::from_millis(args.mdns_timeout_ms);
   let services = discover_services(timeout, args.bind)?;
   if services.is_empty()
   {
      bail!("no DIRCON services discovered via mDNS for {SERVICE_TYPE}");
   }

   print_discovery(&services);
   let selected = select_service(&services, &args)?;
   println!("\nSelected instance: {} at {}:{}", selected.instance_name, selected.addr, selected.port);

   let mut client = DirconClient::connect(selected, args.bind)?;
   let service_uuids = client.discover_services()?;
   println!("Discovered {} GATT services", service_uuids.len());

   let mut service_map = BTreeMap::new();
   for service_uuid in service_uuids
   {
      let chars = client.discover_chars(service_uuid)?;
      println!("  service {}", service_uuid);
      for ch in &chars
      {
         println!("    {} [{}]", ch.uuid, format_flags(ch.flags));
      }
      service_map.insert(service_uuid, chars);
   }

   if args.read_all
   {
      read_all_characteristics(&mut client, &service_map)?;
   }

   let subscriptions = select_subscriptions(&args, &service_map)?;
   if !subscriptions.is_empty()
   {
      for uuid in &subscriptions
      {
         client.subscribe(*uuid)?;
         println!("Subscribed to {}", uuid);
      }
      let duration = Duration::from_secs(args.listen_seconds);
      println!("Listening for notifications for {:.1}s", duration.as_secs_f32());
      client.read_notifications(duration)?;
   }

   Ok(())
}

fn read_all_characteristics(client: &mut DirconClient,
                            service_map: &BTreeMap<Uuid, Vec<DiscoveredCharacteristic>>)
                            -> Result<()>
{
   println!("\nReading readable characteristics");
   for chars in service_map.values()
   {
      for ch in chars
      {
         if ch.flags & char_flags::READ != 0
         {
            let value = client.read_char(ch.uuid)?;
            println!("  {} = {}", ch.uuid, hex_bytes(&value));
         }
      }
   }
   Ok(())
}

fn select_subscriptions(args: &Args,
                        service_map: &BTreeMap<Uuid, Vec<DiscoveredCharacteristic>>)
                        -> Result<Vec<Uuid>>
{
   let mut wanted = Vec::new();
   let mut seen = HashSet::new();

   if args.subscribe_all
   {
      for chars in service_map.values()
      {
         for ch in chars
         {
            if ch.flags & char_flags::NOTIFY != 0 && seen.insert(ch.uuid)
            {
               wanted.push(ch.uuid);
            }
         }
      }
   }

   for raw in &args.subscribe
   {
      let uuid = parse_uuid(raw)?;
      if seen.insert(uuid)
      {
         wanted.push(uuid);
      }
   }

   Ok(wanted)
}

fn select_service<'a>(services: &'a [DiscoveredService], args: &Args) -> Result<&'a DiscoveredService>
{
   if let Some(instance) = &args.instance
   {
      let wanted = instance.to_ascii_lowercase();
      return services.iter()
                     .find(|s| s.instance_name.to_ascii_lowercase() == wanted)
                     .ok_or_else(|| anyhow!("no discovered instance matched {instance}"));
   }

   services.get(args.index)
           .ok_or_else(|| anyhow!("discovered {} service(s), index {} is out of range",
                                  services.len(), args.index))
}

fn print_discovery(services: &[DiscoveredService])
{
   println!("Discovered {} DIRCON mDNS service(s):", services.len());
   for (index, service) in services.iter().enumerate()
   {
      println!("[{index}] {} -> {}:{} (host: {}, fqdn: {})",
               service.instance_name,
               service.addr,
               service.port,
               service.host,
               service.instance_fqdn);
      if !service.txt.is_empty()
      {
         println!("     TXT: {}", format_txt(&service.txt));
      }
   }
}

fn format_txt(txt: &BTreeMap<String, String>) -> String
{
   txt.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")
}

fn format_flags(flags: u8) -> String
{
   let mut parts = Vec::new();
   if flags & char_flags::READ != 0
   {
      parts.push("read");
   }
   if flags & char_flags::WRITE != 0
   {
      parts.push("write");
   }
   if flags & char_flags::NOTIFY != 0
   {
      parts.push("notify");
   }
   if parts.is_empty()
   {
      format!("0x{flags:02X}")
   }
   else
   {
      parts.join("|")
   }
}

fn parse_uuid(input: &str) -> Result<Uuid>
{
   if let Some(hex) = input.strip_prefix("0x").or_else(|| input.strip_prefix("0X"))
   {
      let short = u16::from_str_radix(hex, 16).with_context(|| format!("invalid 16-bit UUID {input}"))?;
      return Ok(uuid16(short));
   }
   input.parse().with_context(|| format!("invalid UUID {input}"))
}

fn discover_services(timeout: Duration, bind_ip: Option<Ipv4Addr>) -> Result<Vec<DiscoveredService>>
{
   let bind_addr = SocketAddrV4::new(bind_ip.unwrap_or(Ipv4Addr::UNSPECIFIED), 0);
   let socket = UdpSocket::bind(bind_addr).context("failed to bind mDNS UDP socket")?;
   socket.set_read_timeout(Some(Duration::from_millis(200)))?;
   // Loopback must stay enabled: on Linux IP_MULTICAST_LOOP is a *sender-side*
   // option, and bt-bridge usually runs on this same host - with it disabled
   // the browse query is never delivered to a local responder.
   socket.set_multicast_loop_v4(true)?;
   socket.set_multicast_ttl_v4(255)?;
   if let Some(ip) = bind_ip
   {
      socket.join_multicast_v4(&MDNS_GROUP, &ip)?;
   }
   else
   {
      socket.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED)?;
   }

   let query = build_ptr_query(SERVICE_TYPE);
   let target = SocketAddrV4::new(MDNS_GROUP, MDNS_PORT);
   socket.send_to(&query, target).context("failed to send mDNS browse query")?;

   let deadline = Instant::now() + timeout;
   let mut services = HashMap::<String, PartialService>::new();
   let mut buf = [0u8; 4096];

   while Instant::now() < deadline
   {
      match socket.recv_from(&mut buf)
      {
         | Ok((len, _from)) =>
         {
            if let Ok(packet) = DnsPacket::parse(&buf[..len])
            {
               merge_packet(&mut services, packet);
            }
         }
         | Err(err)
            if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
         {
            continue;
         }
         | Err(err) => return Err(err).context("failed to receive mDNS response"),
      }
   }

   let mut resolved: Vec<DiscoveredService> = services.into_values().filter_map(PartialService::finalize).collect();
   resolved.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));
   Ok(resolved)
}

fn merge_packet(services: &mut HashMap<String, PartialService>, packet: DnsPacket)
{
   let mut addresses = HashMap::<String, Ipv4Addr>::new();
   for rr in &packet.records
   {
      if let DnsRData::A(addr) = rr.data
      {
         addresses.insert(rr.name.clone(), addr);
      }
   }

   for rr in packet.records
   {
      match rr.data
      {
         | DnsRData::Ptr(target) if rr.name.eq_ignore_ascii_case(SERVICE_TYPE) =>
         {
            services.entry(target.clone()).or_insert_with(|| PartialService::new(target));
         }
         | DnsRData::Srv { target, port, .. } =>
         {
            let entry = services.entry(rr.name.clone()).or_insert_with(|| PartialService::new(rr.name.clone()));
            entry.host = Some(target.clone());
            entry.port = Some(port);
            if let Some(addr) = addresses.get(&target)
            {
               entry.addr = Some(*addr);
            }
         }
         | DnsRData::Txt(values) =>
         {
            let entry = services.entry(rr.name.clone()).or_insert_with(|| PartialService::new(rr.name.clone()));
            for (k, v) in values
            {
               entry.txt.insert(k, v);
            }
         }
         | DnsRData::A(addr) =>
         {
            for service in services.values_mut()
            {
               if service.host.as_deref() == Some(rr.name.as_str())
               {
                  service.addr = Some(addr);
               }
            }
         }
         | _ => {}
      }
   }
}

#[derive(Debug, Clone)]
struct DiscoveredService
{
   instance_fqdn: String,
   instance_name: String,
   host: String,
   addr: Ipv4Addr,
   port: u16,
   txt: BTreeMap<String, String>,
}

#[derive(Debug)]
struct PartialService
{
   instance_fqdn: String,
   host: Option<String>,
   addr: Option<Ipv4Addr>,
   port: Option<u16>,
   txt: BTreeMap<String, String>,
}

impl PartialService
{
   fn new(instance_fqdn: String) -> Self
   {
      Self { instance_fqdn,
             host: None,
             addr: None,
             port: None,
             txt: BTreeMap::new() }
   }

   fn finalize(self) -> Option<DiscoveredService>
   {
      Some(DiscoveredService { instance_name: trim_local_suffix(&self.instance_fqdn).split('.').next()?.to_string(),
                               instance_fqdn: self.instance_fqdn,
                               host: trim_local_suffix(self.host.as_deref()?).to_string(),
                               addr: self.addr?,
                               port: self.port?,
                               txt: self.txt })
   }
}

fn trim_local_suffix(name: &str) -> &str { name.trim_end_matches('.').trim_end_matches(".local") }

struct DirconClient
{
   stream: TcpStream,
   read_buf: Vec<u8>,
}

impl DirconClient
{
   fn connect(service: &DiscoveredService, bind_ip: Option<Ipv4Addr>) -> Result<Self>
   {
      let stream = if let Some(ip) = bind_ip
      {
         TcpStream::connect((service.addr, service.port)).with_context(|| {
                              format!("failed to connect to {}:{} after browsing from {ip}",
                                                              service.addr, service.port)
                                                   })?
      }
      else
      {
         TcpStream::connect((service.addr, service.port)).with_context(|| {
                                                      format!("failed to connect to {}:{}", service.addr, service.port)
                                                   })?
      };
      stream.set_read_timeout(Some(Duration::from_secs(2)))?;
      stream.set_write_timeout(Some(Duration::from_secs(2)))?;
      stream.set_nodelay(true)?;
      Ok(Self { stream, read_buf: Vec::with_capacity(4096) })
   }

   fn discover_services(&mut self) -> Result<Vec<Uuid>>
   {
      let reply = self.request(DirconFrame::request(MsgType::DiscoverServices, Vec::new()))?;
      if reply.payload.len() % 16 != 0
      {
         bail!("DiscoverServices payload length {} is not a multiple of 16", reply.payload.len());
      }
      let mut services = Vec::new();
      for chunk in reply.payload.as_chunks::<16>().0
      {
         services.push(uuid_from_bytes(chunk)?);
      }
      Ok(services)
   }

   fn discover_chars(&mut self, service_uuid: Uuid) -> Result<Vec<DiscoveredCharacteristic>>
   {
      let reply = self.request(DirconFrame::request(MsgType::DiscoverChars, uuid_to_bytes(service_uuid).to_vec()))?;
      if reply.payload.len() < 16
      {
         bail!("DiscoverChars reply too short: {} bytes", reply.payload.len());
      }
      let echoed = uuid_from_bytes(&reply.payload[..16])?;
      if echoed != service_uuid
      {
         bail!("DiscoverChars echoed service {} but requested {}", echoed, service_uuid);
      }
      let rest = &reply.payload[16..];
      if rest.len() % 17 != 0
      {
         bail!("DiscoverChars tail length {} is not a multiple of 17", rest.len());
      }
      let mut chars = Vec::new();
      for chunk in rest.as_chunks::<17>().0
      {
         chars.push(DiscoveredCharacteristic { uuid: uuid_from_bytes(&chunk[..16])?, flags: chunk[16] });
      }
      Ok(chars)
   }

   fn read_char(&mut self, uuid: Uuid) -> Result<Vec<u8>>
   {
      let reply = self.request(DirconFrame::request(MsgType::ReadChar, uuid_to_bytes(uuid).to_vec()))?;
      if reply.payload.len() < 16
      {
         bail!("ReadChar reply too short: {} bytes", reply.payload.len());
      }
      let echoed = uuid_from_bytes(&reply.payload[..16])?;
      if echoed != uuid
      {
         bail!("ReadChar echoed UUID {} but requested {}", echoed, uuid);
      }
      Ok(reply.payload[16..].to_vec())
   }

   fn subscribe(&mut self, uuid: Uuid) -> Result<()>
   {
      let reply = self.request(DirconFrame::request(MsgType::Subscribe, uuid_to_bytes(uuid).to_vec()))?;
      if reply.payload.len() < 16
      {
         bail!("Subscribe reply too short: {} bytes", reply.payload.len());
      }
      let echoed = uuid_from_bytes(&reply.payload[..16])?;
      if echoed != uuid
      {
         bail!("Subscribe echoed UUID {} but requested {}", echoed, uuid);
      }
      Ok(())
   }

   fn read_notifications(&mut self, duration: Duration) -> Result<()>
   {
      let deadline = Instant::now() + duration;
      while Instant::now() < deadline
      {
         let remaining = deadline.saturating_duration_since(Instant::now());
         self.stream.set_read_timeout(Some(remaining.min(Duration::from_millis(500))))?;
         match self.read_frame()
         {
            | Ok(frame) if frame.msg_type == MsgType::Notification =>
            {
               if frame.payload.len() < 16
               {
                  println!("notification with short payload: {} bytes", frame.payload.len());
                  continue;
               }
               let uuid = uuid_from_bytes(&frame.payload[..16])?;
               println!("notification seq={} {} {}", frame.seq, uuid, hex_bytes(&frame.payload[16..]));
            }
            | Ok(frame) =>
            {
               println!("ignoring unexpected frame {:?} seq={}", frame.msg_type, frame.seq);
            }
            | Err(err)
               if err.downcast_ref::<std::io::Error>()
                     .is_some_and(|io| matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)) =>
            {
               continue;
            }
            | Err(err) => return Err(err),
         }
      }
      Ok(())
   }

   fn request(&mut self, frame: DirconFrame) -> Result<DirconFrame>
   {
      let expected = frame.msg_type;
      let bytes = frame.serialize();
      self.stream.write_all(&bytes)?;

      loop
      {
         let reply = self.read_frame()?;
         match (expected, reply.msg_type)
         {
            | (_, MsgType::Notification) =>
            {
               if reply.payload.len() >= 16
               {
                  let uuid = uuid_from_bytes(&reply.payload[..16])?;
                  println!("notification seq={} {} {}", reply.seq, uuid, hex_bytes(&reply.payload[16..]));
               }
            }
            | (want, got) if want == got => return Ok(reply),
            | (_, got) => bail!("unexpected reply type {:?} while waiting for {:?}", got, expected),
         }
      }
   }

   fn read_frame(&mut self) -> Result<DirconFrame>
   {
      let mut temp = [0u8; 2048];
      loop
      {
         if let Some((frame, consumed)) = DirconFrame::parse(&self.read_buf)?
         {
            self.read_buf.drain(..consumed);
            return Ok(frame);
         }

         let n = self.stream.read(&mut temp)?;
         if n == 0
         {
            bail!("DIRCON server closed the TCP connection");
         }
         self.read_buf.extend_from_slice(&temp[..n]);
      }
   }
}

#[derive(Debug, Clone)]
struct DiscoveredCharacteristic
{
   uuid:  Uuid,
   flags: u8,
}

mod char_flags
{
   pub const READ: u8 = 0x01;
   pub const WRITE: u8 = 0x02;
   pub const NOTIFY: u8 = 0x04;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum MsgType
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
   type Error = anyhow::Error;

   fn try_from(value: u8) -> Result<Self>
   {
      match value
      {
         | 0x01 => Ok(Self::DiscoverServices),
         | 0x02 => Ok(Self::DiscoverChars),
         | 0x03 => Ok(Self::ReadChar),
         | 0x04 => Ok(Self::WriteChar),
         | 0x05 => Ok(Self::Subscribe),
         | 0x06 => Ok(Self::Notification),
         | _ => bail!("unknown DIRCON message type 0x{value:02X}"),
      }
   }
}

#[derive(Debug, Clone)]
struct DirconFrame
{
   msg_type: MsgType,
   seq:      u16,
   payload:  Vec<u8>,
}

impl DirconFrame
{
   const HEADER_LEN: usize = 6;

   fn request(msg_type: MsgType, payload: Vec<u8>) -> Self
   {
      Self { msg_type, seq: 0, payload }
   }

   fn serialize(&self) -> Vec<u8>
   {
      let mut out = Vec::with_capacity(Self::HEADER_LEN + self.payload.len());
      out.push(0x01);
      out.push(self.msg_type as u8);
      out.extend_from_slice(&self.seq.to_le_bytes());
      out.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
      out.extend_from_slice(&self.payload);
      out
   }

   fn parse(buf: &[u8]) -> Result<Option<(Self, usize)>>
   {
      if buf.len() < Self::HEADER_LEN
      {
         return Ok(None);
      }
      if buf[0] != 0x01
      {
         bail!("unsupported DIRCON version byte 0x{:02X}", buf[0]);
      }
      let msg_type = MsgType::try_from(buf[1])?;
      let seq = u16::from_le_bytes([buf[2], buf[3]]);
      let len = u16::from_be_bytes([buf[4], buf[5]]) as usize;
      let total = Self::HEADER_LEN + len;
      if buf.len() < total
      {
         return Ok(None);
      }
      Ok(Some((Self { msg_type, seq, payload: buf[6..total].to_vec() }, total)))
   }
}

fn uuid_to_bytes(uuid: Uuid) -> [u8; 16] { *uuid.as_bytes() }

fn uuid_from_bytes(bytes: &[u8]) -> Result<Uuid>
{
   let arr: [u8; 16] = bytes.get(..16)
                             .ok_or_else(|| anyhow!("expected 16 bytes for UUID, got {}", bytes.len()))?
                             .try_into()
                             .map_err(|_| anyhow!("invalid UUID byte slice"))?;
   Ok(Uuid::from_bytes(arr))
}

fn uuid16(short: u16) -> Uuid
{
   Uuid::from_u128(0x0000_0000_0000_1000_8000_0080_5f9b_34fb | ((short as u128) << 96))
}

fn hex_bytes(bytes: &[u8]) -> String
{
   if bytes.is_empty()
   {
      return "<empty>".to_string();
   }
   bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

fn build_ptr_query(service_type: &str) -> Vec<u8>
{
   let mut out = Vec::new();
   out.extend_from_slice(&0u16.to_be_bytes());
   out.extend_from_slice(&0u16.to_be_bytes());
   out.extend_from_slice(&1u16.to_be_bytes());
   out.extend_from_slice(&0u16.to_be_bytes());
   out.extend_from_slice(&0u16.to_be_bytes());
   out.extend_from_slice(&0u16.to_be_bytes());
   encode_name(service_type, &mut out);
   out.extend_from_slice(&12u16.to_be_bytes());
   out.extend_from_slice(&1u16.to_be_bytes());
   out
}

fn encode_name(name: &str, out: &mut Vec<u8>)
{
   for label in name.trim_end_matches('.').split('.')
   {
      out.push(label.len() as u8);
      out.extend_from_slice(label.as_bytes());
   }
   out.push(0);
}

#[derive(Debug)]
struct DnsPacket
{
   records: Vec<DnsRecord>,
}

impl DnsPacket
{
   fn parse(buf: &[u8]) -> Result<Self>
   {
      if buf.len() < 12
      {
         bail!("DNS packet too short: {} bytes", buf.len());
      }
      let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
      let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
      let nscount = u16::from_be_bytes([buf[8], buf[9]]) as usize;
      let arcount = u16::from_be_bytes([buf[10], buf[11]]) as usize;
      let mut offset = 12;

      for _ in 0..qdcount
      {
         let _ = parse_name(buf, &mut offset)?;
         offset += 4;
         if offset > buf.len()
         {
            bail!("DNS question truncated");
         }
      }

      let total = ancount + nscount + arcount;
      let mut records = Vec::with_capacity(total);
      for _ in 0..total
      {
         records.push(parse_record(buf, &mut offset)?);
      }

      Ok(Self { records })
   }
}

#[derive(Debug)]
struct DnsRecord
{
   name: String,
   data: DnsRData,
}

#[derive(Debug)]
enum DnsRData
{
   Ptr(String),
   Srv { port: u16, target: String },
   Txt(BTreeMap<String, String>),
   A(Ipv4Addr),
   Other,
}

fn parse_record(buf: &[u8], offset: &mut usize) -> Result<DnsRecord>
{
   let name = parse_name(buf, offset)?;
   if *offset + 10 > buf.len()
   {
      bail!("DNS resource record header truncated");
   }
   let rr_type = u16::from_be_bytes([buf[*offset], buf[*offset + 1]]);
   let _class = u16::from_be_bytes([buf[*offset + 2], buf[*offset + 3]]);
   let _ttl = u32::from_be_bytes([buf[*offset + 4], buf[*offset + 5], buf[*offset + 6], buf[*offset + 7]]);
   let rdlen = u16::from_be_bytes([buf[*offset + 8], buf[*offset + 9]]) as usize;
   *offset += 10;
   if *offset + rdlen > buf.len()
   {
      bail!("DNS resource record rdata truncated");
   }

   let data = match rr_type
   {
      | 1 if rdlen == 4 => DnsRData::A(Ipv4Addr::new(buf[*offset], buf[*offset + 1], buf[*offset + 2], buf[*offset + 3])),
      | 12 =>
      {
         let mut nested = *offset;
         DnsRData::Ptr(parse_name(buf, &mut nested)?)
      }
      | 16 => DnsRData::Txt(parse_txt(&buf[*offset..*offset + rdlen])),
      | 33 =>
      {
         if rdlen < 6
         {
            bail!("SRV record too short: {rdlen}");
         }
         let _priority = u16::from_be_bytes([buf[*offset], buf[*offset + 1]]);
         let _weight = u16::from_be_bytes([buf[*offset + 2], buf[*offset + 3]]);
         let port = u16::from_be_bytes([buf[*offset + 4], buf[*offset + 5]]);
         let mut nested = *offset + 6;
         let target = parse_name(buf, &mut nested)?;
         DnsRData::Srv { port, target }
      }
      | _ => DnsRData::Other,
   };

   *offset += rdlen;
   Ok(DnsRecord { name, data })
}

fn parse_txt(buf: &[u8]) -> BTreeMap<String, String>
{
   let mut out = BTreeMap::new();
   let mut offset = 0;
   while offset < buf.len()
   {
      let len = buf[offset] as usize;
      offset += 1;
      if offset + len > buf.len()
      {
         break;
      }
      let item = String::from_utf8_lossy(&buf[offset..offset + len]);
      offset += len;
      let mut parts = item.splitn(2, '=');
      let key = parts.next().unwrap_or_default().to_string();
      let value = parts.next().unwrap_or_default().to_string();
      out.insert(key, value);
   }
   out
}

fn parse_name(buf: &[u8], offset: &mut usize) -> Result<String>
{
   let mut labels = Vec::new();
   let mut cursor = *offset;
   let mut jumped = false;
   let mut seen = 0;

   loop
   {
      if cursor >= buf.len()
      {
         bail!("DNS name exceeds packet length");
      }
      if seen > buf.len()
      {
         bail!("DNS compression pointer loop detected");
      }

      let len = buf[cursor];
      if len & 0xC0 == 0xC0
      {
         if cursor + 1 >= buf.len()
         {
            bail!("DNS compression pointer truncated");
         }
         let ptr = (((len as usize) & 0x3F) << 8) | buf[cursor + 1] as usize;
         if !jumped
         {
            *offset = cursor + 2;
         }
         cursor = ptr;
         jumped = true;
         seen += 2;
         continue;
      }
      if len == 0
      {
         if !jumped
         {
            *offset = cursor + 1;
         }
         break;
      }

      cursor += 1;
      let end = cursor + len as usize;
      if end > buf.len()
      {
         bail!("DNS label truncated");
      }
      labels.push(std::str::from_utf8(&buf[cursor..end]).context("DNS label is not valid UTF-8")?.to_string());
      cursor = end;
      if !jumped
      {
         *offset = cursor;
      }
      seen += 1 + len as usize;
   }

   Ok(labels.join("."))
}

#[cfg(test)]
mod tests
{
   use super::*;

   #[test]
   fn dircon_frame_round_trip()
   {
      let frame = DirconFrame::request(MsgType::DiscoverServices, vec![0xAA, 0xBB]);
      let bytes = frame.serialize();
      let (parsed, consumed) = DirconFrame::parse(&bytes).unwrap().unwrap();
      assert_eq!(consumed, bytes.len());
      assert_eq!(parsed.msg_type, MsgType::DiscoverServices);
      assert_eq!(parsed.seq, 0);
      assert_eq!(parsed.payload, vec![0xAA, 0xBB]);
   }

   #[test]
   fn parse_compressed_dns_name()
   {
      let packet = [0x05, b'h', b'o', b's', b't', b'1',
                    0x05, b'l', b'o', b'c', b'a', b'l',
                    0x00,
                    0xC0, 0x00];
      let mut offset = 13;
      let name = parse_name(&packet, &mut offset).unwrap();
      assert_eq!(name, "host1.local");
      assert_eq!(offset, 15);
   }

   #[test]
   fn parse_txt_properties()
   {
      let txt = [0x0F, b's', b'e', b'r', b'i', b'a', b'l', b'-', b'n', b'u', b'm', b'b', b'e', b'r', b'=', b'x',
              0x0A, b'm', b'a', b'c', b'-', b'a', b'd', b'd', b'r', b'=', b'y'];
      let parsed = parse_txt(&txt);
      assert_eq!(parsed.get("serial-number").unwrap(), "x");
      assert_eq!(parsed.get("mac-addr").unwrap(), "y");
   }

   #[test]
   fn uuid_parser_accepts_short_and_full_forms()
   {
      assert_eq!(parse_uuid("0x1818").unwrap(), uuid16(0x1818));
      assert_eq!(parse_uuid("00001818-0000-1000-8000-00805f9b34fb").unwrap(), uuid16(0x1818));
   }
}
