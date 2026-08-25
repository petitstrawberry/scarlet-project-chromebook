// SPDX-License-Identifier: GPL-2.0-only

//! Canonical, offline-generated IR3 programs for the A618 graphics subset.
//!
//! This crate is deliberately transport and operating-system independent.
//! The kernel and pure code generator share stable variant identities and the
//! exact same immutable bytes; only the kernel is allowed to materialize the
//! bytes in executable GPU memory.

#![no_std]

/// Pinned Mesa revision used by the offline generator.
pub const MESA_SHA: &str = "3f1b217baffffa00cb8f53e158713a33e1bd4632";
/// SHA256 of the generated Mesa metadata consumed by `pack_state.py`.
pub const MESA_METADATA_SHA256: &str =
    "84a727b282d051a97cc9a3d43c5921dbe3751845db7c2de39d8f241727e701b9";
/// SHA256 of the generated packed-state metadata represented below.
pub const PACKED_STATE_SHA256: &str =
    "c4c746958a7c1dbbd2e2860299194f4ebe5a0d6e8401a1c7cc71494ed6f2de48";
/// Required start alignment and fixed allocation stride of every program.
pub const SHADER_ALIGNMENT: usize = 128;
/// Exact byte count of every canonical program.
pub const SHADER_SIZE: usize = 128;

/// Stable identity of a kernel-owned canonical A618 IR3 program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ShaderVariant {
    VsStride16Pos2 = 0,
    VsStride16Pos2Uv2 = 1,
    VsStride40Pos4 = 2,
    VsStride40Pos4Color4 = 3,
    VsStride40Pos4Color4Uv2 = 4,
    FsSolid = 5,
    FsVertexColor = 6,
    FsTextureRgba = 7,
    FsTextureAlphaMask = 8,
    FsTextureVertexColorRgba = 9,
    VsStride24Pos4Uv2 = 10,
    VsStride28Pos4Color3 = 11,
    FsTextureRgbIgnoreAlpha = 12,
}

impl ShaderVariant {
    pub const ALL: [Self; 13] = [
        Self::VsStride16Pos2,
        Self::VsStride16Pos2Uv2,
        Self::VsStride40Pos4,
        Self::VsStride40Pos4Color4,
        Self::VsStride40Pos4Color4Uv2,
        Self::FsSolid,
        Self::FsVertexColor,
        Self::FsTextureRgba,
        Self::FsTextureAlphaMask,
        Self::FsTextureVertexColorRgba,
        Self::VsStride24Pos4Uv2,
        Self::VsStride28Pos4Color3,
        Self::FsTextureRgbIgnoreAlpha,
    ];

    pub const fn from_raw(raw: u16) -> Option<Self> {
        if raw < Self::ALL.len() as u16 {
            Some(Self::ALL[raw as usize])
        } else {
            None
        }
    }

    pub const fn raw(self) -> u16 {
        self as u16
    }

    pub const fn offset(self) -> usize {
        self as usize * SHADER_ALIGNMENT
    }

    pub const fn is_vertex(self) -> bool {
        matches!(
            self,
            Self::VsStride16Pos2
                | Self::VsStride16Pos2Uv2
                | Self::VsStride40Pos4
                | Self::VsStride40Pos4Color4
                | Self::VsStride40Pos4Color4Uv2
                | Self::VsStride24Pos4Uv2
                | Self::VsStride28Pos4Color3
        )
    }

    pub const fn bytes(self) -> &'static [u8; SHADER_SIZE] {
        match self {
            Self::VsStride16Pos2 => include_bytes!("../artifacts/a618/vs_stride16_pos2.bin"),
            Self::VsStride16Pos2Uv2 => {
                include_bytes!("../artifacts/a618/vs_stride16_pos2_uv2.bin")
            }
            Self::VsStride40Pos4 => include_bytes!("../artifacts/a618/vs_stride40_pos4.bin"),
            Self::VsStride40Pos4Color4 => {
                include_bytes!("../artifacts/a618/vs_stride40_pos4_color4.bin")
            }
            Self::VsStride40Pos4Color4Uv2 => {
                include_bytes!("../artifacts/a618/vs_stride40_pos4_color4_uv2.bin")
            }
            Self::FsSolid => include_bytes!("../artifacts/a618/fs_solid.bin"),
            Self::FsVertexColor => include_bytes!("../artifacts/a618/fs_vertex_color.bin"),
            Self::FsTextureRgba => include_bytes!("../artifacts/a618/fs_texture_rgba.bin"),
            Self::FsTextureAlphaMask => {
                include_bytes!("../artifacts/a618/fs_texture_alpha_mask.bin")
            }
            Self::FsTextureVertexColorRgba => {
                include_bytes!("../artifacts/a618/fs_texture_vertex_color_rgba.bin")
            }
            Self::VsStride24Pos4Uv2 => {
                include_bytes!("../artifacts/a618/vs_stride24_pos4_uv2.bin")
            }
            Self::VsStride28Pos4Color3 => {
                include_bytes!("../artifacts/a618/vs_stride28_pos4_color3.bin")
            }
            Self::FsTextureRgbIgnoreAlpha => {
                include_bytes!("../artifacts/a618/fs_texture_rgb_ignore_alpha.bin")
            }
        }
    }
}

/// Mesa-derived A618 vertex-stage payloads for one canonical program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexMeta {
    pub sp_vs_cntl_0: u32,
    pub sp_vs_instr_size: u32,
    pub sp_vs_const_config: u32,
    pub sp_vs_config: u32,
    pub vfd_dest_cntl: &'static [u32],
    pub vfd_cntl_1_6: [u32; 6],
}

/// Mesa-derived A618 fragment-stage payloads for one canonical program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentMeta {
    pub sp_ps_cntl_0: u32,
    pub sp_ps_instr_size: u32,
    pub sp_ps_const_config: u32,
    pub sp_ps_config: u32,
    pub sp_ps_wave_cntl: u32,
    pub gras_cl_interp_cntl: u32,
    pub rb_interp_cntl: u32,
    pub rb_ps_input_cntl: u32,
    pub initial_tex_load_cntl: u32,
    pub initial_tex_load_cmd: &'static [u32],
    pub sp_reg_prog_id: [u32; 4],
    pub sp_ps_output_cntl: u32,
    pub sp_ps_output_reg: &'static [u32],
    pub sp_ps_output_mask: u32,
    pub rb_ps_output_cntl: u32,
    pub rb_ps_output_mask: u32,
}

/// Stage-specific metadata generated from the pinned Mesa variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderMeta {
    Vertex(VertexMeta),
    Fragment(FragmentMeta),
}

const PROG_ID_SOLID: [u32; 4] = [0xfcfc_fcfc, 0xfcfc_fcfc, 0xfcfc_fcfc, 0xfcfc];
const PROG_ID_VARYING: [u32; 4] = [0xfcfc_fcfc, 0xfcfc_fc00, 0xfcfc_fcfc, 0xfcfc];
const VFD_CNTL_1_6_NO_SYSVALS: [u32; 6] = [
    0xfcfc_fcfc,
    0x0000_fcfc,
    0xfcfc_fcfc,
    0x0000_00fc,
    0x0000_fcfc,
    0,
];

/// Return immutable generated metadata for one stable program identity.
pub const fn shader_meta(variant: ShaderVariant) -> ShaderMeta {
    use ShaderVariant::*;
    match variant {
        VsStride16Pos2 => ShaderMeta::Vertex(VertexMeta {
            sp_vs_cntl_0: 0x0010_0180,
            sp_vs_instr_size: 1,
            sp_vs_const_config: 0x103,
            sp_vs_config: 0x100,
            vfd_dest_cntl: &[3],
            vfd_cntl_1_6: VFD_CNTL_1_6_NO_SYSVALS,
        }),
        VsStride16Pos2Uv2 => ShaderMeta::Vertex(VertexMeta {
            sp_vs_cntl_0: 0x0010_0180,
            sp_vs_instr_size: 1,
            sp_vs_const_config: 0x103,
            sp_vs_config: 0x100,
            vfd_dest_cntl: &[3, 35],
            vfd_cntl_1_6: VFD_CNTL_1_6_NO_SYSVALS,
        }),
        VsStride40Pos4 => ShaderMeta::Vertex(VertexMeta {
            sp_vs_cntl_0: 0x0010_0180,
            sp_vs_instr_size: 1,
            sp_vs_const_config: 0x103,
            sp_vs_config: 0x100,
            vfd_dest_cntl: &[15],
            vfd_cntl_1_6: VFD_CNTL_1_6_NO_SYSVALS,
        }),
        VsStride40Pos4Color4 => ShaderMeta::Vertex(VertexMeta {
            sp_vs_cntl_0: 0x0010_0200,
            sp_vs_instr_size: 1,
            sp_vs_const_config: 0x103,
            sp_vs_config: 0x100,
            vfd_dest_cntl: &[15, 79],
            vfd_cntl_1_6: VFD_CNTL_1_6_NO_SYSVALS,
        }),
        VsStride40Pos4Color4Uv2 => ShaderMeta::Vertex(VertexMeta {
            sp_vs_cntl_0: 0x0010_0280,
            sp_vs_instr_size: 1,
            sp_vs_const_config: 0x103,
            sp_vs_config: 0x100,
            vfd_dest_cntl: &[15, 79, 131],
            vfd_cntl_1_6: VFD_CNTL_1_6_NO_SYSVALS,
        }),
        VsStride24Pos4Uv2 => ShaderMeta::Vertex(VertexMeta {
            sp_vs_cntl_0: 0x0010_0200,
            sp_vs_instr_size: 1,
            sp_vs_const_config: 0x103,
            sp_vs_config: 0x100,
            vfd_dest_cntl: &[15, 67],
            vfd_cntl_1_6: VFD_CNTL_1_6_NO_SYSVALS,
        }),
        VsStride28Pos4Color3 => ShaderMeta::Vertex(VertexMeta {
            sp_vs_cntl_0: 0x0010_0200,
            sp_vs_instr_size: 1,
            sp_vs_const_config: 0x103,
            sp_vs_config: 0x100,
            vfd_dest_cntl: &[15, 71],
            vfd_cntl_1_6: VFD_CNTL_1_6_NO_SYSVALS,
        }),
        FsSolid => ShaderMeta::Fragment(FragmentMeta {
            sp_ps_cntl_0: 0x8110_0080,
            sp_ps_instr_size: 1,
            sp_ps_const_config: 0x102,
            sp_ps_config: 0x100,
            sp_ps_wave_cntl: 1,
            gras_cl_interp_cntl: 0,
            rb_interp_cntl: 0,
            rb_ps_input_cntl: 0,
            initial_tex_load_cntl: 8,
            initial_tex_load_cmd: &[],
            sp_reg_prog_id: PROG_ID_SOLID,
            sp_ps_output_cntl: 0xfcfc_fc00,
            sp_ps_output_reg: &[0],
            sp_ps_output_mask: 15,
            rb_ps_output_cntl: 0,
            rb_ps_output_mask: 15,
        }),
        FsVertexColor => fragment_varying(0x8150_0180, 0, &[], &[6]),
        FsTextureRgba => fragment_varying(0x8150_0180, 1, &[0x83c2_0000], &[6]),
        FsTextureAlphaMask => fragment_varying(0x8150_0180, 1, &[0x8202_0000], &[6]),
        FsTextureVertexColorRgba => fragment_varying(0x8150_0200, 1, &[0x83c2_0004], &[10]),
        FsTextureRgbIgnoreAlpha => fragment_varying(0x8150_0180, 1, &[0x81c2_0000], &[5]),
    }
}

const fn fragment_varying(
    cntl: u32,
    tex_cntl: u32,
    tex_cmd: &'static [u32],
    output: &'static [u32],
) -> ShaderMeta {
    ShaderMeta::Fragment(FragmentMeta {
        sp_ps_cntl_0: cntl,
        sp_ps_instr_size: 1,
        sp_ps_const_config: 0x102,
        sp_ps_config: if tex_cntl == 0 { 0x100 } else { 0x20300 },
        sp_ps_wave_cntl: 3,
        gras_cl_interp_cntl: 1,
        rb_interp_cntl: 0x401,
        rb_ps_input_cntl: 0,
        initial_tex_load_cntl: tex_cntl,
        initial_tex_load_cmd: tex_cmd,
        sp_reg_prog_id: PROG_ID_VARYING,
        sp_ps_output_cntl: 0xfcfc_fc00,
        sp_ps_output_reg: output,
        sp_ps_output_mask: 15,
        rb_ps_output_cntl: 0,
        rb_ps_output_mask: 15,
    })
}

/// Stable allowed shader pairing and its generated VPC linkage payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkMeta {
    pub vs: ShaderVariant,
    pub fs: ShaderVariant,
    pub sp_vs_output_reg: &'static [u32],
    pub sp_vs_vpc_dest_reg: &'static [u32],
    pub vpc_vs_cntl: u32,
    pub pc_vs_cntl: u32,
    pub sp_vs_output_cntl: u32,
    pub lm_transfer_disable: [u32; 4],
    pub vpc_ps_cntl: u32,
}

/// Complete fixed shader/link state selected by one normalized pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PipelineVariant {
    Stride16Solid,
    Stride16TextureRgba,
    Stride16TextureAlphaMask,
    Stride40Solid,
    Stride40VertexColor,
    Stride40TextureVertexColorRgba,
    Stride24Solid,
    Stride24TextureRgba,
    Stride24TextureRgbIgnoreAlpha,
    Stride28VertexColor,
}

impl PipelineVariant {
    pub const ALL: [Self; 10] = [
        Self::Stride16Solid,
        Self::Stride16TextureRgba,
        Self::Stride16TextureAlphaMask,
        Self::Stride40Solid,
        Self::Stride40VertexColor,
        Self::Stride40TextureVertexColorRgba,
        Self::Stride24Solid,
        Self::Stride24TextureRgba,
        Self::Stride24TextureRgbIgnoreAlpha,
        Self::Stride28VertexColor,
    ];
}

/// Non-address fixed state which completes the canonical pipeline identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipelineStateMeta {
    pub stride: u32,
    pub vfd_fetch: &'static [u32],
    pub sampler_dwords: Option<[u32; 4]>,
    pub source_over: bool,
}

/// Mesa/XML-derived fixed VFD/sampler/blend state for a canonical pipeline.
pub const fn pipeline_state_meta(variant: PipelineVariant) -> PipelineStateMeta {
    use PipelineVariant::*;
    const POS2: &[u32] = &[0xc670_0000, 1];
    const POS2_UV2: &[u32] = &[0xc670_0000, 1, 0xc670_0100, 1];
    const POS4: &[u32] = &[0xc820_0000, 1];
    const POS4_UV2: &[u32] = &[0xc820_0000, 1, 0xc670_0200, 1];
    const POS4_COLOR3: &[u32] = &[0xc820_0000, 1, 0xc740_0200, 1];
    const POS4_COLOR4: &[u32] = &[0xc820_0000, 1, 0xc820_0200, 1];
    const POS4_COLOR4_UV2: &[u32] = &[0xc820_0000, 1, 0xc820_0200, 1, 0xc670_0400, 1];
    match variant {
        Stride16Solid => fixed(16, POS2, None, true),
        Stride16TextureRgba | Stride16TextureAlphaMask => {
            fixed(16, POS2_UV2, Some([0x920, 0x40, 0, 0]), true)
        }
        Stride40Solid => fixed(40, POS4, None, true),
        Stride40VertexColor => fixed(40, POS4_COLOR4, None, true),
        Stride40TextureVertexColorRgba => {
            fixed(40, POS4_COLOR4_UV2, Some([0x920, 0x40, 0, 0]), true)
        }
        Stride24Solid => fixed(24, POS4, None, true),
        Stride24TextureRgba | Stride24TextureRgbIgnoreAlpha => {
            fixed(24, POS4_UV2, Some([0x92a, 0x40, 0x20, 0]), true)
        }
        Stride28VertexColor => fixed(28, POS4_COLOR3, None, false),
    }
}

const fn fixed(
    stride: u32,
    vfd_fetch: &'static [u32],
    sampler_dwords: Option<[u32; 4]>,
    source_over: bool,
) -> PipelineStateMeta {
    PipelineStateMeta {
        stride,
        vfd_fetch,
        sampler_dwords,
        source_over,
    }
}

/// Return the only shader pair/linkage allowed for a normalized variant.
pub const fn link_meta(variant: PipelineVariant) -> LinkMeta {
    use PipelineVariant::*;
    use ShaderVariant::*;
    match variant {
        Stride16Solid => link(
            VsStride16Pos2,
            FsSolid,
            &[3846],
            &[0],
            0x00ff_0004,
            4,
            1,
            [u32::MAX; 4],
            0xff00_ff00,
        ),
        Stride16TextureRgba => link(
            VsStride16Pos2Uv2,
            FsTextureRgba,
            &[0x0f08_0302],
            &[512],
            0x00ff_0206,
            6,
            2,
            [0xffff_fffc, u32::MAX, u32::MAX, u32::MAX],
            0xff01_ff02,
        ),
        Stride16TextureAlphaMask => link(
            VsStride16Pos2Uv2,
            FsTextureAlphaMask,
            &[0x0f08_0302],
            &[512],
            0x00ff_0206,
            6,
            2,
            [0xffff_fffc, u32::MAX, u32::MAX, u32::MAX],
            0xff01_ff02,
        ),
        Stride40Solid => link(
            VsStride40Pos4,
            FsSolid,
            &[3848],
            &[0],
            0x00ff_0004,
            4,
            1,
            [u32::MAX; 4],
            0xff00_ff00,
        ),
        Stride40VertexColor => link(
            VsStride40Pos4Color4,
            FsVertexColor,
            &[0x0f0c_0f04],
            &[1024],
            0x00ff_0408,
            8,
            2,
            [0xffff_fff0, u32::MAX, u32::MAX, u32::MAX],
            0xff01_ff04,
        ),
        Stride40TextureVertexColorRgba => link(
            VsStride40Pos4Color4Uv2,
            FsTextureVertexColorRgba,
            &[0x0308_0f04, 3854],
            &[394240],
            0x00ff_060a,
            10,
            3,
            [0xffff_ffc0, u32::MAX, u32::MAX, u32::MAX],
            0xff01_ff06,
        ),
        Stride24Solid => link(
            VsStride40Pos4,
            FsSolid,
            &[3848],
            &[0],
            0x00ff_0004,
            4,
            1,
            [u32::MAX; 4],
            0xff00_ff00,
        ),
        Stride24TextureRgba => link(
            VsStride24Pos4Uv2,
            FsTextureRgba,
            &[0x0f0a_0304],
            &[512],
            0x00ff_0206,
            6,
            2,
            [0xffff_fffc, u32::MAX, u32::MAX, u32::MAX],
            0xff01_ff02,
        ),
        Stride24TextureRgbIgnoreAlpha => link(
            VsStride24Pos4Uv2,
            FsTextureRgbIgnoreAlpha,
            &[0x0f0a_0304],
            &[512],
            0x00ff_0206,
            6,
            2,
            [0xffff_fffc, u32::MAX, u32::MAX, u32::MAX],
            0xff01_ff02,
        ),
        Stride28VertexColor => link(
            VsStride28Pos4Color3,
            FsVertexColor,
            &[0x0f0c_0f04],
            &[1024],
            0x00ff_0408,
            8,
            2,
            [0xffff_fff0, u32::MAX, u32::MAX, u32::MAX],
            0xff01_ff04,
        ),
    }
}

const fn link(
    vs: ShaderVariant,
    fs: ShaderVariant,
    outputs: &'static [u32],
    destinations: &'static [u32],
    vpc_vs_cntl: u32,
    pc_vs_cntl: u32,
    output_count: u32,
    lm_transfer_disable: [u32; 4],
    vpc_ps_cntl: u32,
) -> LinkMeta {
    LinkMeta {
        vs,
        fs,
        sp_vs_output_reg: outputs,
        sp_vs_vpc_dest_reg: destinations,
        vpc_vs_cntl,
        pc_vs_cntl,
        sp_vs_output_cntl: output_count,
        lm_transfer_disable,
        vpc_ps_cntl,
    }
}

/// Total byte count of the kernel-owned, 128-byte-strided pack allocation.
pub const PACK_SIZE: usize = ShaderVariant::ALL.len() * SHADER_ALIGNMENT;

/// Copy all canonical programs into one aligned kernel-owned allocation.
pub fn copy_pack(destination: &mut [u8]) -> bool {
    if destination.len() < PACK_SIZE {
        return false;
    }
    for variant in ShaderVariant::ALL {
        let start = variant.offset();
        destination[start..start + SHADER_SIZE].copy_from_slice(variant.bytes());
    }
    true
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn object<'a>(json: &'a str, name: &str) -> &'a str {
        let marker = std::format!("\"{name}\": {{");
        let start = json.find(&marker).unwrap() + marker.len() - 1;
        let mut depth = 0;
        for (relative, byte) in json.as_bytes()[start..].iter().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &json[start..=start + relative];
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated generated JSON object")
    }

    fn number(json: &str, name: &str) -> u32 {
        let marker = std::format!("\"{name}\": ");
        let tail = &json[json.find(&marker).unwrap() + marker.len()..];
        tail.split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn array(json: &str, name: &str) -> std::vec::Vec<u32> {
        let marker = std::format!("\"{name}\": [");
        let tail = &json[json.find(&marker).unwrap() + marker.len()..];
        tail[..tail.find(']').unwrap()]
            .split(',')
            .filter_map(|value| value.trim().parse().ok())
            .collect()
    }

    #[test]
    fn stable_ids_are_dense_and_programs_are_aligned() {
        for (index, variant) in ShaderVariant::ALL.into_iter().enumerate() {
            assert_eq!(variant.raw() as usize, index);
            assert_eq!(variant.offset() % SHADER_ALIGNMENT, 0);
            assert_eq!(variant.bytes().len(), SHADER_SIZE);
            assert_eq!(ShaderVariant::from_raw(index as u16), Some(variant));
        }
        assert_eq!(ShaderVariant::from_raw(13), None);
    }

    #[test]
    fn typed_metadata_is_chained_to_schema_v2_generated_state() {
        let generated = include_str!("../artifacts/a618/packed-state.json");
        assert!(generated.contains(MESA_METADATA_SHA256));
        let names = [
            "vs_stride16_pos2",
            "vs_stride16_pos2_uv2",
            "vs_stride40_pos4",
            "vs_stride40_pos4_color4",
            "vs_stride40_pos4_color4_uv2",
            "fs_solid",
            "fs_vertex_color",
            "fs_texture_rgba",
            "fs_texture_alpha_mask",
            "fs_texture_vertex_color_rgba",
            "vs_stride24_pos4_uv2",
            "vs_stride28_pos4_color3",
            "fs_texture_rgb_ignore_alpha",
        ];
        for (variant, name) in ShaderVariant::ALL.into_iter().zip(names) {
            let generated = object(object(generated, "variants"), name);
            match shader_meta(variant) {
                ShaderMeta::Vertex(meta) => {
                    assert_eq!(meta.sp_vs_cntl_0, number(generated, "sp_vs_cntl_0"));
                    assert_eq!(meta.sp_vs_instr_size, number(generated, "sp_vs_instr_size"));
                    assert_eq!(
                        meta.sp_vs_const_config,
                        number(generated, "sp_vs_const_config")
                    );
                    assert_eq!(meta.sp_vs_config, number(generated, "sp_vs_config"));
                    assert_eq!(meta.vfd_dest_cntl, array(generated, "vfd_dest_cntl"));
                    assert_eq!(
                        meta.vfd_cntl_1_6,
                        array(generated, "vfd_cntl_1_6").as_slice()
                    );
                }
                ShaderMeta::Fragment(meta) => {
                    assert_eq!(meta.sp_ps_cntl_0, number(generated, "sp_ps_cntl_0"));
                    assert_eq!(meta.sp_ps_instr_size, number(generated, "sp_ps_instr_size"));
                    assert_eq!(
                        meta.sp_ps_const_config,
                        number(generated, "sp_ps_const_config")
                    );
                    assert_eq!(meta.sp_ps_config, number(generated, "sp_ps_config"));
                    assert_eq!(meta.sp_ps_wave_cntl, number(generated, "sp_ps_wave_cntl"));
                    assert_eq!(
                        meta.gras_cl_interp_cntl,
                        number(generated, "gras_cl_interp_cntl")
                    );
                    assert_eq!(meta.rb_interp_cntl, number(generated, "rb_interp_cntl"));
                    assert_eq!(meta.rb_ps_input_cntl, number(generated, "rb_ps_input_cntl"));
                    assert_eq!(
                        meta.initial_tex_load_cntl,
                        number(generated, "sp_ps_initial_tex_load_cntl")
                    );
                    assert_eq!(
                        meta.initial_tex_load_cmd,
                        array(generated, "sp_ps_initial_tex_load_cmd")
                    );
                    assert_eq!(
                        meta.sp_reg_prog_id,
                        array(generated, "sp_reg_prog_id").as_slice()
                    );
                    assert_eq!(
                        meta.sp_ps_output_cntl,
                        number(generated, "sp_ps_output_cntl")
                    );
                    assert_eq!(meta.sp_ps_output_reg, array(generated, "sp_ps_output_reg"));
                    assert_eq!(
                        meta.sp_ps_output_mask,
                        number(generated, "sp_ps_output_mask")
                    );
                    assert_eq!(
                        meta.rb_ps_output_cntl,
                        number(generated, "rb_ps_output_cntl")
                    );
                    assert_eq!(
                        meta.rb_ps_output_mask,
                        number(generated, "rb_ps_output_mask")
                    );
                }
            }
        }
        let link_names = [
            "stride16_solid",
            "stride16_texture_rgba",
            "stride16_texture_alpha_mask",
            "stride40_solid",
            "stride40_vertex_color",
            "stride40_texture_vertex_color_rgba",
            "stride24_solid",
            "stride24_texture_rgba",
            "stride24_texture_rgb_ignore_alpha",
            "stride28_vertex_color",
        ];
        for (variant, name) in PipelineVariant::ALL.into_iter().zip(link_names) {
            let generated = object(object(generated, "links"), name);
            let meta = link_meta(variant);
            assert_eq!(meta.sp_vs_output_reg, array(generated, "sp_vs_output_reg"));
            assert_eq!(
                meta.sp_vs_vpc_dest_reg,
                array(generated, "sp_vs_vpc_dest_reg")
            );
            assert_eq!(meta.vpc_vs_cntl, number(generated, "vpc_vs_cntl"));
            assert_eq!(meta.pc_vs_cntl, number(generated, "pc_vs_cntl"));
            assert_eq!(
                meta.sp_vs_output_cntl,
                number(generated, "sp_vs_output_cntl")
            );
            assert_eq!(
                meta.lm_transfer_disable,
                array(generated, "vpc_varying_lm_transfer_cntl_disable").as_slice()
            );
            assert_eq!(meta.vpc_ps_cntl, number(generated, "vpc_ps_cntl"));
        }
        assert_eq!(
            pipeline_state_meta(PipelineVariant::Stride16TextureRgba).sampler_dwords,
            Some([0x920, 0x40, 0, 0])
        );
        assert_eq!(
            pipeline_state_meta(PipelineVariant::Stride24TextureRgba).sampler_dwords,
            Some([0x92a, 0x40, 0x20, 0])
        );
        assert_eq!(PACKED_STATE_SHA256.len(), 64);
    }
}
