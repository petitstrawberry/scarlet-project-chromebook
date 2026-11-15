// SPDX-License-Identifier: GPL-2.0-only
//! Mesa 3f1b217b IR3 metadata, ABI 1. Offsets explicitly carry their units.
use alloc::{collections::BTreeMap, string::String, vec::Vec};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ConstantAllocation {
    /// Mesa ir3_const_alloc_type: push=0, driver params=3, UBO ranges=4, etc.
    pub kind: u32,
    pub offset_vec4: u32,
    pub size_vec4: u32,
    pub reserved_size_vec4: u32,
    pub reserved_align_vec4: u32,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct DriverUbo {
    pub idx: i32,
    pub size: u32,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct UboRange {
    pub block: u32,
    pub bindless_base: u32,
    pub bindless: bool,
    pub global: bool,
    /// Destination in constant register file, bytes. Source interval [start,end), bytes.
    pub offset: u32,
    pub start: u32,
    pub end: u32,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Input {
    /// Mesa gl_vert_attrib / gl_varying_slot, or gl_system_value when sysval=true.
    pub slot: u32,
    pub regid: u32,
    pub compmask: u32,
    pub inloc: u32,
    pub sysval: bool,
    pub bary: bool,
    pub half: bool,
    pub flat: bool,
    pub rasterflat: bool,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Output {
    pub slot: u32,
    pub regid: u32,
    pub view: u32,
    pub aliased_components: u32,
    pub half: bool,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ImageMapping {
    pub num_tex: u32,
    pub tex_base: u32,
    pub ssbo_to_tex: Vec<u8>,
    pub image_to_tex: Vec<u8>,
    /// 0x80 | ssbo_slot denotes an SSBO; 0xff means unused.
    pub tex_to_image: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Compute {
    pub req_local_mem: u32,
    pub force_linear_dispatch: bool,
    pub round_robin_mode: bool,
    pub local_invocation_id: u32,
    pub work_group_id: u32,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct SamplerPrefetch {
    pub src: u32,
    pub samp_id: u32,
    pub tex_id: u32,
    pub dst: u32,
    pub wrmask: u32,
    pub half: u32,
    pub bindless: bool,
    pub tex_opc: u32,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Metadata {
    pub abi_version: u32,
    pub gpu_id: u32,
    pub stage: String,
    /// Entire allocation size, including executable padding AND embedded rodata.
    pub allocation_bytes: u32,
    /// Executable size in dwords; instrlen is the hardware instruction group count.
    pub sizedwords: u32,
    pub instrlen: u32,
    pub constant_data_offset: u32,
    pub constant_data_bytes: u32,
    pub constlen: u32,
    pub const_alloc_max_vec4: u32,
    pub immediate_offset_dwords: u32,
    pub imm_values: Vec<u32>,
    pub const_allocations: Vec<ConstantAllocation>,
    pub reserved_vec4: u32,
    pub num_ubos: u32,
    pub num_app_ubos: u32,
    pub num_driver_params: u32,
    /// consts, driver_params, primitive_map, primitive_param (in that order).
    pub driver_ubos: Vec<DriverUbo>,
    pub ubo_ranges: Vec<UboRange>,
    pub ubo_range_size: u32,
    pub push_consts_type: u32,
    pub max_reg: i32,
    pub max_half_reg: i32,
    pub halfregs_footprint: u32,
    pub max_waves: i32,
    pub subgroup_size: u32,
    pub branchstack: u32,
    pub branchstack_hw: u32,
    pub mergedregs: bool,
    pub early_preamble: bool,
    pub double_threadsize: bool,
    pub threadsize: u32,
    pub pvtmem_size: u32,
    pub pvtmem_per_wave: bool,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub total_in: u32,
    pub sysval_in: u32,
    pub varying_in: u32,
    pub attr_in: u32,
    pub output_size: u32,
    pub vtxid_base: u32,
    pub writes_pos: bool,
    pub writes_smask: bool,
    pub writes_psize: bool,
    pub writes_viewport: bool,
    pub writes_stencilref: bool,
    pub need_full_quad: bool,
    pub need_pixlod: bool,
    pub need_driver_params: bool,
    pub no_earlyz: bool,
    pub has_kill: bool,
    pub sample_shading: bool,
    /// Explicit rasterization profile supplied to the native compiler: 1 for
    /// FS and 0 for VS/CS. None identifies legacy artifacts without this tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rasterization_samples: Option<u32>,
    pub fragcoord_compmask: u32,
    pub frag_face: bool,
    pub reads_smask: bool,
    /// FS primitive ID after nir_lower_sysvals_to_varyings: a flat non-sysval
    /// VARYING_SLOT_PRIMITIVE_ID (21) input with its actual VPC inloc. A raw
    /// SYSTEM_VALUE_PRIMITIVE_ID (37) register is not this interface on A618.
    pub reads_primid: bool,
    pub num_samp: i32,
    pub num_sampler_prefetch: u32,
    pub sampler_prefetch: Vec<SamplerPrefetch>,
    pub prefetch_bary_type: u32,
    pub prefetch_end_of_quad: bool,
    pub num_ssbos: u32,
    pub num_uavs: u32,
    pub has_ssbo: bool,
    pub image_mapping: ImageMapping,
    /// Physical shared allocation in bytes per workgroup (not a register encoding).
    pub shared_size: u32,
    pub local_size: [u32; 3],
    pub local_size_variable: bool,
    pub has_barrier: bool,
    pub compute: Option<Compute>,
    /// Additional versioned Mesa state, kept losslessly rather than discarded.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}
