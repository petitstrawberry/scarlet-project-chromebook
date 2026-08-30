// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Google SC7180 Trogdor-family audio machine routing.
//!
//! The machine driver resolves the secondary-MI2S CPU and codec endpoints from
//! the firmware DAI link, leaving the LPASS controller and MAX98360A codec as
//! independently reusable drivers.

extern crate alloc;

use alloc::{boxed::Box, vec};

use scarlet::{
    device::{
        fdt::FdtManager,
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    early_println,
};

const SECONDARY_MI2S: u32 = 1;

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn probe(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("google-sc7180-audio: FDT unavailable")?;
    let sound = fdt
        .all_nodes()
        .find(|node| {
            node.compatible().is_some_and(|compatible| {
                compatible
                    .all()
                    .any(|entry| matches!(entry, "google,sc7180-coachz" | "google,sc7180-trogdor"))
            })
        })
        .ok_or("google-sc7180-audio: sound node not found")?;
    let link = sound
        .children()
        .find(|node| {
            node.property("reg")
                .and_then(|property| read_be_u32(property.value, 0))
                == Some(SECONDARY_MI2S)
        })
        .ok_or("google-sc7180-audio: secondary MI2S link not found")?;
    let cpu = link
        .children()
        .find(|node| node.name.split('@').next() == Some("cpu"))
        .ok_or("google-sc7180-audio: CPU DAI endpoint missing")?;
    let codec = link
        .children()
        .find(|node| node.name.split('@').next() == Some("codec"))
        .ok_or("google-sc7180-audio: codec DAI endpoint missing")?;
    let cpu_dai = cpu
        .property("sound-dai")
        .ok_or("google-sc7180-audio: CPU sound-dai missing")?
        .value;
    let codec_dai = codec
        .property("sound-dai")
        .ok_or("google-sc7180-audio: codec sound-dai missing")?
        .value;
    let cpu_phandle =
        read_be_u32(cpu_dai, 0).ok_or("google-sc7180-audio: malformed CPU sound-dai")?;
    let codec_phandle =
        read_be_u32(codec_dai, 0).ok_or("google-sc7180-audio: malformed codec sound-dai")?;
    let manager = DeviceManager::get_manager();
    let provider = manager
        .get_audio_dai_provider_by_phandle(cpu_phandle)
        .ok_or(PROBE_DEFER)?;
    let cells = provider.sound_dai_cells();
    if cpu_dai.len() != (cells + 1) * 4 {
        return Err("google-sc7180-audio: malformed CPU DAI specifier");
    }
    let mut spec = alloc::vec::Vec::with_capacity(cells);
    for index in 0..cells {
        spec.push(
            read_be_u32(cpu_dai, (index + 1) * 4)
                .ok_or("google-sc7180-audio: truncated CPU DAI specifier")?,
        );
    }
    let codec = manager
        .get_audio_codec_by_phandle(codec_phandle)
        .ok_or(PROBE_DEFER)?;
    provider.attach_playback_codec_tdm(&spec, codec, 1)?;
    early_println!(
        "[google-sc7180-audio] routed secondary MI2S provider={:#x} codec={:#x}",
        cpu_phandle,
        codec_phandle,
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "google-sc7180-trogdor-audio",
            probe,
            remove,
            vec!["google,sc7180-coachz", "google,sc7180-trogdor"],
        )),
        DriverPriority::Late,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_GOOGLE_SC7180_TROGDOR_AUDIO_ANCHOR: fn() = force_link;

/// Keep the Trogdor-family audio machine driver linked into module builds.
#[inline(never)]
pub fn force_link() {}
