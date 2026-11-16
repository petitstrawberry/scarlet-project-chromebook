// SPDX-License-Identifier: GPL-2.0-only
//! Wire format follows Linux v6.12 drivers/rpmsg/qcom_glink_native.c.
use alloc::{collections::VecDeque, string::String, vec::Vec};
pub const MAX_PACKET: usize = 65536;
const MAX_CHANNELS: usize = 16;
const MAX_INTENTS: usize = 32;
const MAX_QUEUE: usize = 128;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Unsupported,
    Exhausted,
    Closed,
    NotReady,
}
#[cfg(test)]
mod tests {
    use super::*;
    fn connected()->Link {
        let mut link=Link::new();link.output.clear();
        link.receive(&frame(0,1,1,&[])).unwrap();
        link.receive(&frame(1,1,1,&[])).unwrap();
        link.receive(&frame(2,7,7,b"IPCRTR\0")).unwrap();
        link.receive(&frame(4,1,0,&[])).unwrap();
        link
    }
    #[test]fn reassembles_only_matching_fragment_sequence(){
        let mut link=connected();
        let mut body=Vec::new();body.extend_from_slice(&2u32.to_le_bytes());body.extend_from_slice(&2u32.to_le_bytes());body.extend_from_slice(b"ab");
        link.receive(&frame(9,7,1,&body)).unwrap();
        body[4..8].copy_from_slice(&0u32.to_le_bytes());body[8..].copy_from_slice(b"cd");
        link.receive(&frame(12,7,1,&body)).unwrap();
        assert!(link.events.iter().any(|event|matches!(event,Event::Packet(name,b) if name=="IPCRTR" && b==b"abcd")));
        assert_eq!(link.receive(&frame(12,7,1,&body)),Err(Error::Malformed));
    }
    #[test]fn remote_intent_cannot_be_reused_until_done(){
        let mut link=connected();link.output.clear();let mut body=Vec::new();body.extend_from_slice(&1024u32.to_le_bytes());body.extend_from_slice(&42u32.to_le_bytes());
        link.receive(&frame(5,7,1,&body)).unwrap();link.send("IPCRTR",b"one").unwrap();link.send("IPCRTR",b"two").unwrap();
        assert_eq!(link.output.iter().filter(|b|u16le(b)==9).count(),1);
        link.receive(&frame(14,7,42,&[])).unwrap();assert_eq!(link.output.iter().filter(|b|u16le(b)==9).count(),2);
    }
}
pub fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes(b[..2].try_into().unwrap())
}
pub fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
pub fn frame(cmd: u16, p1: u16, p2: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity((8 + body.len() + 7) & !7);
    b.extend_from_slice(&cmd.to_le_bytes());
    b.extend_from_slice(&p1.to_le_bytes());
    b.extend_from_slice(&p2.to_le_bytes());
    b.extend_from_slice(body);
    b.resize((b.len() + 7) & !7, 0);
    b
}
/// None means an incomplete header, not permission to consume a partial frame.
pub fn frame_size(b: &[u8], fifo_size: usize) -> Result<Option<usize>, Error> {
    if b.len() < 8 {
        return Ok(None);
    }
    let body = match u16le(b) {
        0 | 1 | 3 | 4 | 6 | 7 | 8 | 11 | 13 | 14 | 15 => 0,
        2 => {
            let n = u32le(&b[4..]) as usize;
            if n == 0 || n > 64 {
                return Err(Error::Malformed);
            }
            n
        }
        5 => {
            let n = u32le(&b[4..]) as usize;
            if n > MAX_INTENTS {
                return Err(Error::Exhausted);
            }
            n * 8
        }
        9 | 12 => {
            if b.len() < 16 {
                return Ok(None);
            }
            8 + u32le(&b[8..]) as usize
        }
        _ => return Err(Error::Unsupported),
    };
    let size = 8usize
        .checked_add(body)
        .and_then(|n| n.checked_add(7))
        .ok_or(Error::Malformed)?
        & !7;
    if size > fifo_size.saturating_sub(8) {
        return Err(Error::Malformed);
    }
    Ok(Some(size))
}
pub enum Event {
    Opened(String),
    Packet(String, Vec<u8>),
    Closed(String),
}
struct LocalIntent {
    id: u32,
    size: usize,
    data: Vec<u8>,
    remaining: Option<usize>,
}
struct RemoteIntent {
    id: u32,
    size: usize,
    busy: bool,
}
struct Channel {
    name: String,
    local: u16,
    remote: u16,
    ack: bool,
    announced: bool,
    locals: Vec<LocalIntent>,
    remotes: Vec<RemoteIntent>,
    next_intent: u32,
    pending: VecDeque<Vec<u8>>,
    intent_requested: bool,
}
pub struct Link {
    features: u32,
    negotiated: bool,
    next_channel: u16,
    channels: Vec<Channel>,
    pub output: VecDeque<Vec<u8>>,
    pub events: VecDeque<Event>,
}
impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}
impl Link {
    pub fn new() -> Self {
        let mut output = VecDeque::new();
        output.push_back(frame(0, 1, 1, &[]));
        Self {
            features: 1,
            negotiated: false,
            next_channel: 1,
            channels: Vec::new(),
            output,
            events: VecDeque::new(),
        }
    }
    fn queue(&mut self, b: Vec<u8>) -> Result<(), Error> {
        if self.output.len() >= MAX_QUEUE {
            return Err(Error::Exhausted);
        }
        self.output.push_back(b);
        Ok(())
    }
    fn advertise(&mut self, index: usize, size: usize) -> Result<bool, Error> {
        let ch = &mut self.channels[index];
        if size == 0 || size > MAX_PACKET || ch.locals.len() >= MAX_INTENTS {
            return Ok(false);
        }
        let id = ch.next_intent;
        ch.next_intent = id.checked_add(1).ok_or(Error::Exhausted)?;
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&(size as u32).to_le_bytes());
        body.extend_from_slice(&id.to_le_bytes());
        let local = ch.local;
        ch.locals.push(LocalIntent {
            id,
            size,
            data: Vec::new(),
            remaining: None,
        });
        self.queue(frame(5, local, 1, &body))?;
        Ok(true)
    }
    fn activate(&mut self, index: usize) -> Result<(), Error> {
        let ch = &mut self.channels[index];
        if !ch.ack || ch.remote == 0 || ch.announced {
            return Ok(());
        }
        ch.announced = true;
        let name = ch.name.clone();
        self.events.push_back(Event::Opened(name));
        for _ in 0..5 {
            self.advertise(index, 1024)?;
        }
        Ok(())
    }
    pub fn send(&mut self, name: &str, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() || data.len() > MAX_PACKET {
            return Err(Error::Malformed);
        }
        let ch = self
            .channels
            .iter_mut()
            .find(|ch| ch.name == name)
            .ok_or(Error::Closed)?;
        if !ch.announced {
            return Err(Error::NotReady);
        }
        if ch.pending.len() >= 16 {
            return Err(Error::Exhausted);
        }
        ch.pending.push_back(data.to_vec());
        self.flush()
    }
    pub fn flush(&mut self) -> Result<(), Error> {
        for ch in &mut self.channels {
            while let Some(packet) = ch.pending.front() {
                let Some(intent) = ch
                    .remotes
                    .iter_mut()
                    .find(|i| !i.busy && i.size >= packet.len())
                else {
                    if !ch.intent_requested {
                        if self.output.len() >= MAX_QUEUE {
                            break;
                        }
                        self.output
                            .push_back(frame(7, ch.local, packet.len() as u32, &[]));
                        ch.intent_requested = true;
                    }
                    break;
                };
                let chunks = packet.len().div_ceil(4096);
                if self.output.len() + chunks > MAX_QUEUE {
                    break;
                }
                intent.busy = true;
                let packet = ch.pending.pop_front().unwrap();
                for (n, bytes) in packet.chunks(4096).enumerate() {
                    let mut body = Vec::with_capacity(8 + bytes.len());
                    body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                    body.extend_from_slice(
                        &((packet.len() - n * 4096 - bytes.len()) as u32).to_le_bytes(),
                    );
                    body.extend_from_slice(bytes);
                    self.output.push_back(frame(
                        if n == 0 { 9 } else { 12 },
                        ch.local,
                        intent.id,
                        &body,
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn receive(&mut self, b: &[u8]) -> Result<(), Error> {
        if frame_size(b, 1 << 20)? != Some(b.len()) || self.events.len() >= MAX_QUEUE {
            return Err(Error::Malformed);
        }
        let cmd = u16le(b);
        let p1 = u16le(&b[2..]);
        let p2 = u32le(&b[4..]);
        match cmd {
            0 => {
                if p1 == 0 {
                    return Err(Error::Unsupported);
                }
                if p1 == 1 {
                    self.features &= p2;
                }
                self.queue(frame(1, 1, self.features, &[]))?;
            }
            1 => {
                if p1 == 0 {
                    return Err(Error::Unsupported);
                }
                if p1 != 1 || p2 != self.features {
                    self.features &= p2;
                    self.queue(frame(0, 1, self.features, &[]))?;
                } else {
                    self.negotiated = true;
                }
            }
            2 => {
                if !self.negotiated || p1 == 0 {
                    return Err(Error::NotReady);
                }
                let raw = &b[8..8 + p2 as usize];
                if raw.last() != Some(&0) || raw[..raw.len() - 1].contains(&0) {
                    return Err(Error::Malformed);
                }
                let name =
                    core::str::from_utf8(&raw[..raw.len() - 1]).map_err(|_| Error::Malformed)?;
                // Unbound firmware channels remain unacknowledged, as in rpmsg.
                if name != "IPCRTR" {
                    return Ok(());
                }
                if self
                    .channels
                    .iter()
                    .any(|ch| ch.remote == p1 || ch.name == name)
                {
                    return Err(Error::Malformed);
                }
                if self.channels.len() >= MAX_CHANNELS {
                    return Err(Error::Exhausted);
                }
                let local = self.next_channel;
                self.next_channel = local.checked_add(1).ok_or(Error::Exhausted)?;
                self.channels.push(Channel {
                    name: String::from(name),
                    local,
                    remote: p1,
                    ack: false,
                    announced: false,
                    locals: Vec::new(),
                    remotes: Vec::new(),
                    next_intent: 1,
                    pending: VecDeque::new(),
                    intent_requested: false,
                });
                self.queue(frame(4, p1, 0, &[]))?;
                self.queue(frame(2, local, p2, raw))?;
            }
            4 => {
                let index = self
                    .channels
                    .iter()
                    .position(|ch| ch.local == p1)
                    .ok_or(Error::Closed)?;
                self.channels[index].ack = true;
                self.activate(index)?;
            }
            13 => {}
            11 => {}
            _ => {
                let index = self
                    .channels
                    .iter()
                    .position(|ch| ch.remote == p1)
                    .ok_or(Error::Closed)?;
                match cmd {
                    3 => {
                        let ch = self.channels.remove(index);
                        self.output.retain(|f| {
                            !matches!(u16le(f), 2 | 5 | 7 | 8 | 9 | 12 | 15)
                                || u16le(&f[2..]) != ch.local
                        });
                        self.queue(frame(11, p1, 0, &[]))?;
                        self.queue(frame(3, ch.local, 0, &[]))?;
                        self.events.push_back(Event::Closed(ch.name));
                    }
                    5 => {
                        let ch = &mut self.channels[index];
                        if ch.remotes.len() + p2 as usize > MAX_INTENTS {
                            return Err(Error::Exhausted);
                        }
                        for pair in b[8..8 + p2 as usize * 8].chunks_exact(8) {
                            let size = u32le(pair) as usize;
                            let id = u32le(&pair[4..]);
                            if size == 0
                                || size > MAX_PACKET
                                || ch.remotes.iter().any(|i| i.id == id)
                            {
                                return Err(Error::Malformed);
                            }
                            ch.remotes.push(RemoteIntent {
                                id,
                                size,
                                busy: false,
                            });
                        }
                        ch.intent_requested = false;
                    }
                    6 | 14 => {
                        let ch = &mut self.channels[index];
                        let intent = ch
                            .remotes
                            .iter()
                            .position(|i| i.id == p2 && i.busy)
                            .ok_or(Error::Malformed)?;
                        if cmd == 14 {
                            if self.features & 1 == 0 {
                                return Err(Error::Malformed);
                            }
                            ch.remotes[intent].busy = false;
                        } else {
                            ch.remotes.remove(intent);
                        }
                    }
                    7 => {
                        let granted = self.advertise(index, p2 as usize)?;
                        let local = self.channels[index].local;
                        self.queue(frame(8, local, granted as u32, &[]))?;
                    }
                    8 => {
                        if p2 > 1 {
                            return Err(Error::Malformed);
                        }
                    }
                    9 | 12 => {
                        let ch = &mut self.channels[index];
                        let n = u32le(&b[8..]) as usize;
                        let left = u32le(&b[12..]) as usize;
                        let intent = ch
                            .locals
                            .iter_mut()
                            .find(|i| i.id == p2)
                            .ok_or(Error::Malformed)?;
                        if n == 0
                            || n.checked_add(left)
                                .is_none_or(|v| v > intent.size.saturating_sub(intent.data.len()))
                        {
                            return Err(Error::Malformed);
                        }
                        if (cmd == 9 && intent.remaining.is_some())
                            || (cmd == 12 && intent.remaining != Some(n + left))
                        {
                            return Err(Error::Malformed);
                        }
                        intent.data.extend_from_slice(&b[16..16 + n]);
                        intent.remaining = Some(left);
                        if left == 0 {
                            let packet = core::mem::take(&mut intent.data);
                            intent.remaining = None;
                            self.events
                                .push_back(Event::Packet(ch.name.clone(), packet));
                            let local = ch.local;
                            if self.features & 1 != 0 {
                                self.queue(frame(14, local, p2, &[]))?;
                            } else {
                                let at = ch.locals.iter().position(|i| i.id == p2).unwrap();
                                let size = ch.locals.remove(at).size;
                                self.queue(frame(6, local, p2, &[]))?;
                                self.advertise(index, size)?;
                            }
                        }
                    }
                    15 => {}
                    _ => return Err(Error::Unsupported),
                }
            }
        }
        self.flush()
    }
}
