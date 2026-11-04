//! Minimal mDNS (RFC 6762) + DNS-SD (RFC 6763) codec: enough to answer for
//! `<host>.local`, advertise the local web UI as `_http._tcp`, and browse the
//! LAN for the farm server's `_farmos._tcp` service. No allocation, no
//! sockets; the firmware feeds packets in and sends the bytes it gets back.
//!
//! Wire format recap (RFC 1035): 12-byte header, then questions
//! (`name type class`), then resource records (`name type class ttl rdlen
//! rdata`). Names are length-prefixed labels; a label byte with the top two
//! bits set is a pointer back into the packet (compression). Outgoing packets
//! never use compression; incoming ones must handle it.

use heapless::{String, Vec};

pub const MDNS_PORT: u16 = 5353;
pub const MDNS_GROUP: [u8; 4] = [224, 0, 0, 251];

/// TTLs recommended by RFC 6762 §10: host records short, service records long.
const TTL_HOST: u32 = 120;
const TTL_SERVICE: u32 = 4500;

pub const TYPE_A: u16 = 1;
pub const TYPE_PTR: u16 = 12;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_SRV: u16 = 33;
pub const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
/// Top bit of the class field: cache-flush in answers, unicast-response in questions.
const CLASS_TOP_BIT: u16 = 0x8000;
const FLAG_RESPONSE: u16 = 0x8000;
const FLAG_AUTHORITATIVE: u16 = 0x0400;

const MAX_NAME: usize = 128;
const MAX_POINTER_HOPS: usize = 16;
/// Longest TXT value we keep (the server's `base=` path).
pub const MAX_BASE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Packet ended in the middle of a field.
    Truncated,
    /// Compression pointer loop or forward pointer.
    BadPointer,
    /// Name or label longer than the limits.
    NameTooLong,
    /// Output buffer too small.
    NoSpace,
}

// ---------------------------------------------------------------------------
// Names

/// A domain name in uncompressed wire form (`\x04tank\x05local\x00`).
#[derive(Debug, Clone, Default)]
pub struct Name {
    wire: Vec<u8, MAX_NAME>,
}

impl Name {
    /// Build from labels, e.g. `["tank-ece4", "local"]`. Labels may contain
    /// any byte except they must be 1..=63 long; this is what lets a service
    /// instance be called "Farm server" without escaping rules.
    pub fn from_labels(labels: &[&str]) -> Result<Name, Error> {
        let mut wire = Vec::new();
        for l in labels {
            let b = l.as_bytes();
            if b.is_empty() || b.len() > 63 {
                return Err(Error::NameTooLong);
            }
            wire.push(b.len() as u8).map_err(|_| Error::NameTooLong)?;
            wire.extend_from_slice(b).map_err(|_| Error::NameTooLong)?;
        }
        wire.push(0).map_err(|_| Error::NameTooLong)?;
        Ok(Name { wire })
    }

    /// Build from dotted text (`"_farmos._tcp.local"`). No escaping.
    pub fn from_dotted(s: &str) -> Result<Name, Error> {
        let mut wire = Vec::new();
        for l in s.split('.').filter(|l| !l.is_empty()) {
            let b = l.as_bytes();
            if b.len() > 63 {
                return Err(Error::NameTooLong);
            }
            wire.push(b.len() as u8).map_err(|_| Error::NameTooLong)?;
            wire.extend_from_slice(b).map_err(|_| Error::NameTooLong)?;
        }
        wire.push(0).map_err(|_| Error::NameTooLong)?;
        Ok(Name { wire })
    }

    /// `prefix` label(s) followed by the labels of `suffix`: `"tank"` + `local`.
    pub fn prepend(label: &str, suffix: &Name) -> Result<Name, Error> {
        let b = label.as_bytes();
        if b.is_empty() || b.len() > 63 {
            return Err(Error::NameTooLong);
        }
        let mut wire = Vec::new();
        wire.push(b.len() as u8).map_err(|_| Error::NameTooLong)?;
        wire.extend_from_slice(b).map_err(|_| Error::NameTooLong)?;
        wire.extend_from_slice(&suffix.wire).map_err(|_| Error::NameTooLong)?;
        Ok(Name { wire })
    }

    /// Decode a possibly compressed name at `off`. Returns the name and the
    /// offset just past it in the packet (not past any pointer target).
    pub fn parse(packet: &[u8], mut off: usize) -> Result<(Name, usize), Error> {
        let mut wire: Vec<u8, MAX_NAME> = Vec::new();
        let mut end: Option<usize> = None;
        let mut hops = 0;
        loop {
            let len = *packet.get(off).ok_or(Error::Truncated)? as usize;
            match len {
                0 => {
                    wire.push(0).map_err(|_| Error::NameTooLong)?;
                    let next = end.unwrap_or(off + 1);
                    return Ok((Name { wire }, next));
                }
                1..=63 => {
                    let label = packet.get(off + 1..off + 1 + len).ok_or(Error::Truncated)?;
                    wire.push(len as u8).map_err(|_| Error::NameTooLong)?;
                    wire.extend_from_slice(label).map_err(|_| Error::NameTooLong)?;
                    off += 1 + len;
                }
                l if l & 0xC0 == 0xC0 => {
                    let lo = *packet.get(off + 1).ok_or(Error::Truncated)? as usize;
                    let target = ((l & 0x3F) << 8) | lo;
                    // Pointers must go backwards; this also kills loops.
                    if target >= off {
                        return Err(Error::BadPointer);
                    }
                    hops += 1;
                    if hops > MAX_POINTER_HOPS {
                        return Err(Error::BadPointer);
                    }
                    end.get_or_insert(off + 2);
                    off = target;
                }
                _ => return Err(Error::BadPointer),
            }
        }
    }

    pub fn wire(&self) -> &[u8] {
        &self.wire
    }

    pub fn labels(&self) -> Labels<'_> {
        Labels { wire: &self.wire, off: 0 }
    }

    /// Case-insensitive comparison (DNS names are).
    pub fn eq_ignore_case(&self, other: &Name) -> bool {
        self.wire.len() == other.wire.len()
            && self.wire.iter().zip(other.wire.iter()).all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// `self` is `<one label>.<suffix>`.
    pub fn is_child_of(&self, suffix: &Name) -> bool {
        let first = self.wire.first().copied().unwrap_or(0) as usize;
        first > 0 && self.wire.len() == 1 + first + suffix.wire.len() && {
            let rest = &self.wire[1 + first..];
            rest.iter().zip(suffix.wire.iter()).all(|(a, b)| a.eq_ignore_ascii_case(b))
        }
    }

    /// Dotted text for logs and UIs, truncated to the string's capacity.
    pub fn to_dotted<const N: usize>(&self) -> String<N> {
        let mut s = String::new();
        for (i, l) in self.labels().enumerate() {
            if i > 0 {
                let _ = s.push('.');
            }
            for &b in l {
                let _ = s.push(if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' });
            }
        }
        s
    }
}

pub struct Labels<'a> {
    wire: &'a [u8],
    off: usize,
}

impl<'a> Iterator for Labels<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        let len = *self.wire.get(self.off)? as usize;
        if len == 0 {
            return None;
        }
        let l = self.wire.get(self.off + 1..self.off + 1 + len)?;
        self.off += 1 + len;
        Some(l)
    }
}

// ---------------------------------------------------------------------------
// Reading

fn u16_at(p: &[u8], off: usize) -> Result<u16, Error> {
    let b = p.get(off..off + 2).ok_or(Error::Truncated)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn u32_at(p: &[u8], off: usize) -> Result<u32, Error> {
    let b = p.get(off..off + 4).ok_or(Error::Truncated)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[derive(Debug, Clone)]
pub struct Question {
    pub name: Name,
    pub qtype: u16,
    /// Querier asked for a unicast reply (QU bit).
    pub unicast: bool,
}

#[derive(Debug, Clone)]
pub struct Record<'p> {
    pub name: Name,
    pub rtype: u16,
    pub ttl: u32,
    pub rdata: &'p [u8],
    /// Offset of `rdata` in the packet, needed to follow compression
    /// pointers inside PTR/SRV data.
    pub rdata_off: usize,
}

/// A parsed packet: header fields plus iterators over its sections.
pub struct Message<'p> {
    packet: &'p [u8],
    pub id: u16,
    pub is_response: bool,
    pub qdcount: u16,
    rrcount: u16,
    records_off: usize,
}

impl<'p> Message<'p> {
    pub fn parse(packet: &'p [u8]) -> Result<Message<'p>, Error> {
        if packet.len() < 12 {
            return Err(Error::Truncated);
        }
        let id = u16_at(packet, 0)?;
        let flags = u16_at(packet, 2)?;
        let qdcount = u16_at(packet, 4)?;
        let rrcount = u16_at(packet, 6)?
            .saturating_add(u16_at(packet, 8)?)
            .saturating_add(u16_at(packet, 10)?);
        // Walk the questions once to find where records start.
        let mut off = 12;
        for _ in 0..qdcount {
            let (_, next) = Name::parse(packet, off)?;
            off = next + 4;
        }
        if off > packet.len() {
            return Err(Error::Truncated);
        }
        Ok(Message {
            packet,
            id,
            is_response: flags & FLAG_RESPONSE != 0,
            qdcount,
            rrcount,
            records_off: off,
        })
    }

    pub fn questions(&self) -> Questions<'p> {
        Questions {
            packet: self.packet,
            off: 12,
            left: self.qdcount,
        }
    }

    /// All resource records (answer, authority and additional sections).
    pub fn records(&self) -> Records<'p> {
        Records {
            packet: self.packet,
            off: self.records_off,
            left: self.rrcount,
        }
    }
}

pub struct Questions<'p> {
    packet: &'p [u8],
    off: usize,
    left: u16,
}

impl<'p> Iterator for Questions<'p> {
    type Item = Result<Question, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        Some((|| {
            let (name, next) = Name::parse(self.packet, self.off)?;
            let qtype = u16_at(self.packet, next)?;
            let qclass = u16_at(self.packet, next + 2)?;
            self.off = next + 4;
            Ok(Question {
                name,
                qtype,
                unicast: qclass & CLASS_TOP_BIT != 0,
            })
        })())
    }
}

pub struct Records<'p> {
    packet: &'p [u8],
    off: usize,
    left: u16,
}

impl<'p> Iterator for Records<'p> {
    type Item = Result<Record<'p>, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        Some((|| {
            let (name, next) = Name::parse(self.packet, self.off)?;
            let rtype = u16_at(self.packet, next)?;
            let ttl = u32_at(self.packet, next + 4)?;
            let rdlen = u16_at(self.packet, next + 8)? as usize;
            let rdata_off = next + 10;
            let rdata = self.packet.get(rdata_off..rdata_off + rdlen).ok_or(Error::Truncated)?;
            self.off = rdata_off + rdlen;
            Ok(Record {
                name,
                rtype,
                ttl,
                rdata,
                rdata_off,
            })
        })())
    }
}

impl<'p> Record<'p> {
    /// The name inside PTR rdata, or the target inside SRV rdata.
    fn rdata_name(&self, packet: &[u8], skip: usize) -> Result<Name, Error> {
        Ok(Name::parse(packet, self.rdata_off + skip)?.0)
    }

    /// SRV `(port, target)`.
    fn srv(&self, packet: &[u8]) -> Result<(u16, Name), Error> {
        let port = u16_at(self.rdata, 4)?;
        Ok((port, self.rdata_name(packet, 6)?))
    }

    fn a(&self) -> Option<[u8; 4]> {
        self.rdata.try_into().ok()
    }

    /// Value of `key=` in TXT rdata (first match).
    fn txt_value(&self, key: &str) -> Option<&'p [u8]> {
        let mut off = 0;
        while let Some(&len) = self.rdata.get(off) {
            let item = self.rdata.get(off + 1..off + 1 + len as usize)?;
            off += 1 + len as usize;
            if item.len() > key.len() && item[..key.len()].eq_ignore_ascii_case(key.as_bytes()) && item[key.len()] == b'=' {
                return Some(&item[key.len() + 1..]);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Writing

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
    answers: u16,
    additionals: u16,
}

impl<'a> Writer<'a> {
    fn response(buf: &'a mut [u8]) -> Result<Writer<'a>, Error> {
        let mut w = Writer {
            buf,
            pos: 0,
            answers: 0,
            additionals: 0,
        };
        w.u16(0)?; // id: always 0 in multicast responses
        w.u16(FLAG_RESPONSE | FLAG_AUTHORITATIVE)?;
        w.u16(0)?; // qd
        w.u16(0)?; // an, patched in finish()
        w.u16(0)?; // ns
        w.u16(0)?; // ar, patched in finish()
        Ok(w)
    }

    fn query(buf: &'a mut [u8], questions: u16) -> Result<Writer<'a>, Error> {
        let mut w = Writer {
            buf,
            pos: 0,
            answers: 0,
            additionals: 0,
        };
        w.u16(0)?;
        w.u16(0)?;
        w.u16(questions)?;
        w.u16(0)?;
        w.u16(0)?;
        w.u16(0)?;
        Ok(w)
    }

    fn bytes(&mut self, b: &[u8]) -> Result<(), Error> {
        let dst = self.buf.get_mut(self.pos..self.pos + b.len()).ok_or(Error::NoSpace)?;
        dst.copy_from_slice(b);
        self.pos += b.len();
        Ok(())
    }
    fn u8(&mut self, v: u8) -> Result<(), Error> {
        self.bytes(&[v])
    }
    fn u16(&mut self, v: u16) -> Result<(), Error> {
        self.bytes(&v.to_be_bytes())
    }
    fn u32(&mut self, v: u32) -> Result<(), Error> {
        self.bytes(&v.to_be_bytes())
    }

    fn question(&mut self, name: &Name, qtype: u16) -> Result<(), Error> {
        self.bytes(name.wire())?;
        self.u16(qtype)?;
        self.u16(CLASS_IN)
    }

    /// Record header; returns the offset of the rdlength field to patch.
    fn rr_head(&mut self, name: &Name, rtype: u16, flush: bool, ttl: u32) -> Result<usize, Error> {
        self.bytes(name.wire())?;
        self.u16(rtype)?;
        self.u16(if flush { CLASS_IN | CLASS_TOP_BIT } else { CLASS_IN })?;
        self.u32(ttl)?;
        let at = self.pos;
        self.u16(0)?;
        Ok(at)
    }

    fn rr_finish(&mut self, rdlen_at: usize, additional: bool) {
        let len = (self.pos - rdlen_at - 2) as u16;
        self.buf[rdlen_at..rdlen_at + 2].copy_from_slice(&len.to_be_bytes());
        if additional {
            self.additionals += 1;
        } else {
            self.answers += 1;
        }
    }

    fn a(&mut self, name: &Name, ip: [u8; 4], additional: bool) -> Result<(), Error> {
        let at = self.rr_head(name, TYPE_A, true, TTL_HOST)?;
        self.bytes(&ip)?;
        self.rr_finish(at, additional);
        Ok(())
    }

    fn ptr(&mut self, name: &Name, target: &Name, additional: bool) -> Result<(), Error> {
        // Shared-name records (PTR) never carry cache-flush.
        let at = self.rr_head(name, TYPE_PTR, false, TTL_SERVICE)?;
        self.bytes(target.wire())?;
        self.rr_finish(at, additional);
        Ok(())
    }

    fn srv(&mut self, name: &Name, port: u16, target: &Name, additional: bool) -> Result<(), Error> {
        let at = self.rr_head(name, TYPE_SRV, true, TTL_HOST)?;
        self.u16(0)?; // priority
        self.u16(0)?; // weight
        self.u16(port)?;
        self.bytes(target.wire())?;
        self.rr_finish(at, additional);
        Ok(())
    }

    fn txt(&mut self, name: &Name, items: &[&str], additional: bool) -> Result<(), Error> {
        let at = self.rr_head(name, TYPE_TXT, true, TTL_SERVICE)?;
        if items.is_empty() {
            self.u8(0)?; // RFC 6763 §6.1: empty TXT is a single zero byte
        }
        for item in items {
            let b = item.as_bytes();
            if b.len() > 255 {
                return Err(Error::NameTooLong);
            }
            self.u8(b.len() as u8)?;
            self.bytes(b)?;
        }
        self.rr_finish(at, additional);
        Ok(())
    }

    fn finish(self) -> usize {
        self.buf[6..8].copy_from_slice(&self.answers.to_be_bytes());
        self.buf[10..12].copy_from_slice(&self.additionals.to_be_bytes());
        self.pos
    }
}

// ---------------------------------------------------------------------------
// Responder: `<host>.local` A record + `<host>._http._tcp.local` service

/// What this device advertises.
pub struct Host<'a> {
    /// Hostname label without `.local`, e.g. `tank-ece4`.
    pub hostname: &'a str,
    pub ip: [u8; 4],
    pub http_port: u16,
    /// TXT items for the `_http._tcp` service (`key=value`).
    pub txt: &'a [&'a str],
}

pub struct Reply {
    pub len: usize,
    /// Send directly to the querier instead of the multicast group.
    pub unicast: bool,
}

struct Names {
    host: Name,
    service: Name,
    instance: Name,
    enumeration: Name,
}

impl Names {
    fn for_host(h: &Host) -> Result<Names, Error> {
        let local = Name::from_labels(&["local"])?;
        let service = Name::from_labels(&["_http", "_tcp", "local"])?;
        Ok(Names {
            host: Name::prepend(h.hostname, &local)?,
            instance: Name::prepend(h.hostname, &service)?,
            enumeration: Name::from_labels(&["_services", "_dns-sd", "_udp", "local"])?,
            service,
        })
    }
}

#[derive(Default, Clone, Copy)]
struct Wanted {
    a: bool,
    ptr: bool,
    srv: bool,
    txt: bool,
    enumeration: bool,
}

fn write_records(w: &mut Writer, h: &Host, n: &Names, want: Wanted) -> Result<(), Error> {
    if want.enumeration {
        w.ptr(&n.enumeration, &n.service, false)?;
    }
    if want.ptr {
        w.ptr(&n.service, &n.instance, false)?;
    }
    if want.srv {
        w.srv(&n.instance, h.http_port, &n.host, false)?;
    }
    if want.txt {
        w.txt(&n.instance, h.txt, false)?;
    }
    if want.a {
        w.a(&n.host, h.ip, false)?;
    }
    // Additionals the querier will want next (RFC 6763 §12).
    if want.ptr {
        if !want.srv {
            w.srv(&n.instance, h.http_port, &n.host, true)?;
        }
        if !want.txt {
            w.txt(&n.instance, h.txt, true)?;
        }
    }
    if (want.ptr || want.srv) && !want.a {
        w.a(&n.host, h.ip, true)?;
    }
    Ok(())
}

/// Unsolicited announcement of everything (send on link-up, twice, 1 s apart).
pub fn announce(h: &Host, out: &mut [u8]) -> Result<usize, Error> {
    let n = Names::for_host(h)?;
    let mut w = Writer::response(out)?;
    write_records(
        &mut w,
        h,
        &n,
        Wanted {
            a: true,
            ptr: true,
            srv: true,
            txt: true,
            enumeration: true,
        },
    )?;
    Ok(w.finish())
}

/// Answer a query packet if it asks for anything of ours.
pub fn respond(h: &Host, query: &[u8], out: &mut [u8]) -> Result<Option<Reply>, Error> {
    let msg = Message::parse(query)?;
    if msg.is_response {
        return Ok(None);
    }
    let n = Names::for_host(h)?;
    let mut want = Wanted::default();
    let mut unicast = true;
    let mut any = false;
    for q in msg.questions() {
        let q = q?;
        let t = q.qtype;
        let is = |ty: u16| t == ty || t == TYPE_ANY;
        let hit = if q.name.eq_ignore_case(&n.host) && is(TYPE_A) {
            want.a = true;
            true
        } else if q.name.eq_ignore_case(&n.service) && is(TYPE_PTR) {
            want.ptr = true;
            true
        } else if q.name.eq_ignore_case(&n.instance) && (is(TYPE_SRV) || is(TYPE_TXT)) {
            want.srv |= is(TYPE_SRV);
            want.txt |= is(TYPE_TXT);
            true
        } else if q.name.eq_ignore_case(&n.enumeration) && is(TYPE_PTR) {
            want.enumeration = true;
            true
        } else {
            false
        };
        if hit {
            any = true;
            unicast &= q.unicast;
        }
    }
    if !any {
        return Ok(None);
    }
    let mut w = Writer::response(out)?;
    write_records(&mut w, h, &n, want)?;
    Ok(Some(Reply {
        len: w.finish(),
        unicast,
    }))
}

// ---------------------------------------------------------------------------
// Browser: find one `_<service>._tcp.local` instance → ip, port, TXT base

/// A discovered server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub ip: [u8; 4],
    pub port: u16,
    /// URL path prefix from TXT `base=` (normalised: leading `/`, no trailing `/`), or empty.
    pub base: String<MAX_BASE>,
}

/// Browse state: ask for PTR, then SRV/TXT, then A, keeping what each reply
/// brings. Feed every packet from the mDNS socket to [`Browser::ingest`];
/// call [`Browser::query`] periodically until [`Browser::result`] is `Some`.
pub struct Browser {
    service: Name,
    instance: Option<Name>,
    target: Option<Name>,
    port: Option<u16>,
    base: Option<String<MAX_BASE>>,
    ip: Option<[u8; 4]>,
}

impl Browser {
    /// `service` like `"_farmos._tcp"`.
    pub fn new(service: &str) -> Result<Browser, Error> {
        let mut dotted: String<64> = String::new();
        dotted.push_str(service).map_err(|_| Error::NameTooLong)?;
        dotted.push_str(".local").map_err(|_| Error::NameTooLong)?;
        Ok(Browser {
            service: Name::from_dotted(&dotted)?,
            instance: None,
            target: None,
            port: None,
            base: None,
            ip: None,
        })
    }

    pub fn reset(&mut self) {
        self.instance = None;
        self.target = None;
        self.port = None;
        self.base = None;
        self.ip = None;
    }

    pub fn instance(&self) -> Option<&Name> {
        self.instance.as_ref()
    }

    /// Build the next query for whatever is still missing.
    pub fn query(&self, out: &mut [u8]) -> Result<usize, Error> {
        match (&self.instance, &self.target) {
            (None, _) => {
                let mut w = Writer::query(out, 1)?;
                w.question(&self.service, TYPE_PTR)?;
                Ok(w.finish())
            }
            (Some(inst), None) => {
                let mut w = Writer::query(out, 2)?;
                w.question(inst, TYPE_SRV)?;
                w.question(inst, TYPE_TXT)?;
                Ok(w.finish())
            }
            (Some(inst), Some(target)) => {
                let mut w = Writer::query(out, if self.base.is_none() { 2 } else { 1 })?;
                w.question(target, TYPE_A)?;
                if self.base.is_none() {
                    w.question(inst, TYPE_TXT)?;
                }
                Ok(w.finish())
            }
        }
    }

    /// Absorb a packet. Returns `true` if something new was learned.
    pub fn ingest(&mut self, packet: &[u8]) -> Result<bool, Error> {
        let msg = Message::parse(packet)?;
        if !msg.is_response {
            return Ok(false);
        }
        let mut changed = false;
        // Two passes so record order in the packet does not matter
        // (PTR → instance → SRV → target → A).
        for _ in 0..3 {
            for r in msg.records() {
                let r = r?;
                if r.ttl == 0 {
                    continue; // goodbye
                }
                match r.rtype {
                    TYPE_PTR if self.instance.is_none() && r.name.eq_ignore_case(&self.service) => {
                        let inst = r.rdata_name(packet, 0)?;
                        if inst.is_child_of(&self.service) {
                            self.instance = Some(inst);
                            changed = true;
                        }
                    }
                    TYPE_SRV if self.instance.as_ref().is_some_and(|i| i.eq_ignore_case(&r.name)) => {
                        let (port, target) = r.srv(packet)?;
                        if self.port != Some(port) || self.target.as_ref().is_none_or(|t| !t.eq_ignore_case(&target)) {
                            self.port = Some(port);
                            self.target = Some(target);
                            self.ip = None;
                            changed = true;
                        }
                    }
                    TYPE_TXT if self.instance.as_ref().is_some_and(|i| i.eq_ignore_case(&r.name)) => {
                        let base = normalise_base(r.txt_value("base").unwrap_or(b""));
                        if self.base.as_ref() != Some(&base) {
                            self.base = Some(base);
                            changed = true;
                        }
                    }
                    TYPE_A if self.target.as_ref().is_some_and(|t| t.eq_ignore_case(&r.name)) => {
                        if let Some(ip) = r.a() {
                            if self.ip != Some(ip) {
                                self.ip = Some(ip);
                                changed = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(changed)
    }

    pub fn result(&self) -> Option<Server> {
        Some(Server {
            ip: self.ip?,
            port: self.port?,
            base: self.base.clone()?,
        })
    }
}

/// `base=` value → `/api/v1` form; junk becomes empty.
fn normalise_base(raw: &[u8]) -> String<MAX_BASE> {
    let mut s: String<MAX_BASE> = String::new();
    let raw = core::str::from_utf8(raw).unwrap_or("");
    let trimmed = raw.trim().trim_matches('/');
    if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_graphic()) {
        if s.push('/').is_ok() && s.push_str(trimmed).is_err() {
            s.clear();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;
    use std::vec;
    use super::*;

    fn host<'a>(txt: &'a [&'a str]) -> Host<'a> {
        Host {
            hostname: "tank-ece4",
            ip: [192, 168, 50, 34],
            http_port: 80,
            txt,
        }
    }

    fn names(p: &[u8], msg: &Message) -> std::vec::Vec<(std::string::String, u16, u32)> {
        let _ = p;
        msg.records()
            .map(|r| {
                let r = r.unwrap();
                (r.name.to_dotted::<64>().to_string(), r.rtype, r.ttl)
            })
            .collect()
    }

    #[test]
    fn name_roundtrip_and_compare() {
        let n = Name::from_dotted("Tank-ECE4.local").unwrap();
        assert_eq!(n.wire(), b"\x09Tank-ECE4\x05local\x00");
        let (back, next) = Name::parse(n.wire(), 0).unwrap();
        assert_eq!(next, n.wire().len());
        assert!(back.eq_ignore_case(&Name::from_dotted("tank-ece4.LOCAL").unwrap()));
        assert!(!back.eq_ignore_case(&Name::from_dotted("tank-ece5.local").unwrap()));
        assert_eq!(back.to_dotted::<32>(), "Tank-ECE4.local");
        let svc = Name::from_dotted("_farmos._tcp.local").unwrap();
        assert!(Name::prepend("Farm server", &svc).unwrap().is_child_of(&svc));
        assert!(!svc.is_child_of(&svc));
    }

    #[test]
    fn name_compression_pointers() {
        // "a.local" at 0, then a pointer-only name at 9 → "b" + pointer to "local" at 2.
        let mut p = std::vec::Vec::new();
        p.extend_from_slice(b"\x01a\x05local\x00"); // 0..9
        p.extend_from_slice(b"\x01b\xC0\x02"); // 9..13
        let (n, next) = Name::parse(&p, 9).unwrap();
        assert_eq!(next, 13);
        assert_eq!(n.to_dotted::<16>(), "b.local");
        // Forward / self pointer is rejected rather than looping.
        let bad = b"\xC0\x00";
        assert_eq!(Name::parse(bad, 0).unwrap_err(), Error::BadPointer);
        let truncated = b"\x05loc";
        assert_eq!(Name::parse(truncated, 0).unwrap_err(), Error::Truncated);
    }

    #[test]
    fn announce_contains_everything() {
        let mut buf = [0u8; 512];
        let txt = ["path=/", "id=f0f5bd0bece4"];
        let len = announce(&host(&txt), &mut buf).unwrap();
        let msg = Message::parse(&buf[..len]).unwrap();
        assert!(msg.is_response);
        assert_eq!(msg.qdcount, 0);
        let rr = names(&buf, &msg);
        assert_eq!(
            rr,
            vec![
                ("_services._dns-sd._udp.local".into(), TYPE_PTR, TTL_SERVICE),
                ("_http._tcp.local".into(), TYPE_PTR, TTL_SERVICE),
                ("tank-ece4._http._tcp.local".into(), TYPE_SRV, TTL_HOST),
                ("tank-ece4._http._tcp.local".into(), TYPE_TXT, TTL_SERVICE),
                ("tank-ece4.local".into(), TYPE_A, TTL_HOST),
            ]
        );
        // And our own browser can read it back as if it were a server.
        let mut b = Browser::new("_http._tcp").unwrap();
        assert!(b.ingest(&buf[..len]).unwrap());
        assert_eq!(
            b.result(),
            Some(Server {
                ip: [192, 168, 50, 34],
                port: 80,
                base: String::new()
            })
        );
    }

    fn query_for(name: &str, qtype: u16, unicast: bool) -> std::vec::Vec<u8> {
        let mut buf = [0u8; 256];
        let n = Name::from_dotted(name).unwrap();
        let mut w = Writer::query(&mut buf, 1).unwrap();
        w.bytes(n.wire()).unwrap();
        w.u16(qtype).unwrap();
        w.u16(if unicast { CLASS_IN | CLASS_TOP_BIT } else { CLASS_IN }).unwrap();
        let len = w.finish();
        buf[..len].to_vec()
    }

    #[test]
    fn responds_only_to_our_names() {
        let txt = [];
        let h = host(&txt);
        let mut out = [0u8; 512];

        let q = query_for("tank-ece4.local", TYPE_A, false);
        let r = respond(&h, &q, &mut out).unwrap().unwrap();
        assert!(!r.unicast);
        let msg = Message::parse(&out[..r.len]).unwrap();
        assert_eq!(names(&out, &msg), vec![("tank-ece4.local".into(), TYPE_A, TTL_HOST)]);
        let rec = msg.records().next().unwrap().unwrap();
        assert_eq!(rec.a(), Some([192, 168, 50, 34]));

        // Case-insensitive, QU bit honoured.
        let q = query_for("TANK-ECE4.LOCAL", TYPE_ANY, true);
        assert!(respond(&h, &q, &mut out).unwrap().unwrap().unicast);

        // Someone else's host: silence.
        let q = query_for("printer.local", TYPE_A, false);
        assert!(respond(&h, &q, &mut out).unwrap().is_none());
        // Our name but a type we don't have (AAAA = 28): silence.
        let q = query_for("tank-ece4.local", 28, false);
        assert!(respond(&h, &q, &mut out).unwrap().is_none());

        // Service browse → PTR answer plus SRV/TXT/A additionals.
        let q = query_for("_http._tcp.local", TYPE_PTR, false);
        let r = respond(&h, &q, &mut out).unwrap().unwrap();
        let msg = Message::parse(&out[..r.len]).unwrap();
        let types: std::vec::Vec<u16> = names(&out, &msg).iter().map(|x| x.1).collect();
        assert_eq!(types, vec![TYPE_PTR, TYPE_SRV, TYPE_TXT, TYPE_A]);
        assert_eq!(u16_at(&out, 6).unwrap(), 1, "one answer");
        assert_eq!(u16_at(&out, 10).unwrap(), 3, "three additionals");

        // Service type enumeration.
        let q = query_for("_services._dns-sd._udp.local", TYPE_PTR, false);
        let r = respond(&h, &q, &mut out).unwrap().unwrap();
        let msg = Message::parse(&out[..r.len]).unwrap();
        let rec = msg.records().next().unwrap().unwrap();
        assert_eq!(rec.rdata_name(&out, 0).unwrap().to_dotted::<32>(), "_http._tcp.local");

        // A response packet is never answered (no reply storms).
        let mut ann = [0u8; 512];
        let len = announce(&h, &mut ann).unwrap();
        assert!(respond(&h, &ann[..len], &mut out).unwrap().is_none());
    }

    /// Hand-built Avahi-style response with compression pointers in SRV and PTR rdata.
    fn farmos_reply(with_txt: bool, with_a: bool) -> std::vec::Vec<u8> {
        let mut p = std::vec::Vec::new();
        p.extend_from_slice(&[0, 0, 0x84, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let n_service = p.len(); // 12
        p.extend_from_slice(b"\x07_farmos\x04_tcp\x05local\x00");
        let n_local = n_service + 13;
        p.extend_from_slice(&TYPE_PTR.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&4500u32.to_be_bytes());
        p.extend_from_slice(&14u16.to_be_bytes()); // "\x0bFarm server" + pointer
        let n_instance = p.len();
        p.extend_from_slice(b"\x0bFarm server");
        p.extend_from_slice(&[0xC0, n_service as u8]);
        let mut count = 1u16;
        // SRV
        p.extend_from_slice(&[0xC0, n_instance as u8]);
        p.extend_from_slice(&TYPE_SRV.to_be_bytes());
        p.extend_from_slice(&0x8001u16.to_be_bytes());
        p.extend_from_slice(&120u32.to_be_bytes());
        p.extend_from_slice(&(6 + 9u16).to_be_bytes());
        p.extend_from_slice(&[0, 0, 0, 0]);
        p.extend_from_slice(&3000u16.to_be_bytes());
        let n_target = p.len();
        p.extend_from_slice(b"\x06farmos");
        p.extend_from_slice(&[0xC0, n_local as u8]);
        count += 1;
        if with_txt {
            p.extend_from_slice(&[0xC0, n_instance as u8]);
            p.extend_from_slice(&TYPE_TXT.to_be_bytes());
            p.extend_from_slice(&0x8001u16.to_be_bytes());
            p.extend_from_slice(&4500u32.to_be_bytes());
            let items: &[&[u8]] = &[b"base=/api/v1/", b"version=1"];
            let len: usize = items.iter().map(|i| i.len() + 1).sum();
            p.extend_from_slice(&(len as u16).to_be_bytes());
            for i in items {
                p.push(i.len() as u8);
                p.extend_from_slice(i);
            }
            count += 1;
        }
        if with_a {
            p.extend_from_slice(&[0xC0, n_target as u8]);
            p.extend_from_slice(&TYPE_A.to_be_bytes());
            p.extend_from_slice(&0x8001u16.to_be_bytes());
            p.extend_from_slice(&120u32.to_be_bytes());
            p.extend_from_slice(&4u16.to_be_bytes());
            p.extend_from_slice(&[192, 168, 50, 1]);
            count += 1;
        }
        p[6..8].copy_from_slice(&count.to_be_bytes());
        p
    }

    #[test]
    fn browse_one_shot() {
        let mut b = Browser::new("_farmos._tcp").unwrap();
        let mut q = [0u8; 128];
        let len = b.query(&mut q).unwrap();
        let m = Message::parse(&q[..len]).unwrap();
        let first = m.questions().next().unwrap().unwrap();
        assert_eq!(first.name.to_dotted::<32>(), "_farmos._tcp.local");
        assert_eq!(first.qtype, TYPE_PTR);
        assert!(b.result().is_none());

        let reply = farmos_reply(true, true);
        assert!(b.ingest(&reply).unwrap());
        assert_eq!(b.instance().unwrap().to_dotted::<40>(), "Farm server._farmos._tcp.local");
        let s = b.result().unwrap();
        assert_eq!(s.ip, [192, 168, 50, 1]);
        assert_eq!(s.port, 3000);
        assert_eq!(s.base.as_str(), "/api/v1");
        // Same packet again: nothing new.
        assert!(!b.ingest(&reply).unwrap());
    }

    #[test]
    fn browse_in_steps() {
        let mut b = Browser::new("_farmos._tcp").unwrap();
        // Only PTR + SRV: we learn the target, then ask for A and TXT.
        assert!(b.ingest(&farmos_reply(false, false)).unwrap());
        assert!(b.result().is_none());
        let mut q = [0u8; 256];
        let len = b.query(&mut q).unwrap();
        let m = Message::parse(&q[..len]).unwrap();
        let qs: std::vec::Vec<(std::string::String, u16)> = m
            .questions()
            .map(|q| {
                let q = q.unwrap();
                (q.name.to_dotted::<40>().to_string(), q.qtype)
            })
            .collect();
        assert_eq!(
            qs,
            vec![
                ("farmos.local".to_string(), TYPE_A),
                ("Farm server._farmos._tcp.local".to_string(), TYPE_TXT)
            ]
        );
        // Then the rest arrives.
        assert!(b.ingest(&farmos_reply(true, true)).unwrap());
        assert_eq!(b.result().unwrap().port, 3000);
        b.reset();
        assert!(b.result().is_none());
        assert!(b.instance().is_none());
    }

    #[test]
    fn ignores_goodbyes_and_other_services() {
        let mut b = Browser::new("_farmos._tcp").unwrap();
        let mut reply = farmos_reply(true, true);
        // Zero the PTR TTL → goodbye packet, must not be taken as an instance.
        let ttl_at = 12 + 20 + 4; // header + "_farmos._tcp.local" wire name + type/class
        reply[ttl_at..ttl_at + 4].copy_from_slice(&0u32.to_be_bytes());
        assert!(!b.ingest(&reply).unwrap());
        assert!(b.instance().is_none());
        // A different service's browse reply is ignored entirely.
        let mut other = Browser::new("_printer._tcp").unwrap();
        assert!(!other.ingest(&farmos_reply(true, true)).unwrap());
        // Garbage does not panic, it errors.
        assert!(b.ingest(&[0u8; 5]).is_err());
        assert!(Message::parse(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn base_normalisation() {
        assert_eq!(normalise_base(b"/api/v1/").as_str(), "/api/v1");
        assert_eq!(normalise_base(b"api/v1").as_str(), "/api/v1");
        assert_eq!(normalise_base(b"/").as_str(), "");
        assert_eq!(normalise_base(b"").as_str(), "");
        assert_eq!(normalise_base(b"with space").as_str(), "");
        assert_eq!(normalise_base(&[b'/'; 40]).as_str(), "");
        assert_eq!(normalise_base(&[b'a'; 40]).as_str(), "", "too long is dropped, not truncated");
    }

    /// Captured from avahi-daemon 0.8 answering a `_farmos._tcp.local` PTR
    /// query (`avahi-publish -s "Farm server" _farmos._tcp 3000 base=/api/v1`).
    // 153 bytes from 10.10.1.16
    const AVAHI_FARMOS_REPLY: &[u8] = &[
        0x00, 0x00, 0x84, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x07, 0x5f, 0x66, 0x61,
        0x72, 0x6d, 0x6f, 0x73, 0x04, 0x5f, 0x74, 0x63, 0x70, 0x05, 0x6c, 0x6f, 0x63, 0x61, 0x6c, 0x00,
        0x00, 0x0c, 0x00, 0x01, 0x00, 0x00, 0x11, 0x94, 0x00, 0x0e, 0x0b, 0x46, 0x61, 0x72, 0x6d, 0x20,
        0x73, 0x65, 0x72, 0x76, 0x65, 0x72, 0xc0, 0x0c, 0xc0, 0x2a, 0x00, 0x10, 0x80, 0x01, 0x00, 0x00,
        0x11, 0x94, 0x00, 0x0d, 0x0c, 0x62, 0x61, 0x73, 0x65, 0x3d, 0x2f, 0x61, 0x70, 0x69, 0x2f, 0x76,
        0x31, 0xc0, 0x2a, 0x00, 0x21, 0x80, 0x01, 0x00, 0x00, 0x00, 0x78, 0x00, 0x10, 0x00, 0x00, 0x00,
        0x00, 0x0b, 0xb8, 0x07, 0x66, 0x6c, 0x75, 0x74, 0x65, 0x61, 0x69, 0xc0, 0x19, 0xc0, 0x63, 0x00,
        0x1c, 0x80, 0x01, 0x00, 0x00, 0x00, 0x78, 0x00, 0x10, 0xfe, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x8e, 0x16, 0x45, 0xff, 0xfe, 0xec, 0x76, 0xd1, 0xc0, 0x63, 0x00, 0x01, 0x80, 0x01, 0x00,
        0x00, 0x00, 0x78, 0x00, 0x04, 0x0a, 0x0a, 0x01, 0x10,
    ];

    #[test]
    fn parses_real_avahi_reply() {
        let mut b = Browser::new("_farmos._tcp").unwrap();
        assert!(b.ingest(AVAHI_FARMOS_REPLY).unwrap());
        let s = b.result().expect("PTR+SRV+TXT+A all in one Avahi packet");
        assert_eq!(s.port, 3000);
        assert_eq!(s.base.as_str(), "/api/v1");
        assert_eq!(s.ip[0], 10, "host LAN address");
        assert_eq!(b.instance().unwrap().to_dotted::<40>(), "Farm server._farmos._tcp.local");
    }
}
