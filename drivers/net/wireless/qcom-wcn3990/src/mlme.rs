// SPDX-License-Identifier: GPL-2.0-only
//! Single-owner host STA MLME. Firmware events drive transitions; deadlines
//! bound individual operations, with no idle polling or blocking API callbacks.
use crate::{ce, datapath::Datapath, htc::Htc, radio::Radio, sta, wmi};
use alloc::{sync::Arc, vec::Vec};
use scarlet::device::wireless::{
    abi::*,
    auth::Association,
    core::{BACKEND_ASSOCIATED, BACKEND_COMPLETE, BACKEND_RESET, BackendRequest},
};
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Mapping,
    Authenticating,
    Associating,
    Configuring,
    Activating,
    Up,
    Authorizing,
    Draining,
    Unmapping,
    Stopping,
    Destroying,
}
struct Connection {
    token: Token,
    ap: sta::Bss,
    own_ie: Vec<u8>,
    phase: Phase,
    deadline: u64,
    attempts: u8,
    peer_id: Option<u16>,
    aid: u16,
    disconnect: Option<Token>,
    failure: i32,
    vdev_up: bool,
    fw_authorized: bool,
    deauth_sent: bool,
}
struct KeyWork {
    request: BackendRequest,
    reference: KeyRef,
    group: bool,
    delete: bool,
    cipher: u8,
    deadline: u64,
    cancelled: bool,
}
struct Scan {
    token: Option<Token>,
    connect: Option<BackendRequest>,
    id: u16,
    started: u64,
    deadline: u64,
}
pub struct Mlme {
    pub radio: Arc<Radio>,
    mac: [u8; 6],
    cache: Vec<sta::Bss>,
    scan: Option<Scan>,
    connection: Option<Connection>,
    serial: u16,
    sequence: u16,
    pub deadline: Option<u64>,
    key_work: Option<KeyWork>,
    keys: Vec<KeyRef>,
    vdev_exists: bool,
    rx_diagnostics: u32,
    rx_handshake_diagnostics: u8,
}
fn format<T>(v: Result<T, wmi::Error>) -> Result<T, &'static str> {
    v.map_err(|_| "wcn3990: invalid MLME wire format")
}
fn queue(htc: &mut Htc, mut cmd: Vec<u8>) -> Result<(), &'static str> {
    let result = htc
        .send(0x100, &cmd)
        .map_err(|_| "wcn3990: MLME command queue full");
    crate::secret::erase(&mut cmd);
    result
}
fn record(ap: &sta::Bss) -> Bss {
    let mut b = Bss {
        bssid: ap.bssid,
        ssid_len: ap.ssid.len() as u8,
        frequency_mhz: crate::channel::frequency(ap.channel).unwrap_or(0),
        signal_mbm: ap.signal_dbm * 100,
        akm: if ap.rsn.is_empty() {
            AKM_OPEN
        } else {
            AKM_EXTERNAL
        },
        seen_ns: ap.timestamp.saturating_mul(1000),
        ..Bss::default()
    };
    b.ssid[..ap.ssid.len()].copy_from_slice(&ap.ssid);
    b
}
impl Mlme {
    pub fn new(radio: Arc<Radio>, mac: [u8; 6]) -> Self {
        Self {
            radio,
            mac,
            cache: Vec::new(),
            scan: None,
            connection: None,
            serial: 0,
            sequence: 0,
            deadline: None,
            key_work: None,
            keys: Vec::new(),
            vdev_exists: true,
            rx_diagnostics: 0,
            rx_handshake_diagnostics: 0,
        }
    }
    fn next_sequence(&mut self) -> u16 {
        self.sequence = self.sequence.wrapping_add(1) & 0xfff;
        self.sequence
    }
    pub fn bootstrap(&mut self, htc: &mut Htc, now: u64) -> Result<(), &'static str> {
        self.start_scan(None, None, htc, now)
    }
    fn start_scan(
        &mut self,
        token: Option<Token>,
        connect: Option<BackendRequest>,
        htc: &mut Htc,
        now: u64,
    ) -> Result<(), &'static str> {
        if !self.vdev_exists {
            queue(htc, format(wmi::vdev_create(self.mac))?)?;
            self.vdev_exists = true;
        }
        self.serial = self.serial % 0xfff + 1;
        queue(htc, format(wmi::passive_scan(self.serial))?)?;
        self.scan = Some(Scan {
            token,
            connect,
            id: self.serial,
            started: now,
            deadline: now + 25_000_000,
        });
        Ok(())
    }
    fn management(
        &mut self,
        frame: Vec<u8>,
        htc: &mut Htc,
        dp: &mut Datapath,
    ) -> Result<(), &'static str> {
        let (_, cmd) = dp.management(&frame)?;
        queue(htc, cmd)
    }
    fn begin_connect(
        &mut self,
        req: BackendRequest,
        htc: &mut Htc,
        now: u64,
    ) -> Result<(), &'static str> {
        let selected = self
            .cache
            .iter()
            .filter(|ap| {
                now.saturating_sub(ap.timestamp) < 30_000_000
                    && ap.ssid == req.connect.ssid[..req.connect.ssid_len as usize]
                    && (ap.rsn.is_empty() == req.rsn.is_none())
            })
            .max_by_key(|ap| ap.signal_dbm)
            .cloned();
        let Some(ap) = selected else {
            self.radio
                .complete(req.token, BACKEND_COMPLETE, NOT_FOUND, Vec::new());
            return Ok(());
        };
        let own_ie = match req.rsn {
            Some(r) => r.ie[..r.ie_len as usize].to_vec(),
            None => Vec::new(),
        };
        // Validating the association frame also checks the AP's required rates.
        if sta::associate(self.mac, &ap, 0, &own_ie).is_err() {
            self.radio
                .complete(req.token, BACKEND_COMPLETE, NOT_SUPPORTED, Vec::new());
            return Ok(());
        }
        queue(htc, format(wmi::vdev_start(ap.channel, ap.interval))?)?;
        self.rx_handshake_diagnostics = 0;
        self.connection = Some(Connection {
            token: req.token,
            ap,
            own_ie,
            phase: Phase::Starting,
            deadline: now + 5_000_000,
            attempts: 0,
            peer_id: None,
            aid: 0,
            disconnect: None,
            failure: OK,
            vdev_up: false,
            fw_authorized: false,
            deauth_sent: false,
        });
        let ap = &self.connection.as_ref().unwrap().ap;
        scarlet::println!("[wcn3990] STA starting channel={} RSNXE={}", ap.channel, ap.rsnxe.len());
        Ok(())
    }
    fn authenticate(
        &mut self,
        htc: &mut Htc,
        dp: &mut Datapath,
        now: u64,
    ) -> Result<(), &'static str> {
        let seq = self.next_sequence();
        let c = self
            .connection
            .as_mut()
            .ok_or("wcn3990: no authentication scope")?;
        c.phase = Phase::Authenticating;
        c.deadline = now + 1_000_000;
        c.attempts += 1;
        let frame = sta::authenticate(self.mac, c.ap.bssid, seq);
        self.management(frame, htc, dp)
    }
    fn stop(
        &mut self,
        token: Option<Token>,
        failure: i32,
        htc: &mut Htc,
        now: u64,
    ) -> Result<(), &'static str> {
        let Some(c) = self.connection.as_mut() else {
            self.radio.down();
            if let Some(token) = token {
                self.radio.complete(token, BACKEND_COMPLETE, OK, Vec::new());
            }
            return Ok(());
        };
        if matches!(
            c.phase,
            Phase::Draining | Phase::Unmapping | Phase::Stopping | Phase::Destroying
        ) {
            if token.is_some() {
                c.disconnect = token;
            }
            return Ok(());
        }
        self.radio.down();
        c.disconnect = token;
        c.failure = failure;
        c.deadline = now + 3_000_000;
        c.phase = Phase::Draining;
        if let Some(key) = &mut self.key_work {
            key.cancelled = true;
        }
        if c.peer_id.is_some() {
            queue(htc, format(wmi::flush_peer(c.ap.bssid))?)?;
        }
        Ok(())
    }
    fn stop_after_drain(&mut self, htc: &mut Htc, now: u64) -> Result<(), &'static str> {
        let c = self
            .connection
            .as_mut()
            .ok_or("wcn3990: missing drain scope")?;
        if c.vdev_up {
            queue(htc, format(wmi::command(0x5007, 94, &wmi::words(&[0])))?)?;
        }
        c.deadline = now + 5_000_000;
        if c.peer_id.is_some() {
            queue(htc, format(wmi::peer_delete(c.ap.bssid))?)?;
            c.phase = Phase::Unmapping;
        } else {
            queue(htc, format(wmi::command(0x5006, 93, &wmi::words(&[0])))?)?;
            c.phase = Phase::Stopping;
        }
        Ok(())
    }
    pub fn wmi_event(
        &mut self,
        id: u32,
        bytes: &[u8],
        htc: &mut Htc,
        dp: &mut Datapath,
        now: u64,
    ) -> Result<(), &'static str> {
        match id {
            0x7001 => {
                let Ok(rx) = wmi::management(bytes) else {
                    return Ok(());
                };
                if matches!(rx.frame[0], 0x80 | 0x50) {
                    if let Ok(ap) = sta::beacon(rx.frame, rx.channel as u8, rx.signal_dbm, now) {
                        if let Some(old) = self
                            .cache
                            .iter_mut()
                            .find(|old| old.bssid == ap.bssid && old.channel == ap.channel)
                        {
                            *old = ap;
                        } else if self.cache.len() < 256 {
                            self.cache.push(ap);
                        }
                    }
                }
                let Some(c) = self.connection.as_ref() else {
                    return Ok(());
                };
                if rx.frame[10..16] != c.ap.bssid || rx.frame[4..10] != self.mac {
                    return Ok(());
                }
                if rx.frame[0] == 0xb0 && c.phase == Phase::Authenticating {
                    if let Err(error) = sta::auth_response(rx.frame, self.mac, c.ap.bssid) {
                        if matches!(error, sta::Error::Rejected(_)) {
                            self.stop(None, FAILED, htc, now)?;
                        }
                        return Ok(());
                    }
                    let seq = self.next_sequence();
                    let c = self.connection.as_mut().unwrap();
                    let frame = sta::associate(self.mac, &c.ap, seq, &c.own_ie)
                        .map_err(|_| "wcn3990: association format")?;
                    c.phase = Phase::Associating;
                    c.deadline = now + 1_000_000;
                    c.attempts = 1;
                    self.management(frame, htc, dp)?;
                    scarlet::println!("[wcn3990] STA authenticated");
                } else if rx.frame[0] == 0x10 && c.phase == Phase::Associating {
                    match sta::assoc_response(rx.frame, self.mac, c.ap.bssid) {
                        Ok(aid) => {
                            let c = self.connection.as_mut().unwrap();
                            c.aid = aid;
                            c.phase = Phase::Configuring;
                            c.deadline = now + 5_000_000;
                            queue(htc, format(wmi::peer_associate(&c.ap, aid))?)?;
                        }
                        Err(sta::Error::Rejected(_)) => self.stop(None, FAILED, htc, now)?,
                        _ => {}
                    }
                } else if matches!(rx.frame[0], 0xc0 | 0xa0) && c.phase == Phase::Up {
                    self.stop(None, FAILED, htc, now)?;
                    self.radio
                        .complete(Token::default(), BACKEND_RESET, FAILED, Vec::new());
                }
            }
            0x3001 => {
                let ev = format(wmi::scan_event(bytes))?;
                let Some(scan) = &self.scan else {
                    return Ok(());
                };
                if ev[4] != (0xa000 | u32::from(scan.id)) || ev[5] != 0 || ev[3] != 0xa001 {
                    return Ok(());
                }
                if ev[0] & (2 | 64) != 0 {
                    let scan = self.scan.take().unwrap();
                    let success = ev[0] & 2 != 0 && ev[1] == 0;
                    let bss = self
                        .cache
                        .iter()
                        .filter(|ap| ap.timestamp >= scan.started)
                        .map(record)
                        .collect::<Vec<_>>();
                    scarlet::println!(
                        "[wcn3990] scan finished success={} BSS={}",
                        success,
                        bss.len()
                    );
                    if let Some(token) = scan.token {
                        self.radio.complete(
                            token,
                            BACKEND_COMPLETE,
                            if success { OK } else { FAILED },
                            bss,
                        );
                    }
                    if let Some(req) = scan.connect {
                        if success {
                            self.begin_connect(req, htc, now)?;
                        } else {
                            self.radio
                                .complete(req.token, BACKEND_COMPLETE, FAILED, Vec::new());
                        }
                    }
                }
            }
            0x5001 => {
                let ev = format(wmi::fixed_event(bytes, 40, 16))?;
                if ev[0] != 0 {
                    return Ok(());
                }
                if let Some(c) = self
                    .connection
                    .as_mut()
                    .filter(|c| c.phase == Phase::Starting)
                {
                    if ev[3] != 0 {
                        self.stop(None, FAILED, htc, now)?;
                    } else {
                        c.phase = Phase::Mapping;
                        c.deadline = now + 5_000_000;
                        queue(htc, format(wmi::management_rate(&c.ap))?)?;
                        queue(htc, format(wmi::peer_create(c.ap.bssid))?)?;
                    }
                }
            }
            0x6006 => {
                let Some(c) = self
                    .connection
                    .as_mut()
                    .filter(|c| c.phase == Phase::Configuring)
                else {
                    return Ok(());
                };
                // Firmware's peer-association response is the commit point.
                let ev = format(wmi::fixed_event(bytes, 434, 12))?;
                // Layout is vdev_id + wmi_mac_addr; the event carries no status.
                let mut peer = [0; 6];
                let raw = wmi::words(&ev);
                peer.copy_from_slice(&raw[4..10]);
                if ev[0] != 0 || peer != c.ap.bssid {
                    return Ok(());
                }
                if c.own_ie.is_empty() {
                    queue(htc, format(wmi::peer_authorize(c.ap.bssid))?)?;
                    c.fw_authorized = true;
                }
                queue(htc, format(wmi::vdev_up(c.ap.bssid, c.aid))?)?;
                c.vdev_up = true;
                queue(
                    htc,
                    format(wmi::command(
                        0x1d001,
                        149,
                        &wmi::words(&[0x53470000 | c.token.connection as u32 & 0xffff]),
                    ))?,
                )?;
                c.phase = Phase::Activating;
                c.deadline = now + 5_000_000;
            }
            0x1d001 => {
                let ev = format(wmi::fixed_event(bytes, 54, 4))?;
                if let Some(c) = self
                    .connection
                    .as_mut()
                    .filter(|c| c.phase == Phase::Authorizing)
                {
                    if ev[0] == (0x53480000 | c.token.connection as u32 & 0xffff) {
                        c.fw_authorized = true;
                        c.phase = Phase::Up;
                        if let Some(work) = self.key_work.take() {
                            if !work.cancelled {
                                self.radio.complete(
                                    work.request.token,
                                    BACKEND_COMPLETE,
                                    OK,
                                    Vec::new(),
                                );
                            }
                        }
                    }
                    return Ok(());
                }
                let Some(c) = self
                    .connection
                    .as_mut()
                    .filter(|c| c.phase == Phase::Activating)
                else {
                    return Ok(());
                };
                if ev[0] != (0x53470000 | c.token.connection as u32 & 0xffff) {
                    return Ok(());
                }
                c.phase = Phase::Up;
                c.deadline = now + 10_000_000;
                let association = Association {
                    device: c.token.epoch,
                    connection: c.token.connection,
                    peer: c.ap.bssid,
                };
                let mut s = self.radio.state.lock();
                s.up = true;
                s.peer = c.ap.bssid;
                s.epoch = s.epoch.saturating_add(1);
                s.association = Some(association);
                let mut rsn = AssociationRsn {
                    peer: c.ap.bssid,
                    ..AssociationRsn::default()
                };
                rsn.own_len = c.own_ie.len() as u32;
                rsn.ap_len = (c.ap.rsn.len() + c.ap.rsnxe.len()) as u32;
                rsn.own_ie[..c.own_ie.len()].copy_from_slice(&c.own_ie);
                rsn.ap_ie[..c.ap.rsn.len()].copy_from_slice(&c.ap.rsn);
                rsn.ap_ie[c.ap.rsn.len()..rsn.ap_len as usize].copy_from_slice(&c.ap.rsnxe);
                s.rsn = rsn;
                drop(s);
                self.radio
                    .complete(c.token, BACKEND_ASSOCIATED, OK, alloc::vec![record(&c.ap)]);
                if c.own_ie.is_empty() {
                    self.radio
                        .complete(c.token, BACKEND_COMPLETE, OK, Vec::new());
                }
                scarlet::println!("[wcn3990] STA associated aid={}", c.aid);
            }
            0x5002 => {
                let ev = format(wmi::fixed_event(bytes, 41, 4))?;
                if ev[0] == 0 {
                    if let Some(c) = self
                        .connection
                        .as_mut()
                        .filter(|c| c.phase == Phase::Stopping)
                    {
                        queue(htc, format(wmi::command(0x5002, 87, &wmi::words(&[0])))?)?;
                        c.phase = Phase::Destroying;
                        c.deadline = now + 5_000_000;
                    }
                }
            }
            0x5006 => {
                let ev = format(wmi::fixed_event(bytes, 450, 4))?;
                if ev[0] != 0
                    || !self
                        .connection
                        .as_ref()
                        .is_some_and(|c| c.phase == Phase::Destroying)
                {
                    return Ok(());
                }
                let c = self.connection.take().unwrap();
                self.vdev_exists = false;
                self.keys.clear();
                self.key_work = None;
                {
                    let mut s = self.radio.state.lock();
                    s.security_dirty = false;
                    s.retire_requested = false;
                }
                if let Some(token) = c.disconnect {
                    self.radio.complete(token, BACKEND_COMPLETE, OK, Vec::new());
                } else if c.failure != OK {
                    self.radio
                        .complete(c.token, BACKEND_COMPLETE, c.failure, Vec::new());
                }
                self.radio.notify();
                scarlet::println!("[wcn3990] STA stopped; firmware keys retired");
            }
            0x7006 => {
                let ev = format(wmi::fixed_event(bytes, 423, 8))?;
                dp.management_done(ev[0])?;
                if ev[1] != 0 {
                    scarlet::println!("[wcn3990] management TX failed status={}", ev[1]);
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn htt_event(
        &mut self,
        bytes: &[u8],
        htc: &mut Htc,
        dp: &mut Datapath,
        now: u64,
    ) -> Result<(), &'static str> {
        match bytes.first().copied() {
            Some(0xb) if bytes.len() >= 28 => {
                let Some(c) = self.connection.as_ref() else {
                    return Ok(());
                };
                let Some(work) = self.key_work.as_ref() else {
                    return Ok(());
                };
                let peer = u16::from_le_bytes([bytes[2], bytes[3]]);
                if c.peer_id != Some(peer)
                    || bytes[1] & 0x7f != work.cipher
                    || (bytes[1] & 0x80 == 0) != work.group
                {
                    return Ok(());
                }
                let work = self.key_work.take().unwrap();
                if !work.cancelled && c.phase == Phase::Up {
                    // Like ath10k_set_key(), authorize the firmware peer when
                    // its pairwise key is committed. The host controlled port
                    // continues to gate ordinary traffic until WPA completes.
                    if !work.group && !work.delete {
                        queue(htc, format(wmi::peer_authorize(c.ap.bssid))?)?;
                        queue(
                            htc,
                            format(wmi::command(
                                0x1d001,
                                149,
                                &wmi::words(&[0x53480000 | c.token.connection as u32 & 0xffff]),
                            ))?,
                        )?;
                        let c = self.connection.as_mut().unwrap();
                        c.phase = Phase::Authorizing;
                        c.deadline = now + 5_000_000;
                    }
                    if work.delete {
                        self.keys.retain(|key| *key != work.reference);
                    } else {
                        self.keys.retain(|key| {
                            !(key.kind == work.reference.kind && key.index == work.reference.index)
                        });
                        self.keys.push(work.reference);
                    }
                    scarlet::println!(
                        "[wcn3990] firmware key commit kind={} index={} delete={}",
                        work.reference.kind,
                        work.reference.index,
                        work.delete
                    );
                    if !work.group && !work.delete {
                        self.key_work = Some(work);
                    } else {
                        self.radio
                            .complete(work.request.token, BACKEND_COMPLETE, OK, Vec::new());
                    }
                }
            }

            Some(3) if bytes.len() >= 10 => {
                let Some(c) = self
                    .connection
                    .as_mut()
                    .filter(|c| c.phase == Phase::Mapping)
                else {
                    return Ok(());
                };
                if bytes[1] != 0 || bytes[4..10] != c.ap.bssid {
                    return Ok(());
                }
                c.peer_id = Some(u16::from_le_bytes([bytes[2], bytes[3]]));
                self.authenticate(htc, dp, now)?;
            }
            Some(4) if bytes.len() >= 4 => {
                let id = u16::from_le_bytes([bytes[2], bytes[3]]);
                let Some(c) = self.connection.as_mut().filter(|c| c.peer_id == Some(id)) else {
                    return Ok(());
                };
                c.peer_id = None;
                if c.phase == Phase::Unmapping {
                    queue(htc, format(wmi::command(0x5006, 93, &wmi::words(&[0])))?)?;
                    c.phase = Phase::Stopping;
                    c.deadline = now + 5_000_000;
                } else {
                    self.stop(None, FAILED, htc, now)?;
                    self.radio
                        .complete(Token::default(), BACKEND_RESET, FAILED, Vec::new());
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn receive(&mut self, frames: Vec<crate::datapath::Received>) {
        let (up, peer) = {
            let s = self.radio.state.lock();
            (s.up, s.peer)
        };
        let rsn = self
            .connection
            .as_ref()
            .is_some_and(|c| !c.own_ie.is_empty());
        for frame in frames {
            if up && self.keys.is_empty() && self.rx_handshake_diagnostics < 8 {
                self.rx_handshake_diagnostics += 1;
                scarlet::println!(
                    "[wcn3990] handshake RX len={} info={:#x} attention={:#x} fc={:02x?}",
                    frame.bytes.len(),
                    frame.info,
                    frame.attention,
                    frame.bytes.get(..2)
                );
            }
            let offload = frame.info & 0x20 != 0;
            let (mode, cipher) = if offload {
                (
                    1,
                    if frame.bytes.get(1).is_some_and(|b| b & 0x40 != 0) {
                        6
                    } else {
                        7
                    },
                )
            } else if frame.descriptor.len() >= 360 {
                (
                    u32::from_le_bytes(frame.descriptor[36..40].try_into().unwrap()) >> 8 & 3,
                    u32::from_le_bytes(frame.descriptor[16..20].try_into().unwrap()) >> 28 & 15,
                )
            } else {
                continue;
            };
            let diag = (mode
                + u32::from(offload) * 4
                + if cipher == 6 { 8 } else { 0 }
                + if frame.attention & ((1 << 29) | (1 << 30)) != 0 {
                    16
                } else {
                    0
                })
            .min(31);
            if self.rx_diagnostics & (1 << diag) == 0 {
                self.rx_diagnostics |= 1 << diag;
                scarlet::println!(
                    "[wcn3990] RX format={} offload={} cipher={} attention={:#x} up={} bytes={}",
                    mode,
                    offload,
                    cipher,
                    frame.attention,
                    up,
                    frame.bytes.len()
                );
            }
            if !up
                || frame.info & 0x40 != 0
                || frame.attention & ((1 << 30) | (1 << 29) | (1 << 28) | (1 << 27) | (1 << 17))
                    != 0
            {
                continue;
            }
            if cipher == 6 && frame.attention & (1 << 3) != 0 {
                continue;
            }
            if let Ok(bytes) = sta::native_ethernet(&frame.bytes, mode as u8, self.mac, peer) {
                if rsn && bytes[12..14] != [0x88, 0x8e] && cipher != 6 {
                    continue;
                }
                self.radio.deliver(bytes);
            } else if self.rx_diagnostics & (1 << 30) == 0 {
                self.rx_diagnostics |= 1 << 30;
                scarlet::println!(
                    "[wcn3990] RX decapsulation rejected format={} cipher={} fc={:02x?} bytes={}",
                    mode,
                    cipher,
                    frame.bytes.get(..2),
                    frame.bytes.len()
                );
            }
        }
    }
    pub fn pump(
        &mut self,
        htc: &mut Htc,
        dp: &mut Datapath,
        bus: &ce::Bus,
        now: u64,
    ) -> Result<(), &'static str> {
        let cancelled: Vec<_> = self.radio.state.lock().cancelled.drain(..).collect();
        for token in cancelled {
            if let Some(work) = self.key_work.as_mut().filter(|w| w.request.token == token) {
                work.cancelled = true;
            }
            if self
                .connection
                .as_ref()
                .is_some_and(|c| c.token == token && c.phase != Phase::Up)
            {
                self.stop(None, CANCELED, htc, now)?;
            }
            if self.scan.as_ref().is_some_and(|s| {
                s.token == Some(token) || s.connect.as_ref().is_some_and(|r| r.token == token)
            }) {
                let scan = self.scan.as_mut().unwrap();
                scan.token = None;
                scan.connect = None;
                queue(
                    htc,
                    format(wmi::command(
                        0x3002,
                        78,
                        &wmi::words(&[0xa001, 0xa000 | u32::from(scan.id), 0, 0]),
                    ))?,
                )?;
            }
        }
        if self.key_work.as_ref().is_some_and(|w| w.deadline <= now) {
            return Err("wcn3990: firmware key completion timed out");
        }
        if self.radio.state.lock().retire_requested {
            if self.connection.is_some() {
                self.stop(None, FAILED, htc, now)?;
            } else if self.key_work.is_none()
                && !self.scan.as_ref().is_some_and(|s| s.connect.is_some())
            {
                let mut s = self.radio.state.lock();
                s.security_dirty = false;
                s.retire_requested = false;
                drop(s);
                self.radio.notify();
            }
        }
        for _ in 0..8 {
            if (self.key_work.is_some() || dp.outstanding() != 0)
                && self.radio.state.lock().requests.front().is_some_and(|r| {
                    matches!(r.command, INSTALL_KEY | DELETE_KEY | REPLACE_GROUP_KEY)
                })
            {
                break;
            }
            let Some(req) = self.radio.state.lock().requests.pop_front() else {
                break;
            };
            let token = req.token;
            match req.command {
                SCAN => {
                    if self.scan.is_some() || self.connection.is_some() {
                        self.radio
                            .complete(token, BACKEND_COMPLETE, BUSY, Vec::new());
                    } else {
                        self.start_scan(Some(token), None, htc, now)?;
                    }
                }
                CONNECT | CONNECT_RSN => {
                    if self.scan.is_some() || self.connection.is_some() {
                        self.radio
                            .complete(token, BACKEND_COMPLETE, BUSY, Vec::new());
                    } else {
                        self.start_scan(None, Some(req), htc, now)?;
                    }
                }
                DISCONNECT => self.stop(Some(token), OK, htc, now)?,
                INSTALL_KEY | DELETE_KEY | REPLACE_GROUP_KEY => {
                    let Some(c) = self.connection.as_ref().filter(|c| {
                        c.phase == Phase::Up
                            && c.token.epoch == token.epoch
                            && c.token.connection == token.connection
                    }) else {
                        self.radio
                            .complete(token, BACKEND_COMPLETE, STALE_CONNECTION, Vec::new());
                        continue;
                    };
                    if req.command == REPLACE_GROUP_KEY {
                        self.radio
                            .complete(token, BACKEND_COMPLETE, NOT_SUPPORTED, Vec::new());
                        continue;
                    }
                    let delete = req.command == DELETE_KEY;
                    let reference = if delete {
                        req.key_ref
                    } else {
                        let Some(key) = req.key.as_ref() else {
                            self.radio
                                .complete(token, BACKEND_COMPLETE, INVALID, Vec::new());
                            continue;
                        };
                        KeyRef {
                            kind: key.0.kind,
                            index: key.0.index,
                            version: key.0.version,
                        }
                    };
                    if delete && !self.keys.contains(&reference) {
                        self.radio
                            .complete(token, BACKEND_COMPLETE, NOT_FOUND, Vec::new());
                        continue;
                    }
                    let group = reference.kind == KEY_GROUP;
                    let mut material = [0u8; 16];
                    let mut rsc = [0u8; 8];
                    let cipher = if delete && !group { 0 } else { 4 };
                    if delete && group {
                        if scarlet::random::RandomManager::get_random_bytes(&mut material) != 16 {
                            return Err("wcn3990: GTK invalidation entropy unavailable");
                        }
                    } else if !delete {
                        let key = &req.key.as_ref().unwrap().0;
                        material.copy_from_slice(&key.key[..16]);
                        rsc = key.rx_sequence;
                    }
                    let cmd = wmi::install_key(
                        c.ap.bssid,
                        reference.index,
                        group,
                        cipher,
                        &material,
                        &rsc,
                    );
                    crate::secret::erase(&mut material);
                    queue(htc, format(cmd)?)?;
                    self.key_work = Some(KeyWork {
                        request: req,
                        reference,
                        group,
                        delete,
                        cipher: if cipher == 0 { 0 } else { 6 },
                        deadline: now + 3_000_000,
                        cancelled: false,
                    });
                }
                _ => self
                    .radio
                    .complete(token, BACKEND_COMPLETE, NOT_SUPPORTED, Vec::new()),
            }
        }
        if self
            .connection
            .as_ref()
            .is_some_and(|c| c.phase == Phase::Draining)
            && dp.outstanding() == 0
            && !dp.management_outstanding()
        {
            let c = self.connection.as_mut().unwrap();
            if c.vdev_up && !c.deauth_sent {
                // Inform the AP before retiring its firmware peer and keys.
                // Management DMA has its own completion barrier, independent
                // of HTT data completions.
                c.deauth_sent = true;
                let peer = c.ap.bssid;
                let sequence = self.next_sequence();
                let frame = sta::deauthenticate(self.mac, peer, sequence);
                self.management(frame, htc, dp)?;
            } else {
                self.stop_after_drain(htc, now)?;
            }
        }
        if self.scan.as_ref().is_some_and(|s| s.deadline <= now) {
            let scan = self.scan.take().unwrap();
            if let Some(t) = scan.token {
                self.radio
                    .complete(t, BACKEND_COMPLETE, TIMED_OUT, Vec::new());
            }
            if let Some(r) = scan.connect {
                self.radio
                    .complete(r.token, BACKEND_COMPLETE, TIMED_OUT, Vec::new());
            }
            return Err("wcn3990: scan cancellation barrier missing after deadline");
        }
        if self
            .connection
            .as_ref()
            .is_some_and(|c| c.phase != Phase::Up && c.deadline <= now)
        {
            let c = self.connection.as_mut().unwrap();
            if c.phase == Phase::Authenticating && c.attempts < 3 {
                self.authenticate(htc, dp, now)?;
            } else if c.phase == Phase::Associating && c.attempts < 3 {
                c.attempts += 1;
                c.deadline = now + 1_000_000;
                let ap = c.ap.clone();
                let ie = c.own_ie.clone();
                let seq = self.next_sequence();
                self.management(
                    sta::associate(self.mac, &ap, seq, &ie)
                        .map_err(|_| "wcn3990: retry association format")?,
                    htc,
                    dp,
                )?;
            } else if matches!(
                c.phase,
                Phase::Draining | Phase::Unmapping | Phase::Stopping | Phase::Destroying
            ) {
                return Err("wcn3990: STA stop barrier timed out");
            } else {
                self.stop(None, TIMED_OUT, htc, now)?;
            }
        }
        // Admission, final CE submission and retirement share the radio lock.
        // No ordinary data waits in the HTC control queue after retirement.
        if let Some(c) = self
            .connection
            .as_mut()
            .filter(|c| c.phase == Phase::Up && !c.fw_authorized)
        {
            let ordinary = self
                .radio
                .state
                .lock()
                .tx
                .front()
                .is_some_and(|p| p.bytes[12..14] != [0x88, 0x8e]);
            if ordinary && dp.outstanding() == 0 {
                queue(htc, format(wmi::peer_authorize(c.ap.bssid))?)?;
                queue(
                    htc,
                    format(wmi::command(
                        0x1d001,
                        149,
                        &wmi::words(&[0x53480000 | c.token.connection as u32 & 0xffff]),
                    ))?,
                )?;
                c.phase = Phase::Authorizing;
                c.deadline = now + 5_000_000;
            }
        }
        let mut capacity_restored = false;
        for _ in 0..32 {
            if self
                .connection
                .as_ref()
                .is_none_or(|c| c.phase != Phase::Up)
            {
                break;
            }
            let mut s = self.radio.state.lock();
            if !s.up || s.tx.is_empty() {
                break;
            }
            let packet = s.tx.front().unwrap();
            if packet.epoch != s.epoch {
                s.tx.pop_front();
                continue;
            }
            if packet.bytes[12..14] != [0x88, 0x8e]
                && self.connection.as_ref().is_some_and(|c| !c.fw_authorized)
            {
                break;
            }
            let encrypted = self.keys.iter().any(|key| key.kind == KEY_PAIRWISE)
                && packet.bytes[12..14] != [0x88, 0x8e];
            // The supplicant queues message 4 before calling install_key().
            // Firmware may hold M4 pending PTK installation. Submitting it
            // first and then draining TX before the key creates a cycle.
            // Keep M4 host-owned until the confirmed PTK/peer barrier instead.
            if packet.bytes.len() >= 21 && packet.bytes[12..14] == [0x88, 0x8e] {
                let info = u16::from_be_bytes([packet.bytes[19], packet.bytes[20]]);
                if info & 0x308 == 0x308 && !self.keys.iter().any(|key| key.kind == KEY_PAIRWISE) {
                    break;
                }
            }
            let mut frame = sta::encapsulate(&packet.bytes, self.mac, s.peer, self.sequence)
                .map_err(|_| "wcn3990: Ethernet encapsulation")?;
            if encrypted {
                frame[1] |= 0x40;
            }
            let (id, cmd) = match dp.transmit(&frame, 16, encrypted) {
                Ok(v) => v,
                Err("htt: TX queue full") => break,
                Err(e) => return Err(e),
            };
            let (pipe, wire) = htc
                .data_frame(0x300, &cmd)
                .map_err(|_| "wcn3990: HTT data frame")?;
            match bus.send(pipe, &wire, 0) {
                Ok(()) => {
                    capacity_restored |= s.tx.len() == 128;
                    s.tx.pop_front();
                    self.sequence = self.sequence.wrapping_add(1) & 0xfff;
                }
                Err(ce::SendError::Full) => {
                    dp.cancel_unsubmitted(id);
                    break;
                }
                Err(_) => return Err("wcn3990: data CE submission"),
            }
        }
        if capacity_restored {
            scarlet::network::tcp::notify_transmit_available();
        }
        self.deadline = self
            .scan
            .as_ref()
            .map(|s| s.deadline)
            .into_iter()
            .chain(
                self.connection
                    .as_ref()
                    .filter(|c| c.phase != Phase::Up)
                    .map(|c| c.deadline),
            )
            .chain(self.key_work.as_ref().map(|w| w.deadline))
            .min();
        Ok(())
    }
}
