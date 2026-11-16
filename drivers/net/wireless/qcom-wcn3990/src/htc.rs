// SPDX-License-Identifier: GPL-2.0-only
//! Bounded HTC control negotiation and endpoint credit accounting.
use alloc::{collections::VecDeque, vec, vec::Vec};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Unsupported,
    Sequence,
    Full,
    NotReady,
}
fn half(b: &[u8]) -> u16 {
    u16::from_le_bytes(b[..2].try_into().unwrap())
}
pub enum Event {
    Ready { credits: u16, size: u16 },
    Connected { service: u16, endpoint: u8 },
    Started,
    Data { service: u16, bytes: Vec<u8> },
}
pub struct Outbound {
    pub pipe: usize,
    pub endpoint: u8,
    pub bytes: Vec<u8>,
    cost: u16,
    start: bool,
}
impl Drop for Outbound {
    fn drop(&mut self) {
        crate::secret::erase(&mut self.bytes);
    }
}
#[derive(Clone, Copy)]
struct Endpoint {
    service: u16,
    pipe: usize,
    rx: usize,
    max: usize,
    flow: bool,
    sequence: u8,
}
pub struct Htc {
    endpoints: [Option<Endpoint>; 32],
    credits: [u16; 32],
    credit_size: u16,
    max_endpoints: usize,
    pending: Option<u16>,
    ready: bool,
    output: VecDeque<Outbound>,
    pub events: VecDeque<Event>,
}
impl Default for Htc {
    fn default() -> Self {
        Self::new()
    }
}
impl Htc {
    pub fn new() -> Self {
        let mut endpoints = [None; 32];
        endpoints[0] = Some(Endpoint {
            service: 1,
            pipe: 0,
            rx: 2,
            max: 512,
            flow: false,
            sequence: 0,
        });
        Self {
            endpoints,
            credits: [0; 32],
            credit_size: 0,
            max_endpoints: 32,
            pending: None,
            ready: false,
            output: VecDeque::new(),
            events: VecDeque::new(),
        }
    }
    fn enqueue(&mut self, eid: usize, payload: &[u8], start: bool) -> Result<(), Error> {
        if self.output.len() >= 128 {
            return Err(Error::Full);
        }
        let ep = self
            .endpoints
            .get_mut(eid)
            .and_then(|v| v.as_mut())
            .ok_or(Error::NotReady)?;
        if payload.len() > ep.max || payload.len() > u16::MAX as usize {
            return Err(Error::Malformed);
        }
        let mut bytes = Vec::with_capacity(8 + payload.len());
        bytes.extend_from_slice(&[eid as u8, if ep.flow { 1 } else { 0 }]);
        bytes.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&[0, ep.sequence, 0, 0]);
        bytes.extend_from_slice(payload);
        ep.sequence = ep.sequence.wrapping_add(1);
        let cost = if ep.flow {
            bytes.len().div_ceil(self.credit_size as usize) as u16
        } else {
            0
        };
        self.output.push_back(Outbound {
            pipe: ep.pipe,
            endpoint: eid as u8,
            bytes,
            cost,
            start,
        });
        Ok(())
    }
    fn connect(&mut self, service: u16) -> Result<(), Error> {
        if self.pending.is_some() {
            return Err(Error::Sequence);
        }
        let flags = if service == 0x100 { 1u16 << 8 } else { 8 };
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&service.to_le_bytes());
        body.extend_from_slice(&flags.to_le_bytes());
        body.extend_from_slice(&[0, 0]);
        self.enqueue(0, &body, false)?;
        self.pending = Some(service);
        Ok(())
    }
    pub fn send(&mut self, service: u16, payload: &[u8]) -> Result<(), Error> {
        let eid = self
            .endpoints
            .iter()
            .position(|ep| ep.is_some_and(|ep| ep.service == service))
            .ok_or(Error::NotReady)?;
        self.enqueue(eid, payload, false)
    }
    pub fn next(&self) -> Option<(usize, &Outbound)> {
        self.output
            .iter()
            .enumerate()
            .find(|(_, out)| out.cost <= self.credits[out.endpoint as usize] || out.cost == 0)
    }
    /// Creditless HTT data bypasses the control queue. The caller commits the
    /// complete frame directly to CE while holding its admission barrier.
    pub fn data_frame(&mut self, service: u16, payload: &[u8]) -> Result<(usize, Vec<u8>), Error> {
        let (eid, ep) = self
            .endpoints
            .iter_mut()
            .enumerate()
            .find_map(|(i, ep)| ep.as_mut().filter(|e| e.service == service).map(|e| (i, e)))
            .ok_or(Error::NotReady)?;
        if ep.flow || payload.len() > ep.max || payload.len() > u16::MAX as usize {
            return Err(Error::Unsupported);
        }
        let mut bytes = vec![eid as u8, 0];
        bytes.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&[0, ep.sequence, 0, 0]);
        bytes.extend_from_slice(payload);
        ep.sequence = ep.sequence.wrapping_add(1);
        Ok((ep.pipe, bytes))
    }
    pub fn submitted(&mut self, index: usize) -> Result<(), Error> {
        let out = self.output.remove(index).ok_or(Error::Sequence)?;
        let credits = &mut self.credits[out.endpoint as usize];
        *credits = credits.checked_sub(out.cost).ok_or(Error::Sequence)?;
        if out.start {
            self.events.push_back(Event::Started);
        }
        Ok(())
    }
    pub fn receive(&mut self, pipe: usize, b: &[u8]) -> Result<(), Error> {
        if b.len() < 8 || self.events.len() >= 128 {
            return Err(Error::Malformed);
        }
        let eid = b[0] as usize;
        let flags = b[1];
        let len = half(&b[2..]) as usize;
        if eid >= self.max_endpoints
            || len + 8 > b.len()
            || b.len() - len - 8 > 3
            || b[8 + len..].iter().any(|v| *v != 0)
        {
            return Err(Error::Malformed);
        }
        // RX bundling is not requested in SETUP_COMPLETE_EX.
        if flags & 0xfc != 0 {
            return Err(Error::Unsupported);
        }
        let ep = self.endpoints[eid].ok_or(Error::Sequence)?;
        if pipe != ep.rx {
            return Err(Error::Sequence);
        }
        let trailer = if flags & 2 != 0 { b[4] as usize } else { 0 };
        if trailer > len {
            return Err(Error::Malformed);
        }
        let mut records = &b[8 + len - trailer..8 + len];
        while !records.is_empty() {
            if records.len() < 4 {
                return Err(Error::Malformed);
            }
            let id = records[0];
            let size = records[1] as usize;
            let data = records.get(4..4 + size).ok_or(Error::Malformed)?;
            if id == 1 {
                if size % 4 != 0 {
                    return Err(Error::Malformed);
                }
                for report in data.chunks_exact(4) {
                    let eid = report[0] as usize;
                    if eid >= self.max_endpoints {
                        return Err(Error::Malformed);
                    }
                    if self.endpoints[eid].is_some_and(|ep| ep.flow) {
                        self.credits[eid] = self.credits[eid]
                            .checked_add(u16::from(report[1]))
                            .filter(|v| *v <= 256)
                            .ok_or(Error::Malformed)?;
                    }
                }
            }
            records = &records[4 + size..];
        }
        let payload = &b[8..8 + len - trailer];
        if payload.is_empty() {
            return Ok(());
        }
        if eid != 0 {
            self.events.push_back(Event::Data {
                service: ep.service,
                bytes: payload.to_vec(),
            });
            return Ok(());
        }
        if payload.len() < 2 {
            return Err(Error::Malformed);
        }
        match half(payload) {
            1 => {
                if self.ready || payload.len() < 8 {
                    return Err(Error::Sequence);
                }
                self.credit_size = half(&payload[4..]);
                self.max_endpoints = payload[6] as usize;
                if self.credit_size == 0 || !(3..=32).contains(&self.max_endpoints) {
                    return Err(Error::Malformed);
                }
                self.ready = true;
                self.events.push_back(Event::Ready {
                    credits: half(&payload[2..]),
                    size: self.credit_size,
                });
                self.connect(0x100)?;
            }
            3 => {
                if payload.len() < 8 {
                    return Err(Error::Malformed);
                }
                let service = half(&payload[2..]);
                let endpoint = payload[5] as usize;
                let maximum = half(&payload[6..]) as usize;
                if self.pending != Some(service)
                    || payload[4] != 0
                    || endpoint == 0
                    || endpoint >= self.max_endpoints
                    || self.endpoints[endpoint].is_some()
                    || maximum == 0
                {
                    return Err(Error::Sequence);
                }
                let (pipe, rx) = match service {
                    0x100 => (3, 2),
                    0x300 => (4, 1),
                    _ => return Err(Error::Unsupported),
                };
                self.endpoints[endpoint] = Some(Endpoint {
                    service,
                    pipe,
                    rx,
                    max: maximum,
                    flow: service == 0x100,
                    sequence: 0,
                });
                // WCN3990 allocates one WMI credit, as Linux's use_fw_tx_credits
                // false path does; HTT has its own descriptor flow control.
                self.credits[endpoint] = if service == 0x100 { 1 } else { 0 };
                self.pending = None;
                self.events.push_back(Event::Connected {
                    service,
                    endpoint: endpoint as u8,
                });
                if service == 0x100 {
                    self.connect(0x300)?;
                } else {
                    let mut body = vec![0; 12];
                    body[..2].copy_from_slice(&5u16.to_le_bytes());
                    self.enqueue(0, &body, true)?;
                }
            }
            6 => {}
            _ => return Err(Error::Unsupported),
        }
        Ok(())
    }
}
