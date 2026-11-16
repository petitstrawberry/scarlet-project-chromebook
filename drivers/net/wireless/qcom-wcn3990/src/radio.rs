// SPDX-License-Identifier: GPL-2.0-only
//! Native wireless-core adapter. Public callbacks only manipulate bounded
//! queues; the WCN worker alone owns firmware state and command ordering.
use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use scarlet::{
    device::{
        network::{DevicePacket, MacAddress},
        wireless::{
            self,
            abi::*,
            auth::{Association, AuthenticationPort, WirelessNotifier},
            core::{BackendEvent, BackendRequest, WirelessBackend},
        },
    },
    network::{InterfaceStats, NetworkInterface, ipv4::Ipv4Address},
    sync::IrqSpinLock,
};
pub struct QueuedPacket {
    pub epoch: u64,
    pub bytes: Vec<u8>,
}
pub struct State {
    pub requests: VecDeque<BackendRequest>,
    pub cancelled: VecDeque<Token>,
    pub completions: VecDeque<BackendEvent>,
    pub port: Option<Arc<dyn AuthenticationPort>>,
    pub notifier: Option<Arc<dyn WirelessNotifier>>,
    pub mac: [u8; 6],
    pub peer: [u8; 6],
    pub up: bool,
    pub epoch: u64,
    pub failed: bool,
    pub security_dirty: bool,
    pub retire_requested: bool,
    pub association: Option<Association>,
    pub rsn: AssociationRsn,
    pub tx: VecDeque<QueuedPacket>,
    pub stats: InterfaceStats,
    pub ip: Option<Ipv4Address>,
}
pub struct Radio {
    pub state: IrqSpinLock<State>,
    wake: fn(),
}
impl Radio {
    pub fn new(mac: [u8; 6], wake: fn()) -> Arc<Self> {
        Arc::new(Self {
            wake,
            state: IrqSpinLock::new(State {
                requests: VecDeque::new(),
                cancelled: VecDeque::new(),
                completions: VecDeque::new(),
                port: None,
                notifier: None,
                mac,
                peer: [0; 6],
                up: false,
                epoch: 1,
                failed: false,
                security_dirty: false,
                retire_requested: false,
                association: None,
                rsn: AssociationRsn::default(),
                tx: VecDeque::new(),
                stats: InterfaceStats::default(),
                ip: None,
            }),
        })
    }
    pub fn notify(&self) {
        let n = self.state.lock().notifier.clone();
        if let Some(n) = n {
            n.notify();
        }
    }
    pub fn complete(&self, token: Token, kind: u32, result: i32, bss: Vec<Bss>) {
        let mut s = self.state.lock();
        if s.completions.len() >= 64 {
            s.failed = true;
        } else {
            s.completions.push_back(BackendEvent {
                token,
                kind,
                result,
                bss,
            });
        }
        drop(s);
        self.notify();
    }
    pub fn down(&self) {
        let mut s = self.state.lock();
        s.up = false;
        s.epoch = s.epoch.saturating_add(1);
        s.peer = [0; 6];
        s.association = None;
        s.tx.clear();
        s.rsn = AssociationRsn::default();
        drop(s);
        self.notify();
        scarlet::network::tcp::notify_transmit_available();
    }
    pub fn deliver(&self, bytes: Vec<u8>) {
        let port = {
            let mut s = self.state.lock();
            if !s.up {
                return;
            }
            s.stats.rx_packets += 1;
            s.stats.rx_bytes += bytes.len() as u64;
            s.port.clone()
        };
        if let Some(port) = port {
            let _ = port.receive_frame(&DevicePacket::with_data(bytes));
        }
    }
    pub fn publish(self: &Arc<Self>) -> Result<(), &'static str> {
        scarlet::println!("[wcn3990] registering controlled transport");
        wireless::register_data_device("wlan0", self.clone(), Box::new(Backend(self.clone())))?;
        scarlet::println!("[wcn3990] controlled transport registered");
        Ok(())
    }
}
pub struct Backend(Arc<Radio>);
impl WirelessBackend for Backend {
    fn attach_notifier(&mut self, n: Arc<dyn WirelessNotifier>) {
        self.0.state.lock().notifier = Some(n);
    }
    fn attach_authentication_port(&mut self, p: Arc<dyn AuthenticationPort>) {
        self.0.state.lock().port = Some(p);
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            mlme: MLME_HOST,
            akm_mask: AKM_OPEN | AKM_EXTERNAL,
            features: FEATURE_DATA | FEATURE_AUTH | FEATURE_RSN,
            max_sessions: 8,
            max_requests: 8,
            max_events: 64,
            max_timeout_ms: 60000,
            reserved: 0,
        }
    }
    fn submit(&mut self, request: BackendRequest, _now: u64) -> Result<(), i32> {
        if !matches!(
            request.command,
            SCAN | CONNECT
                | CONNECT_RSN
                | DISCONNECT
                | INSTALL_KEY
                | DELETE_KEY
                | REPLACE_GROUP_KEY
        ) {
            return Err(NOT_SUPPORTED);
        }
        let mut s = self.0.state.lock();
        if s.failed {
            return Err(DEVICE_GONE);
        }
        if s.requests.len() >= 8 {
            return Err(BUSY);
        }
        if matches!(request.command, CONNECT | CONNECT_RSN | INSTALL_KEY) {
            s.security_dirty = true;
        }
        s.requests.push_back(request);
        drop(s);
        (self.0.wake)();
        Ok(())
    }
    fn cancel(&mut self, token: Token) {
        let mut s = self.0.state.lock();
        s.requests.retain(|r| r.token != token);
        if s.cancelled.len() < 16 {
            s.cancelled.push_back(token);
        } else {
            s.failed = true;
        }
        drop(s);
        (self.0.wake)();
    }
    fn poll(&mut self, _now: u64) -> Option<BackendEvent> {
        self.0.state.lock().completions.pop_front()
    }
    fn retire_security(&mut self) -> Result<(), i32> {
        let mut s = self.0.state.lock();
        if s.failed {
            return Err(DEVICE_GONE);
        }
        if !s.security_dirty && !s.retire_requested {
            return Ok(());
        }
        if s.retire_requested {
            return Err(BUSY);
        }
        s.tx.clear();
        s.requests.clear();
        s.up = false;
        s.epoch = s.epoch.saturating_add(1);
        s.retire_requested = true;
        drop(s);
        (self.0.wake)();
        scarlet::network::tcp::notify_transmit_available();
        Err(BUSY)
    }
    fn association_rsn(&self, device: u64, connection: u64) -> Result<AssociationRsn, i32> {
        let s = self.0.state.lock();
        if !s
            .association
            .is_some_and(|a| a.device == device && a.connection == connection)
        {
            return Err(STALE_CONNECTION);
        }
        Ok(s.rsn)
    }
}
impl NetworkInterface for Radio {
    fn name(&self) -> &str {
        "wlan0"
    }
    fn mac_address(&self) -> MacAddress {
        MacAddress::new(self.state.lock().mac)
    }
    fn link_state(&self) -> u8 {
        if self.state.lock().up {
            wireless::network_abi::LINK_UP
        } else {
            wireless::network_abi::LINK_DOWN
        }
    }
    fn link_epoch(&self) -> u64 {
        self.state.lock().epoch
    }
    fn link_observation(&self) -> scarlet::device::network::LinkObservation {
        let s = self.state.lock();
        scarlet::device::network::LinkObservation {
            mac: MacAddress::new(s.mac),
            state: if s.up {
                wireless::network_abi::LINK_UP
            } else {
                wireless::network_abi::LINK_DOWN
            },
            kind: wireless::network_abi::LINK_KIND_WIFI,
            mtu: 1500,
            epoch: s.epoch,
        }
    }
    fn link_kind(&self) -> u8 {
        wireless::network_abi::LINK_KIND_WIFI
    }
    fn supports_packet_retirement(&self) -> bool {
        true
    }
    fn retire_packets(&self) -> Result<(), &'static str> {
        self.state.lock().tx.clear();
        scarlet::network::tcp::notify_transmit_available();
        Ok(())
    }
    fn ip_address(&self) -> Option<Ipv4Address> {
        self.state.lock().ip
    }
    fn set_ip_address(&self, ip: Ipv4Address) {
        self.state.lock().ip = Some(ip);
    }
    fn clear_ip_address(&self) {
        self.state.lock().ip = None;
    }
    fn send(&self, packet: DevicePacket) -> Result<(), &'static str> {
        if packet.len < 14 || packet.len > packet.data.len() || packet.len > 4096 {
            return Err("wcn3990: invalid Ethernet packet");
        }
        let mut s = self.state.lock();
        if s.failed || !s.up {
            return Err("wcn3990: link down");
        }
        if s.tx.len() >= 128 {
            return Err(scarlet::network::TX_QUEUE_FULL);
        }
        let epoch = s.epoch;
        s.tx.push_back(QueuedPacket {
            epoch,
            bytes: packet.data[..packet.len].to_vec(),
        });
        s.stats.tx_packets += 1;
        s.stats.tx_bytes += packet.len as u64;
        drop(s);
        (self.wake)();
        Ok(())
    }
    fn poll(&self) -> Result<Vec<DevicePacket>, &'static str> {
        Ok(Vec::new())
    }
    fn stats(&self) -> InterfaceStats {
        self.state.lock().stats.clone()
    }
}
