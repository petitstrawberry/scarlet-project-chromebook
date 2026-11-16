// SPDX-License-Identifier: GPL-2.0-only
use crate::{
    qmi,
    qrtr::{self, Address},
};
use alloc::{boxed::Box, collections::VecDeque, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use qcom_glink::{Edge, Listener, protocol::Event};
use scarlet::{
    device::{
        iommu::{
            DmaAddr, DmaContext, DmaMapping, IommuDomainConfig, IommuDomainType, IommuMapFlags,
            Iova,
        },
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    mem::address::PhysAddr,
    sync::{IrqSpinLock, Mutex, Waker},
    time,
};
static DEVICE: IrqSpinLock<Option<Arc<Wcn>>> = IrqSpinLock::new(None);
static WAKER: Waker = Waker::new_uninterruptible("wcn3990");
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn wake() {
    SEQUENCE.fetch_add(1, Ordering::Release);
    WAKER.wake_one();
}
struct Wcn {
    edge: Arc<Edge>,
    events: IrqSpinLock<VecDeque<Event>>,
    failed: AtomicBool,
    msa: u64,
    msa_size: usize,
    _firmware_mapping: DmaMapping,
    _firmware_dma: DmaContext,
    rmtfs: IrqSpinLock<Option<crate::rmtfs::Service<crate::rmtfs::native::Shared>>>,
    board_container: Vec<u8>,
    power: crate::power::Power,
    bus: Arc<crate::ce::Bus>,
    host_memory: Mutex<Vec<crate::ce::HostMemory>>,
    datapath: Mutex<Option<crate::datapath::Datapath>>,
    radio: Mutex<Option<Arc<crate::radio::Radio>>>,
}
impl Listener for Wcn {
    fn event(&self, event: Event) {
        let mut events = self.events.lock();
        if events.len() >= 128 {
            self.failed.store(true, Ordering::Release);
        } else {
            events.push_back(event);
        }
        drop(events);
        wake();
    }
    fn failed(&self) {
        self.failed.store(true, Ordering::Release);
        wake();
    }
}
struct Transaction {
    id: u16,
    serial: u16,
    deadline: u64,
}
struct Control {
    device: Arc<Wcn>,
    server: Option<Address>,
    next_transaction: u16,
    inflight: Option<Transaction>,
    hello: bool,
    transmitted: u8,
    waiting: VecDeque<Vec<u8>>,
    fw_ready: bool,
    msa_ready: bool,
    board: Option<Vec<u8>>,
    board_offset: usize,
    rmtfs: crate::rmtfs::Service<crate::rmtfs::native::Shared>,
    htc: crate::htc::Htc,
    transport_phase: u8,
    transport_deadline: Option<u64>,
    wmi_service_ready: Option<Vec<u8>>,
    mac: Option<[u8; 6]>,
    htt_version: Option<(u8, u8)>,
    pipe_seen: u16,
    htt_seen: u32,
    tx_status_seen: u8,
    mlme: Option<crate::mlme::Mlme>,
}
fn word(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
impl Control {
    fn announce_rmtfs(&self, node: u32) -> Result<(), &'static str> {
        self.send(&qrtr::control(
            4,
            Address {
                node,
                port: qrtr::CTRL,
            },
            [14, 1, qrtr::LOCAL, crate::rmtfs::PORT],
        ))
    }
    fn send(&self, packet: &[u8]) -> Result<(), &'static str> {
        self.device
            .edge
            .send("IPCRTR", packet)
            .map_err(|_| "wcn3990: GLINK send failed")
    }
    fn request(&mut self, id: u16, body: &[u8]) -> Result<(), &'static str> {
        if self.inflight.is_some() {
            return Err("wcn3990: overlapping QMI transaction");
        }
        let server = self.server.ok_or("wcn3990: WLFW unavailable")?;
        let serial = self.next_transaction;
        self.next_transaction = serial
            .checked_add(1)
            .ok_or("wcn3990: transaction IDs exhausted")?;
        let data = qmi::request(serial, id, body).map_err(|_| "wcn3990: invalid QMI request")?;
        if self.waiting.len() >= 8 {
            return Err("wcn3990: QRTR backlog full");
        }
        self.waiting.push_back(qrtr::encode(
            1,
            Address {
                node: qrtr::LOCAL,
                port: qrtr::CLIENT,
            },
            server,
            self.transmitted == 4,
            &data,
        ));
        self.inflight = Some(Transaction {
            id,
            serial,
            deadline: time::current_time() + 30_000_000,
        });
        self.flush()
    }
    fn flush(&mut self) -> Result<(), &'static str> {
        while self.transmitted < 10 {
            let Some(packet) = self.waiting.front() else {
                break;
            };
            self.send(packet)?;
            self.waiting.pop_front();
            self.transmitted += 1;
        }
        Ok(())
    }
    fn register(&mut self) -> Result<(), &'static str> {
        let mut body = Vec::new();
        qmi::tlv(&mut body, 0x10, &[1]).unwrap();
        qmi::tlv(&mut body, 0x13, &[1]).unwrap();
        qmi::tlv(&mut body, 0x15, &0x4b4e454cu32.to_le_bytes()).unwrap();
        self.request(0x20, &body)
    }
    fn event(&mut self, event: Event) -> Result<(), &'static str> {
        match event {
            Event::Opened(name) if name == "IPCRTR" => {
                scarlet::println!("[wcn3990] GLINK IPCRTR connected");
                self.send(&qrtr::control(
                    2,
                    Address {
                        node: qrtr::BROADCAST,
                        port: qrtr::CTRL,
                    },
                    [0; 4],
                ))?;
                self.announce_rmtfs(qrtr::BROADCAST)?;
            }
            Event::Closed(name) if name == "IPCRTR" => return Err("wcn3990: IPCRTR disconnected"),
            Event::Packet(name, b) if name == "IPCRTR" => self.packet(&b)?,
            _ => {}
        }
        Ok(())
    }
    fn packet(&mut self, b: &[u8]) -> Result<(), &'static str> {
        let p = qrtr::parse(b).map_err(|_| "wcn3990: malformed QRTR datagram")?;
        if let Some(resume) = p.resume() {
            self.send(&resume)?;
        }
        match p.kind {
            2 => {
                if !p.control_valid() {
                    return Err("wcn3990: malformed HELLO");
                }
                if !self.hello {
                    self.hello = true;
                    self.send(&qrtr::control(
                        2,
                        Address {
                            node: p.source.node,
                            port: qrtr::CTRL,
                        },
                        [0; 4],
                    ))?;
                    self.announce_rmtfs(p.source.node)?;
                }
            }
            4 => {
                let f = p
                    .fields()
                    .map_err(|_| "wcn3990: malformed service announcement")?;
                scarlet::println!(
                    "[wcn3990] QRTR service={:#x} instance={:#x} node={} port={}",
                    f[0],
                    f[1],
                    f[2],
                    f[3]
                );
                if f[0] == 0x45 && f[1] & 0xff == 1 {
                    let address = Address {
                        node: f[2],
                        port: f[3],
                    };
                    if address.port == 0 || address.port >= qrtr::CTRL {
                        return Err("wcn3990: invalid WLFW address");
                    }
                    if let Some(previous) = self.server {
                        if previous != address {
                            return Err("wcn3990: WLFW address changed");
                        }
                    } else {
                        self.server = Some(address);
                        scarlet::println!(
                            "[wcn3990] WLFW v1 found node={} port={}",
                            address.node,
                            address.port
                        );
                        self.register()?;
                    }
                }
            }
            5 => {
                let f = p
                    .fields()
                    .map_err(|_| "wcn3990: malformed service removal")?;
                if self.server
                    == Some(Address {
                        node: f[2],
                        port: f[3],
                    })
                {
                    return Err("wcn3990: WLFW service removed");
                }
            }
            3 => {
                if self.server.is_some_and(|s| s.node == p.source.node) {
                    return Err("wcn3990: modem QRTR node departed");
                }
            }
            7 => {
                let f = p.fields().map_err(|_| "wcn3990: malformed resume")?;
                if self.server
                    == Some(Address {
                        node: f[0],
                        port: f[1],
                    })
                {
                    self.transmitted = 0;
                    self.flush()?;
                }
            }
            1 if p.destination.port == crate::rmtfs::PORT => {
                let reply = self
                    .rmtfs
                    .request(p.source.node, p.data)
                    .map_err(|_| "wcn3990: malformed RMTFS request")?;
                let id = u16::from_le_bytes(reply[3..5].try_into().unwrap());
                if id == 1 || id == 4 {
                    scarlet::println!(
                        "[wcn3990] RMTFS request {} result={:?}",
                        id,
                        qmi::Message::parse(&reply).map(|m| m.result())
                    );
                }
                self.send(&qrtr::encode(
                    1,
                    Address {
                        node: qrtr::LOCAL,
                        port: crate::rmtfs::PORT,
                    },
                    p.source,
                    false,
                    &reply,
                ))?;
            }
            1 if self.server == Some(p.source) && p.destination.port == qrtr::CLIENT => {
                self.qmi(p.data)?
            }
            _ => {}
        }
        Ok(())
    }
    fn qmi(&mut self, b: &[u8]) -> Result<(), &'static str> {
        let m = qmi::Message::parse(b).map_err(|_| "wcn3990: malformed QMI response")?;
        if m.kind == 4 {
            match m.id {
                0x21 => {
                    self.fw_ready = true;
                    scarlet::println!("[wcn3990] firmware ready indication");
                }
                0x2b => {
                    self.msa_ready = true;
                    scarlet::println!("[wcn3990] MSA ready indication");
                    self.maybe_board()?;
                }
                _ => {}
            }
            return self.maybe_start_transport();
        }
        let Some(txn) = self.inflight.as_ref() else {
            return Ok(());
        };
        if m.kind != 2 || m.transaction != txn.serial || m.id != txn.id {
            return Ok(());
        }
        if let Err(error) = m.result() {
            if m.id != 0x34 || error != qmi::Error::Rejected(0x5e) {
                scarlet::println!("[wcn3990] QMI {:#x} rejected: {:?}", m.id, error);
                return Err("wcn3990: WLFW request rejected");
            }
        }
        self.inflight = None;
        self.response(&m)?;
        self.maybe_start_transport()
    }
    fn response(&mut self, m: &qmi::Message<'_>) -> Result<(), &'static str> {
        match m.id {
            0x20 => {
                scarlet::println!("[wcn3990] indications registered");
                let mut body = Vec::new();
                qmi::tlv(&mut body, 0x10, &0u32.to_le_bytes()).unwrap();
                self.request(0x34, &body)?;
            }
            0x34 => {
                let mut body = Vec::new();
                qmi::tlv(&mut body, 1, &self.device.msa.to_le_bytes()).unwrap();
                qmi::tlv(&mut body, 2, &(self.device.msa_size as u32).to_le_bytes()).unwrap();
                self.request(0x2d, &body)?;
            }
            0x2d => {
                let regions = m.tlv(3).ok_or("wcn3990: missing MSA regions")?;
                if regions.is_empty()
                    || regions[0] > 2
                    || regions.len() != 1 + regions[0] as usize * 13
                {
                    return Err("wcn3990: invalid MSA regions");
                }
                for region in regions[1..].chunks_exact(13) {
                    let address = u64::from_le_bytes(region[..8].try_into().unwrap());
                    let size = word(&region[8..12]) as u64;
                    if size == 0
                        || address < self.device.msa
                        || address
                            .checked_add(size)
                            .is_none_or(|end| end > self.device.msa + self.device.msa_size as u64)
                        || region[12] > 1
                    {
                        return Err("wcn3990: MSA region outside reserved RAM");
                    }
                }
                scarlet::println!("[wcn3990] MSA regions validated; firmware-fixed permissions");
                self.request(0x2e, &[])?;
            }
            0x2e => self.request(0x24, &[])?,
            0x24 => {
                let chip = m
                    .tlv(0x10)
                    .filter(|b| b.len() == 8)
                    .ok_or("wcn3990: missing chip identity")?;
                let board = m
                    .tlv(0x11)
                    .filter(|b| b.len() == 4)
                    .ok_or("wcn3990: missing board identity")?;
                scarlet::println!(
                    "[wcn3990] chip={} family={} board={}",
                    word(chip),
                    word(&chip[4..]),
                    word(board)
                );
                let selected = crate::board::select(
                    &self.device.board_container,
                    word(chip),
                    word(board),
                    None,
                )
                .map_err(|_| "wcn3990: matching board data unavailable")?;
                scarlet::println!("[wcn3990] selected board data bytes={}", selected.len());
                self.board = Some(selected.to_vec());
                self.maybe_board()?;
            }
            0x25 => {
                let board = self
                    .board
                    .as_ref()
                    .ok_or("wcn3990: missing active board transfer")?;
                self.board_offset = (self.board_offset + 6144).min(board.len());
                if self.board_offset == board.len() {
                    scarlet::println!("[wcn3990] board data accepted");
                    self.board = None;
                    let mut body = Vec::new();
                    qmi::tlv(&mut body, 1, &[0]).unwrap();
                    self.request(0x26, &body)?;
                } else {
                    self.maybe_board()?;
                }
            }
            0x26 => scarlet::println!("[wcn3990] calibration report accepted"),
            0x23 => {
                let mut body = Vec::new();
                qmi::tlv(&mut body, 1, &0u32.to_le_bytes()).unwrap();
                self.transport_phase = 2;
                self.request(0x22, &body)?;
            }
            0x22 => {
                self.device.bus.activate()?;
                self.transport_phase = 3;
                self.transport_deadline = Some(time::current_time() + 5_000_000);
                scarlet::println!("[wcn3990] CE rings active; waiting for HTC ready");
            }
            _ => {}
        }
        Ok(())
    }
    fn maybe_start_transport(&mut self) -> Result<(), &'static str> {
        if self.fw_ready && self.inflight.is_none() && self.transport_phase == 0 {
            self.device.power.enable()?;
            self.transport_phase = 1;
            self.request(0x23, &crate::wlfw::configuration())?;
        }
        Ok(())
    }
    fn pump_transport(&mut self) -> Result<(), &'static str> {
        if self.transport_phase < 3 {
            return Ok(());
        }
        let activity = self.device.bus.process()?;
        let _tx_progress = activity.tx_progress;
        for (pipe, bytes) in activity.received {
            if self.pipe_seen & (1 << pipe) == 0 {
                self.pipe_seen |= 1 << pipe;
                scarlet::println!("[wcn3990] first CE RX pipe={} bytes={}", pipe, bytes.len());
            }
            if matches!(pipe, 1 | 2 | 9 | 10) {
                self.htc.receive(pipe, &bytes).map_err(|error| {
                    scarlet::println!(
                        "[wcn3990] HTC RX pipe={} bytes={} error={:?}",
                        pipe,
                        bytes.len(),
                        error
                    );
                    "wcn3990: HTC protocol failure"
                })?;
            } else if pipe == 5 {
                // SNOC CE5 carries bare HTT indications, without an HTC
                // header (ath10k_snoc_htt_rx_cb). CE1 carries HTC-wrapped HTT.
                if self.htc.events.len() >= 128 {
                    return Err("wcn3990: HTT receive event queue overflow");
                }
                self.htc.events.push_back(crate::htc::Event::Data {
                    service: 0x300,
                    bytes,
                });
            }
        }
        while let Some((index, out)) = self.htc.next() {
            match self
                .device
                .bus
                .send(out.pipe, &out.bytes, u16::from(out.endpoint))
            {
                Ok(()) => self
                    .htc
                    .submitted(index)
                    .map_err(|_| "wcn3990: HTC TX accounting failed")?,
                Err(crate::ce::SendError::Full) => break,
                Err(_) => return Err("wcn3990: HTC DMA transmission failed"),
            }
        }
        while let Some(event) = self.htc.events.pop_front() {
            match event {
                crate::htc::Event::Ready { credits, size } => scarlet::println!(
                    "[wcn3990] HTC ready credits={} credit-size={}",
                    credits,
                    size
                ),
                crate::htc::Event::Connected { service, endpoint } => {
                    scarlet::println!("[wcn3990] HTC service={:#x} endpoint={}", service, endpoint)
                }
                crate::htc::Event::Started => {
                    self.transport_deadline = Some(time::current_time() + 5_000_000);
                    scarlet::println!("[wcn3990] HTC setup sent; waiting for WMI service ready");
                }
                crate::htc::Event::Data { service, bytes } => {
                    if service == 0x100 && bytes.len() >= 4 {
                        let id = word(&bytes) & 0xffffff;
                        if !matches!(id, 0x7001 | 0x4002 | 0x3001) {
                            scarlet::println!(
                                "[wcn3990] WMI event={:#x} bytes={}",
                                id,
                                bytes.len()
                            );
                        }
                        if id == 1 {
                            self.initialize_wmi(bytes)?;
                        } else if id == 2 {
                            self.mac = Some(
                                crate::wmi::ready(&bytes)
                                    .map_err(|_| "wcn3990: WMI ready rejected initialization")?,
                            );
                            self.transport_deadline = None;
                            self.transport_phase = 4;
                            scarlet::println!("[wcn3990] WMI initialized");
                            self.htc
                                .send(0x300, &[0; 4])
                                .map_err(|_| "wcn3990: HTT version request enqueue failed")?;
                            self.transport_deadline = Some(time::current_time() + 5_000_000);
                            wake();
                        } else if let Some(mlme) = &mut self.mlme {
                            let mut dp = self.device.datapath.lock();
                            mlme.wmi_event(
                                id,
                                &bytes,
                                &mut self.htc,
                                dp.as_mut().ok_or("wcn3990: MLME without datapath")?,
                                time::current_time(),
                            )?;
                            wake();
                        }
                    } else if service == 0x300 && bytes.len() >= 4 && bytes[0] == 0 {
                        if self.htt_version.is_some() || bytes[2] != 3 {
                            return Err("wcn3990: unsupported or duplicate HTT version response");
                        }
                        self.htt_version = Some((bytes[2], bytes[1]));
                        self.transport_deadline = None;
                        self.transport_phase = 5;
                        scarlet::println!("[wcn3990] HTT version {}.{}", bytes[2], bytes[1]);
                        self.initialize_datapath()?;
                    } else if service == 0x300 {
                        if let Some(id) = bytes.first().copied().filter(|id| *id < 32) {
                            if self.htt_seen & (1 << id) == 0 {
                                self.htt_seen |= 1 << id;
                                scarlet::println!(
                                    "[wcn3990] HTT indication={:#x} bytes={}",
                                    id,
                                    bytes.len()
                                );
                            }
                        }
                        if bytes.first() == Some(&0x12) {
                            let frames = self
                                .device
                                .datapath
                                .lock()
                                .as_mut()
                                .ok_or("wcn3990: RX before datapath")?
                                .receive(&bytes)?;
                            if let Some(mlme) = &mut self.mlme {
                                mlme.receive(frames);
                            }
                        } else if bytes.first() == Some(&7) {
                            let done = self
                                .device
                                .datapath
                                .lock()
                                .as_mut()
                                .ok_or("wcn3990: TX done before datapath")?
                                .complete(&bytes)?;
                            if !done.is_empty() {
                                scarlet::network::tcp::notify_transmit_available();
                            }
                            if let Some((_, status)) = done.first() {
                                if self.tx_status_seen & (1 << status) == 0 {
                                    self.tx_status_seen |= 1 << status;
                                    scarlet::println!(
                                        "[wcn3990] HTT TX completion status={}",
                                        status
                                    );
                                }
                            }
                        } else if let Some(mlme) = &mut self.mlme {
                            let mut dp = self.device.datapath.lock();
                            mlme.htt_event(
                                &bytes,
                                &mut self.htc,
                                dp.as_mut().ok_or("wcn3990: MLME without datapath")?,
                                time::current_time(),
                            )?;
                            wake();
                        }
                    }
                }
            }
        }
        if let Some(mlme) = &mut self.mlme {
            let mut dp = self.device.datapath.lock();
            mlme.pump(
                &mut self.htc,
                dp.as_mut().ok_or("wcn3990: MLME without datapath")?,
                &self.device.bus,
                time::current_time(),
            )?;
            if self.htc.next().is_some() {
                wake();
            }
        }
        Ok(())
    }
    fn initialize_datapath(&mut self) -> Result<(), &'static str> {
        let datapath = crate::datapath::Datapath::prepare(&self.device.bus)?;
        let configuration = datapath.configuration()?;
        // Retain DMA allocations in the device before firmware sees them.
        *self.device.datapath.lock() = Some(datapath);
        for cmd in configuration {
            self.htc
                .send(0x300, &cmd)
                .map_err(|_| "wcn3990: HTT setup queue")?;
        }
        let mut mac = self.mac.ok_or("wcn3990: missing MAC")?;
        if mac == [0; 6] || mac[0] & 1 != 0 {
            if scarlet::random::RandomManager::get_random_bytes(&mut mac) != mac.len() {
                return Err("wcn3990: MAC entropy unavailable");
            }
            mac[0] = (mac[0] & !1) | 2;
            self.mac = Some(mac);
            scarlet::println!("[wcn3990] firmware has no usable MAC; using local unicast address");
        }
        let ready = crate::wmi::ServiceReady::parse(
            self.wmi_service_ready
                .as_ref()
                .ok_or("wcn3990: no radio capabilities")?,
        )
        .map_err(|_| "wcn3990: capabilities lost")?;
        let regdomain = crate::wmi::world_regdomain(&ready);
        for cmd in [
            crate::wmi::pdev_parameter(1, 3),
            crate::wmi::pdev_parameter(2, 3),
            crate::wmi::pdev_parameter(3, 20),
            crate::wmi::pdev_parameter(4, 20),
            crate::wmi::vdev_create(mac),
            crate::wmi::passive_channels(),
            regdomain,
        ] {
            self.htc
                .send(0x100, &cmd.map_err(|_| "wcn3990: STA setup format")?)
                .map_err(|_| "wcn3990: STA setup queue")?;
        }
        let radio = crate::radio::Radio::new(mac, wake);
        *self.device.radio.lock() = Some(radio.clone());
        radio.publish()?;
        let mut mlme = crate::mlme::Mlme::new(radio, mac);
        mlme.bootstrap(&mut self.htc, time::current_time())?;
        self.mlme = Some(mlme);
        self.transport_deadline = None;
        scarlet::println!("[wcn3990] native STA wlan0 published; starting passive discovery");
        wake();
        Ok(())
    }
    fn initialize_wmi(&mut self, bytes: Vec<u8>) -> Result<(), &'static str> {
        if self.wmi_service_ready.is_some() {
            return Err("wcn3990: duplicate WMI service ready");
        }
        let ready = crate::wmi::ServiceReady::parse(&bytes).map_err(|error| {
            scarlet::println!("[wcn3990] WMI service ready parse: {:?}", error);
            "wcn3990: unsupported WMI service ready"
        })?;
        scarlet::println!("[wcn3990] regulatory capability={:x?}", ready.regulatory);
        scarlet::println!(
            "[wcn3990] WMI build={:#x} chains={} memory-requests={} RX-reorder={} mgmt-WMI={}",
            ready.build,
            ready.chains,
            ready.memory.len(),
            ready.service(65),
            ready.service(76)
        );
        let mut allocations = Vec::new();
        let mut chunks = Vec::new();
        for request in &ready.memory {
            let bytes = request
                .bytes()
                .map_err(|_| "wcn3990: excessive WMI memory request")?;
            let memory = self.device.bus.host_memory(bytes)?;
            chunks.push(memory.description(request.id));
            allocations.push(memory);
        }
        let init = crate::wmi::initialize(&ready, &chunks)
            .map_err(|_| "wcn3990: invalid WMI init plan")?;
        // Publish retention before exposing any DMA address to the firmware.
        *self.device.host_memory.lock() = allocations;
        self.htc
            .send(0x100, &init)
            .map_err(|_| "wcn3990: WMI init enqueue failed")?;
        self.wmi_service_ready = Some(bytes);
        self.transport_deadline = Some(time::current_time() + 5_000_000);
        wake();
        Ok(())
    }
    fn maybe_board(&mut self) -> Result<(), &'static str> {
        if !self.msa_ready || self.inflight.is_some() {
            return Ok(());
        }
        let Some(board) = self.board.as_ref() else {
            return Ok(());
        };
        let end = (self.board_offset + 6144).min(board.len());
        let data = &board[self.board_offset..end];
        let mut body = Vec::new();
        qmi::tlv(&mut body, 1, &[1]).unwrap();
        qmi::tlv(&mut body, 0x10, &0u32.to_le_bytes()).unwrap();
        qmi::tlv(&mut body, 0x11, &(board.len() as u32).to_le_bytes()).unwrap();
        qmi::tlv(
            &mut body,
            0x12,
            &((self.board_offset / 6144) as u32).to_le_bytes(),
        )
        .unwrap();
        let mut value = Vec::with_capacity(2 + data.len());
        value.extend_from_slice(&(data.len() as u16).to_le_bytes());
        value.extend_from_slice(data);
        qmi::tlv(&mut body, 0x13, &value).unwrap();
        qmi::tlv(&mut body, 0x14, &[(end == board.len()) as u8]).unwrap();
        self.request(0x25, &body)
    }
}
fn load_board() -> Result<Vec<u8>, &'static str> {
    use scarlet::{fs::manager::get_global_vfs_manager_safe, object::KernelObject};
    let vfs = get_global_vfs_manager_safe().ok_or("wcn3990: VFS unavailable")?;
    let object = vfs
        .open("/lib/firmware/ath10k/WCN3990/hw1.0/board-2.bin", 0)
        .map_err(|error| {
            scarlet::println!("[wcn3990] board container open: {:?}", error);
            "wcn3990: required board data missing"
        })?;
    let KernelObject::File(file) = object else {
        return Err("wcn3990: board data is not a regular file");
    };
    let size = file
        .metadata()
        .map_err(|_| "wcn3990: board data metadata unavailable")?
        .size;
    if size == 0 || size > 2 * 1024 * 1024 {
        return Err("wcn3990: invalid board container size");
    }
    let mut bytes = vec![0; size];
    let mut offset = 0;
    while offset < size {
        let n = file
            .read(&mut bytes[offset..])
            .map_err(|_| "wcn3990: board data read failed")?;
        if n == 0 {
            return Err("wcn3990: truncated board container");
        }
        offset += n;
    }
    Ok(bytes)
}
fn worker() {
    let device = DEVICE.lock().clone().expect("wcn3990: missing device");
    let task = scarlet::task::mytask().expect("wcn3990: worker has no task");
    let mut control = Control {
        device: device.clone(),
        server: None,
        next_transaction: 1,
        inflight: None,
        hello: false,
        transmitted: 0,
        waiting: VecDeque::new(),
        fw_ready: false,
        msa_ready: false,
        board: None,
        board_offset: 0,
        rmtfs: device
            .rmtfs
            .lock()
            .take()
            .expect("wcn3990: RMTFS already owned"),
        htc: crate::htc::Htc::new(),
        transport_phase: 0,
        transport_deadline: None,
        wmi_service_ready: None,
        mac: None,
        htt_version: None,
        pipe_seen: 0,
        htt_seen: 0,
        tx_status_seen: 0,
        mlme: None,
    };
    let startup_deadline = time::current_time() + 30_000_000;
    loop {
        let seen = SEQUENCE.load(Ordering::Acquire);
        let events: Vec<_> = device.events.lock().drain(..).collect();
        for event in events {
            if let Err(error) = control.event(event) {
                scarlet::println!("[wcn3990] {}", error);
                device.failed.store(true, Ordering::Release);
                break;
            }
        }
        if !device.failed.load(Ordering::Acquire) {
            if let Err(error) = control.pump_transport() {
                scarlet::println!("[wcn3990] {}", error);
                device.failed.store(true, Ordering::Release);
            }
        }
        if device.failed.load(Ordering::Acquire) {
            device.bus.quarantine();
            if let Some(radio) = device.radio.lock().as_ref().cloned() {
                radio.state.lock().failed = true;
                radio.down();
                radio.complete(
                    Default::default(),
                    scarlet::device::wireless::core::BACKEND_REMOVED,
                    scarlet::device::wireless::abi::DEVICE_GONE,
                    Vec::new(),
                );
            }
            return;
        }
        let deadline = control.inflight.as_ref().map(|t| t.deadline).or_else(|| {
            if control.server.is_none() {
                Some(startup_deadline)
            } else {
                None
            }
        });
        let deadline = match (deadline, control.transport_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let timeout = deadline.map(|deadline| {
            deadline
                .saturating_sub(time::current_time())
                .saturating_mul(1000)
        });
        if timeout == Some(0) {
            device.bus.startup_snapshot();
            scarlet::println!("[wcn3990] initialization/command response deadline expired");
            device.failed.store(true, Ordering::Release);
            return;
        }
        let mlme_timeout = control
            .mlme
            .as_ref()
            .and_then(|m| m.deadline)
            .map(|deadline| {
                deadline
                    .saturating_sub(time::current_time())
                    .saturating_mul(1000)
            });
        let timeout = match (timeout, mlme_timeout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let retry = device.bus.retry_deadline().map(|deadline| {
            deadline
                .saturating_sub(time::current_time())
                .saturating_mul(1000)
        });
        let timeout = match (timeout, retry) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        WAKER.wait_with_condition(task.get_id(), task.get_trapframe(), timeout, 0, || {
            SEQUENCE.load(Ordering::Acquire) != seen
        });
    }
}
fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if DEVICE.lock().is_some() {
        return Err("wcn3990: duplicate device");
    }
    let edge = qcom_glink::modem_edge().ok_or(PROBE_DEFER)?;
    // Acquire immutable firmware before userspace pivots its mount namespace;
    // asynchronous WLFW callbacks must not depend on that later root.
    let board_container = load_board()?;
    let power = crate::power::Power::prepare(device)?;
    let bus = crate::ce::Bus::prepare(device, wake)?;
    let memory =
        DeviceManager::get_manager().resolve_platform_memory_region(device, "memory-region", 0)?;
    if device.property("qcom,msa-fixed-perm").is_none() {
        return Err("wcn3990: dynamic MSA permissions not supported on this board");
    }
    let size = memory.size()?;
    if size == 0 || size > u32::MAX as usize {
        return Err("wcn3990: invalid MSA reservation");
    }
    let firmware_dma = DeviceManager::get_manager().resolve_platform_child_dma_context(
        device,
        "wifi-firmware",
        IommuDomainConfig {
            domain_type: IommuDomainType::Dma,
            iova_base: Iova::ZERO,
            iova_size: 1u64 << 32,
        },
    )?;
    if firmware_dma.iommu.is_none() {
        return Err("wcn3990: missing firmware SMMU context");
    }
    let firmware_mapping = firmware_dma
        .map_phys_at_owned(
            DmaAddr::new(memory.start),
            PhysAddr::new(memory.start),
            size,
            IommuMapFlags::READ | IommuMapFlags::WRITE,
        )
        .map_err(|_| "wcn3990: firmware MSA mapping failed")?;
    firmware_dma
        .restore_iommu()
        .map_err(|_| "wcn3990: firmware SMMU synchronization failed")?;
    let wcn = Arc::new(Wcn {
        edge: edge.clone(),
        events: IrqSpinLock::new(VecDeque::new()),
        failed: AtomicBool::new(false),
        msa: memory.start,
        msa_size: size,
        _firmware_mapping: firmware_mapping,
        _firmware_dma: firmware_dma,
        rmtfs: IrqSpinLock::new(Some(crate::rmtfs::native::prepare()?)),
        board_container,
        power,
        bus,
        host_memory: Mutex::new(Vec::new()),
        datapath: Mutex::new(None),
        radio: Mutex::new(None),
    });
    edge.bind(wcn.clone())?;
    *DEVICE.lock() = Some(wcn);
    let task = scarlet::task::new_kernel_task(String::from("wcn3990"), 1, worker);
    task.init();
    scarlet::sched::scheduler::add_task(task, 0);
    wake();
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("wcn3990: firmware transport retained")
}
fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-wcn3990",
            probe,
            remove,
            vec!["qcom,wcn3990-wifi"],
        )),
        DriverPriority::Standard,
    );
}
scarlet::driver_initcall!(register_driver);
