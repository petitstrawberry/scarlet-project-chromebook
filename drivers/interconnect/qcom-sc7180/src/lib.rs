// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! SC7180 RPMh interconnect voting for GPU and Venus paths.
//!
//! Platform firmware describes named paths as provider endpoints. Provider
//! discovery is performed through Scarlet platform devices; this crate does
//! not parse a particular firmware-description format. SC7180 realizes those
//! paths through Bus Clock Managers (BCMs). This driver implements the same
//! BCM aggregation and TCS encoding used by Linux.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use qcom_cmd_db::{read_address, read_aux_data};
use qcom_rpmh_rsc::{ActiveCommand, RpmhRsc, controller};
use scarlet::{
    device::{
        DeviceInfo,
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    early_println,
    sync::IrqSpinLock,
};

const MASTER_GFX3D: u32 = 8;
const MASTER_APPSS_PROC: u32 = 0;
const MASTER_VIDEO_P0: u32 = 6;
const SLAVE_EBI1: u32 = 1;
const SLAVE_VENUS_CFG: u32 = 49;
const QCOM_ICC_TAG_ALWAYS: u32 = 0;
const BCM_VOTE_MASK: u64 = 0x3fff;
const BCM_COMMIT: u32 = 1 << 30;
const BCM_VALID: u32 = 1 << 29;
const BCM_VOTE_X_SHIFT: u32 = 14;
const DEFAULT_VOTE_SCALE: u64 = 1_000;
const UNSET_PEAK_KBPS: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BcmAuxData {
    unit: u32,
    width: u16,
    vcd: u8,
}

#[derive(Clone, Copy)]
enum BcmKind {
    Bandwidth {
        node_buswidth: u16,
        keepalive: bool,
        boot_floor: bool,
    },
    EnableMask(u32),
}

#[derive(Clone, Copy)]
struct Bcm {
    name: &'static str,
    address: u32,
    aux: BcmAuxData,
    kind: BcmKind,
}

struct BcmVoter {
    rsc: Arc<RpmhRsc>,
}

struct PreparedCommand {
    name: &'static str,
    vcd: u8,
    vote_x: u64,
    vote_y: u64,
    command: ActiveCommand,
}

/// A resolved SC7180 `gfx-mem` path.
pub struct GpuMemoryPath {
    voter: Arc<BcmVoter>,
    bcms: [Bcm; 3],
    last_peak_kbps: AtomicU32,
}

/// SC7180 interconnect votes required while the Venus firmware is running.
pub struct VenusInterconnectPaths {
    video_memory: BcmPath,
    cpu_config: BcmPath,
}

struct BcmPath {
    label: &'static str,
    voter: Arc<BcmVoter>,
    bcms: Vec<Bcm>,
    last_peak_kbps: AtomicU32,
}

impl GpuMemoryPath {
    /// Apply a peak GPU-to-memory bandwidth vote in the DT OPP unit, kB/s.
    pub fn set_peak_kbps(&self, peak_kbps: u32) -> Result<(), &'static str> {
        if self.last_peak_kbps.load(Ordering::Acquire) == peak_kbps {
            return Ok(());
        }

        let mut prepared = Vec::with_capacity(self.bcms.len());
        for bcm in self.bcms {
            let (vote_x, vote_y) = bcm.votes(peak_kbps)?;
            prepared.push(PreparedCommand {
                name: bcm.name,
                vcd: bcm.aux.vcd,
                vote_x: vote_x.min(BCM_VOTE_MASK),
                vote_y: vote_y.min(BCM_VOTE_MASK),
                command: ActiveCommand {
                    address: bcm.address,
                    data: 0,
                    wait_for_completion: false,
                },
            });
        }
        prepared.sort_unstable_by_key(|entry| entry.vcd);

        for index in 0..prepared.len() {
            let commit =
                index + 1 == prepared.len() || prepared[index].vcd != prepared[index + 1].vcd;
            prepared[index].command.data =
                encode_bcm_vote(prepared[index].vote_x, prepared[index].vote_y, commit);
            // SC7180's bcm-voter defaults qcom,tcs-wait to ACTIVE_ONLY, so
            // Linux requests a response on each VCD commit command.
            prepared[index].command.wait_for_completion = commit;
        }

        let commands: Vec<_> = prepared.iter().map(|entry| entry.command).collect();
        self.voter.rsc.write_active_batch(&commands)?;
        self.last_peak_kbps.store(peak_kbps, Ordering::Release);

        early_println!(
            "[qcom-sc7180-icc] gfx-mem peak={} kB/s votes={}",
            peak_kbps,
            VoteSummary(&prepared),
        );
        Ok(())
    }
}

impl BcmPath {
    fn set_peak_kbps(&self, peak_kbps: u32) -> Result<(), &'static str> {
        if self.last_peak_kbps.load(Ordering::Acquire) == peak_kbps {
            return Ok(());
        }

        let mut prepared = Vec::with_capacity(self.bcms.len());
        for bcm in self.bcms.iter().copied() {
            let (vote_x, vote_y) = bcm.votes(peak_kbps)?;
            prepared.push(PreparedCommand {
                name: bcm.name,
                vcd: bcm.aux.vcd,
                vote_x: vote_x.min(BCM_VOTE_MASK),
                vote_y: vote_y.min(BCM_VOTE_MASK),
                command: ActiveCommand {
                    address: bcm.address,
                    data: 0,
                    wait_for_completion: false,
                },
            });
        }
        prepared.sort_unstable_by_key(|entry| entry.vcd);
        for index in 0..prepared.len() {
            let commit =
                index + 1 == prepared.len() || prepared[index].vcd != prepared[index + 1].vcd;
            prepared[index].command.data =
                encode_bcm_vote(prepared[index].vote_x, prepared[index].vote_y, commit);
            prepared[index].command.wait_for_completion = commit;
        }

        let commands: Vec<_> = prepared.iter().map(|entry| entry.command).collect();
        self.voter.rsc.write_active_batch(&commands)?;
        self.last_peak_kbps.store(peak_kbps, Ordering::Release);
        early_println!(
            "[qcom-sc7180-icc] {} peak={} kB/s votes={}",
            self.label,
            peak_kbps,
            VoteSummary(&prepared),
        );
        Ok(())
    }
}

impl VenusInterconnectPaths {
    /// Apply Linux's conservative firmware-boot bandwidth floors.
    ///
    /// The memory path is established before the Venus core is powered so the
    /// ARM9 can fetch its firmware through the multimedia NoC. The CPU-config
    /// path keeps host accesses to the wrapper and HFI control registers live.
    pub fn enable_firmware_boot(&self) -> Result<(), &'static str> {
        self.video_memory.set_peak_kbps(20_000)?;
        self.cpu_config.set_peak_kbps(1_000)
    }
}

struct VoteSummary<'a>(&'a [PreparedCommand]);

impl core::fmt::Display for VoteSummary<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, entry) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(",")?;
            }
            write!(
                formatter,
                "{}:{}/{}@vcd{}",
                entry.name, entry.vote_x, entry.vote_y, entry.vcd,
            )?;
        }
        Ok(())
    }
}

impl Bcm {
    fn load(name: &'static str, kind: BcmKind) -> Result<Self, &'static str> {
        let address =
            read_address(name).ok_or("qcom-sc7180-icc: Command DB BCM address is missing")?;
        let bytes = read_aux_data(name)
            .ok_or("qcom-sc7180-icc: Command DB BCM auxiliary data is missing")?;
        let aux = parse_bcm_aux(&bytes)?;
        if matches!(kind, BcmKind::Bandwidth { .. }) && (aux.unit == 0 || aux.width == 0) {
            return Err("qcom-sc7180-icc: bandwidth BCM has invalid unit or width");
        }
        Ok(Self {
            name,
            address,
            aux,
            kind,
        })
    }

    fn votes(self, peak_kbps: u32) -> Result<(u64, u64), &'static str> {
        match self.kind {
            BcmKind::EnableMask(mask) => {
                if peak_kbps == 0 {
                    Ok((0, 0))
                } else {
                    Ok((0, u64::from(mask)))
                }
            }
            BcmKind::Bandwidth {
                node_buswidth,
                keepalive,
                boot_floor,
            } => {
                if node_buswidth == 0 {
                    return Err("qcom-sc7180-icc: BCM node has zero bus width");
                }
                if peak_kbps == 0 {
                    return Ok(if keepalive { (1, 1) } else { (0, 0) });
                }
                let normalized = bcm_div(
                    u64::from(peak_kbps)
                        .checked_mul(u64::from(self.aux.width))
                        .ok_or("qcom-sc7180-icc: peak bandwidth overflow")?,
                    u64::from(node_buswidth),
                );
                let mut vote_y = bcm_div(
                    normalized
                        .checked_mul(DEFAULT_VOTE_SCALE)
                        .ok_or("qcom-sc7180-icc: scaled bandwidth overflow")?,
                    u64::from(self.aux.unit),
                );
                // Linux keeps each node's initial bandwidth as a floor until
                // every interconnect provider reaches sync_state.  Its qcom
                // provider has no get_bw callback, so that initial floor is
                // INT_MAX and the BCM encoder clamps it to the 14-bit maximum.
                // Scarlet does not yet manage the CPU BWMON and display paths
                // sharing SH0/MC0; retaining the same boot floor prevents a
                // GPU-only vote from lowering their firmware handoff state.
                if boot_floor {
                    vote_y = vote_y.max(BCM_VOTE_MASK);
                }
                Ok((0, vote_y))
            }
        }
    }
}

fn bcm_div(value: u64, divisor: u64) -> u64 {
    if value != 0 && value < divisor {
        1
    } else {
        value / divisor
    }
}

fn encode_bcm_vote(vote_x: u64, vote_y: u64, commit: bool) -> u32 {
    let x = vote_x.min(BCM_VOTE_MASK) as u32;
    let y = vote_y.min(BCM_VOTE_MASK) as u32;
    let mut data = (x << BCM_VOTE_X_SHIFT) | y;
    if x != 0 || y != 0 {
        data |= BCM_VALID;
    }
    if commit {
        data |= BCM_COMMIT;
    }
    data
}

fn parse_bcm_aux(bytes: &[u8]) -> Result<BcmAuxData, &'static str> {
    let unit = u32::from_le_bytes(
        bytes
            .get(0..4)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("qcom-sc7180-icc: truncated BCM unit")?,
    );
    let width = u16::from_le_bytes(
        bytes
            .get(4..6)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("qcom-sc7180-icc: truncated BCM width")?,
    );
    let vcd = *bytes.get(6).ok_or("qcom-sc7180-icc: truncated BCM VCD")?;
    Ok(BcmAuxData { unit, width, vcd })
}

static VOTERS: IrqSpinLock<Vec<(u32, Arc<BcmVoter>)>> = IrqSpinLock::new(Vec::new());

fn own_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    for name in ["phandle", "linux,phandle"] {
        let Some(property) = device.property(name) else {
            continue;
        };
        let bytes: [u8; 4] = property
            .value()
            .try_into()
            .map_err(|_| "qcom-sc7180-icc: malformed voter phandle")?;
        let phandle = u32::from_be_bytes(bytes);
        if phandle != 0 {
            return Ok(phandle);
        }
    }
    Err("qcom-sc7180-icc: voter phandle is missing")
}

fn probe_voter(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let parent = device
        .parent_phandle()
        .ok_or("qcom-sc7180-icc: BCM voter parent RSC is missing")?;
    let Some(rsc) = controller(parent) else {
        return probe_defer();
    };
    let phandle = own_phandle(device)?;
    let voter = Arc::new(BcmVoter { rsc });
    let replaced = {
        let mut voters = VOTERS.lock();
        if let Some(index) = voters
            .iter()
            .position(|(registered, _)| *registered == phandle)
        {
            Some(core::mem::replace(&mut voters[index].1, voter))
        } else {
            voters.push((phandle, voter));
            None
        }
    };
    drop(replaced);
    early_println!(
        "[qcom-sc7180-icc] registered BCM voter phandle={:#x} parent={:#x}",
        phandle,
        parent,
    );
    Ok(())
}

fn remove_voter(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let phandle = own_phandle(device)?;
    let removed = {
        let mut voters = VOTERS.lock();
        voters
            .iter()
            .position(|(registered, _)| *registered == phandle)
            .map(|index| voters.remove(index).1)
    };
    drop(removed);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoCProviderKind {
    Config,
    Gem,
    McVirtual,
    Multimedia,
}

#[derive(Clone, Copy)]
struct NoCProvider {
    phandle: u32,
    voter_phandle: u32,
    kind: NoCProviderKind,
}

#[derive(Clone, Copy)]
struct PathEndpoints {
    source: NoCProvider,
    source_id: u32,
    source_tag: u32,
    destination: NoCProvider,
    destination_id: u32,
    destination_tag: u32,
}

static NOC_PROVIDERS: IrqSpinLock<Vec<NoCProvider>> = IrqSpinLock::new(Vec::new());

fn be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn noc_provider_kind(device: &PlatformDeviceInfo) -> Result<NoCProviderKind, &'static str> {
    let compatible = device.compatible();
    if compatible.contains(&"qcom,sc7180-config-noc") {
        Ok(NoCProviderKind::Config)
    } else if compatible.contains(&"qcom,sc7180-gem-noc") {
        Ok(NoCProviderKind::Gem)
    } else if compatible.contains(&"qcom,sc7180-mc-virt") {
        Ok(NoCProviderKind::McVirtual)
    } else if compatible.contains(&"qcom,sc7180-mmss-noc") {
        Ok(NoCProviderKind::Multimedia)
    } else {
        Err("qcom-sc7180-icc: unsupported NoC provider")
    }
}

fn probe_noc_provider(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let provider = NoCProvider {
        phandle: own_phandle(device)?,
        voter_phandle: device
            .property("qcom,bcm-voters")
            .and_then(|property| be_u32(property.value()))
            .filter(|phandle| *phandle != 0)
            .ok_or("qcom-sc7180-icc: NoC provider BCM voter is missing")?,
        kind: noc_provider_kind(device)?,
    };
    let mut providers = NOC_PROVIDERS.lock();
    if let Some(index) = providers
        .iter()
        .position(|registered| registered.phandle == provider.phandle)
    {
        providers[index] = provider;
    } else {
        providers.push(provider);
    }
    early_println!(
        "[qcom-sc7180-icc] registered {:?} provider={:#x} voter={:#x}",
        provider.kind,
        provider.phandle,
        provider.voter_phandle,
    );
    Ok(())
}

fn remove_noc_provider(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let phandle = own_phandle(device)?;
    let mut providers = NOC_PROVIDERS.lock();
    if let Some(index) = providers
        .iter()
        .position(|provider| provider.phandle == phandle)
    {
        providers.remove(index);
    }
    Ok(())
}

fn noc_provider(phandle: u32) -> Result<NoCProvider, &'static str> {
    NOC_PROVIDERS
        .lock()
        .iter()
        .find(|provider| provider.phandle == phandle)
        .copied()
        .ok_or(scarlet::device::manager::PROBE_DEFER)
}

fn named_path(device: &PlatformDeviceInfo, name: &str) -> Result<PathEndpoints, &'static str> {
    let names = device
        .property("interconnect-names")
        .and_then(|property| property.as_string_list())
        .ok_or("qcom-sc7180-icc: interconnect names are missing")?;
    let index = names
        .iter()
        .position(|candidate| *candidate == name)
        .ok_or("qcom-sc7180-icc: named interconnect path is missing")?;
    let bytes = device
        .property("interconnects")
        .ok_or("qcom-sc7180-icc: interconnect endpoints are missing")?
        .value();
    const CELLS_PER_PATH: usize = 6;
    const BYTES_PER_PATH: usize = CELLS_PER_PATH * core::mem::size_of::<u32>();
    if bytes.len() != names.len() * BYTES_PER_PATH {
        return Err("qcom-sc7180-icc: interconnect paths have invalid cell counts");
    }
    let path = bytes
        .get(index * BYTES_PER_PATH..(index + 1) * BYTES_PER_PATH)
        .ok_or("qcom-sc7180-icc: named interconnect path is truncated")?;
    let cells: Vec<u32> = path
        .chunks_exact(4)
        .map(|cell| u32::from_be_bytes(cell.try_into().unwrap_or([0; 4])))
        .collect();
    let [
        source_phandle,
        source_id,
        source_tag,
        destination_phandle,
        destination_id,
        destination_tag,
    ] = cells.as_slice()
    else {
        return Err("qcom-sc7180-icc: named interconnect path is malformed");
    };
    let source = noc_provider(*source_phandle)?;
    let destination = noc_provider(*destination_phandle)?;
    if source.voter_phandle != destination.voter_phandle {
        return Err("qcom-sc7180-icc: path endpoints use different BCM voters");
    }
    Ok(PathEndpoints {
        source,
        source_id: *source_id,
        source_tag: *source_tag,
        destination,
        destination_id: *destination_id,
        destination_tag: *destination_tag,
    })
}

fn voter_for_path(path: PathEndpoints) -> Result<Arc<BcmVoter>, &'static str> {
    VOTERS
        .lock()
        .iter()
        .find(|(phandle, _)| *phandle == path.source.voter_phandle)
        .map(|(_, voter)| Arc::clone(voter))
        .ok_or(scarlet::device::manager::PROBE_DEFER)
}

/// Resolve the SC7180 `gfx-mem` interconnect path described by a GPU node.
pub fn gpu_memory_path(device: &PlatformDeviceInfo) -> Result<GpuMemoryPath, &'static str> {
    let path = named_path(device, "gfx-mem")?;
    if path.source.kind != NoCProviderKind::Gem
        || path.destination.kind != NoCProviderKind::McVirtual
        || path.source_id != MASTER_GFX3D
        || path.destination_id != SLAVE_EBI1
        || path.source_tag != QCOM_ICC_TAG_ALWAYS
        || path.destination_tag != QCOM_ICC_TAG_ALWAYS
    {
        return Err("qcom-sc7180-icc: gfx-mem endpoints do not match SC7180");
    }
    let voter = voter_for_path(path)?;

    // Linux sc7180.c attaches SH0 to qns_llcc and ACV/MC0 to ebi.  The
    // corresponding node bus widths are 16 and 4 bytes respectively.
    let sh0 = Bcm::load(
        "SH0",
        BcmKind::Bandwidth {
            node_buswidth: 16,
            keepalive: true,
            boot_floor: true,
        },
    )?;
    let mc0 = Bcm::load(
        "MC0",
        BcmKind::Bandwidth {
            node_buswidth: 4,
            keepalive: true,
            boot_floor: true,
        },
    )?;
    let acv = Bcm::load("ACV", BcmKind::EnableMask(1 << 3))?;

    Ok(GpuMemoryPath {
        voter,
        bcms: [sh0, mc0, acv],
        last_peak_kbps: AtomicU32::new(UNSET_PEAK_KBPS),
    })
}

/// Resolve the two named SC7180 paths required by the Venus firmware.
///
/// Platform bindings are consumed through [`PlatformDeviceInfo`] and probed
/// NoC providers. No FDT parser or firmware-format-specific node traversal is
/// used here.
pub fn venus_interconnect_paths(
    device: &PlatformDeviceInfo,
) -> Result<VenusInterconnectPaths, &'static str> {
    let video = named_path(device, "video-mem")?;
    if video.source.kind != NoCProviderKind::Multimedia
        || video.destination.kind != NoCProviderKind::McVirtual
        || video.source_id != MASTER_VIDEO_P0
        || video.destination_id != SLAVE_EBI1
        || video.source_tag != QCOM_ICC_TAG_ALWAYS
        || video.destination_tag != QCOM_ICC_TAG_ALWAYS
    {
        return Err("qcom-sc7180-icc: video-mem endpoints do not match SC7180");
    }
    let cpu = named_path(device, "cpu-cfg")?;
    if cpu.source.kind != NoCProviderKind::Gem
        || cpu.destination.kind != NoCProviderKind::Config
        || cpu.source_id != MASTER_APPSS_PROC
        || cpu.destination_id != SLAVE_VENUS_CFG
        || cpu.source_tag != QCOM_ICC_TAG_ALWAYS
        || cpu.destination_tag != QCOM_ICC_TAG_ALWAYS
    {
        return Err("qcom-sc7180-icc: cpu-cfg endpoints do not match SC7180");
    }

    let video_voter = voter_for_path(video)?;
    let cpu_voter = voter_for_path(cpu)?;
    let bandwidth = |name, node_buswidth, keepalive| {
        Bcm::load(
            name,
            BcmKind::Bandwidth {
                node_buswidth,
                keepalive,
                // Until Scarlet owns every consumer sharing these BCMs, keep
                // Linux's pre-sync firmware floor instead of lowering another
                // active path with a partial software aggregate.
                boot_floor: true,
            },
        )
    };

    Ok(VenusInterconnectPaths {
        video_memory: BcmPath {
            label: "video-mem",
            voter: video_voter,
            bcms: vec![
                bandwidth("MM1", 32, false)?,
                bandwidth("MM2", 32, false)?,
                bandwidth("SH0", 16, true)?,
                bandwidth("MC0", 4, true)?,
                Bcm::load("ACV", BcmKind::EnableMask(1 << 3))?,
            ],
            last_peak_kbps: AtomicU32::new(UNSET_PEAK_KBPS),
        },
        cpu_config: BcmPath {
            label: "cpu-cfg",
            voter: cpu_voter,
            bcms: vec![
                bandwidth("SH4", 16, false)?,
                bandwidth("SN12", 8, false)?,
                bandwidth("CN0", 4, true)?,
            ],
            last_peak_kbps: AtomicU32::new(UNSET_PEAK_KBPS),
        },
    })
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-bcm-voter",
            probe_voter,
            remove_voter,
            vec!["qcom,bcm-voter"],
        )),
        DriverPriority::Critical,
    );
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-noc-provider",
            probe_noc_provider,
            remove_noc_provider,
            vec![
                "qcom,sc7180-config-noc",
                "qcom,sc7180-gem-noc",
                "qcom,sc7180-mc-virt",
                "qcom,sc7180-mmss-noc",
            ],
        )),
        DriverPriority::Critical,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_INTERCONNECT_ANCHOR: fn() = force_link;

/// Keep the voter platform driver and public path implementation linked.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcm_aux_matches_command_db_layout() {
        assert_eq!(
            parse_bcm_aux(&[0x40, 0x42, 0x0f, 0x00, 0x10, 0x00, 0x03, 0x00]),
            Ok(BcmAuxData {
                unit: 1_000_000,
                width: 16,
                vcd: 3,
            }),
        );
    }

    #[test]
    fn peak_vote_uses_linux_bcm_scaling() {
        let bcm = Bcm {
            name: "SH0",
            address: 0,
            aux: BcmAuxData {
                unit: 1_000_000,
                width: 16,
                vcd: 0,
            },
            kind: BcmKind::Bandwidth {
                node_buswidth: 16,
                keepalive: true,
                boot_floor: false,
            },
        };
        assert_eq!(bcm.votes(8_532_000), Ok((0, 8_532)));
        assert_eq!(bcm.votes(0), Ok((1, 1)));
    }

    #[test]
    fn bcm_command_encodes_commit_valid_and_votes() {
        assert_eq!(
            encode_bcm_vote(2, 3, true),
            BCM_COMMIT | BCM_VALID | (2 << BCM_VOTE_X_SHIFT) | 3,
        );
        assert_eq!(encode_bcm_vote(0, 0, true), BCM_COMMIT);
    }
}
