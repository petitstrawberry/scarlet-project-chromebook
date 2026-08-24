#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Pack A618 shader-state dwords from Mesa-produced IR3 metadata.

Field locations are from pinned Mesa src/freedreno/registers/adreno/a6xx.xml
at 3f1b217baffffa00cb8f53e158713a33e1bd4632.  State selection follows
tu6_emit_xs(), tu6_emit_vfd_dest(), tu6_emit_fs_inputs(), tu6_emit_fs_outputs(),
and tu6_emit_vpc() from the same revision.
"""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(
    description="pack A618 shader-state dwords from Mesa-produced IR3 metadata"
)
parser.add_argument(
    "--pack",
    type=Path,
    default=Path(__file__).resolve().parent,
    help="directory containing mesa-metadata.json (default: this script's directory)",
)
args = parser.parse_args()
ROOT = args.pack.resolve()
d = json.loads((ROOT / "mesa-metadata.json").read_text())
variants = {v["name"]: v for v in d["variants"]}
INVALID = 0xfc  # regid(63, 0)

def xs_state(v):
    full = v["max_reg"] + 1
    half = v["max_half_reg"] + 1
    branch = v["branchstack_hw"]
    merged = int(v["mergedregs"]) << (20 if v["stage"] == "vertex" else 31)
    if v["stage"] == "vertex":
        cntl0 = ((half << 1) | (full << 7) | (branch << 13) | merged |
                 (int(v["early_preamble"]) << 21))
        instr = v["instrlen"]
        vfd = [0] * v["attr_in"]
        for x in v["inputs"]:
            if not x["sysval"]:
                loc = x["slot"] - 15  # VERT_ATTRIB_GENERIC0
                vfd[loc] = x["compmask"] | (x["regid"] << 4)
        return {"sp_vs_cntl_0": cntl0, "sp_vs_instr_size": instr,
                "vfd_dest_cntl": vfd}

    cntl0 = ((half << 1) | (full << 7) | (branch << 13) |
             ((1 if v["threadsize"] == 128 else 0) << 20) |
             ((1 if v["total_in"] else 0) << 22) |
             (int(v["need_full_quad"]) << 23) | (1 << 24) |
             (int(v["need_pixlod"]) << 26) |
             (int(v["early_preamble"]) << 28) | merged)
    ij = next((x["regid"] for x in v["inputs"] if x["sysval"]), INVALID)
    prog0 = INVALID | (INVALID << 8) | (INVALID << 16) | (INVALID << 24)
    prog1 = ij | (INVALID << 8) | (INVALID << 16) | (INVALID << 24)
    prog2 = INVALID | (INVALID << 8) | (INVALID << 16) | (INVALID << 24)
    prog3 = INVALID | (INVALID << 8)
    initial = (v["num_sampler_prefetch"] | ((1 if ij == INVALID else 0) << 3) |
               (int(v["prefetch_end_of_quad"]) << 4))
    cmds = []
    for p in v["sampler_prefetch"]:
        cmds.append(p["src"] | (p["samp_id"] << 7) | (p["tex_id"] << 11) |
                    (p["dst"] << 16) | (p["wrmask"] << 22) |
                    (p["half"] << 26) | (int(p["bindless"]) << 28) |
                    (4 << 29))  # TEX_PREFETCH_SAM
    assert not v["color0_mrt"]
    color_outputs = [x for x in v["outputs"] if x["slot"] == 4]
    assert len(color_outputs) == 1 and color_outputs[0]["aliased_components"] == 0
    out = color_outputs[0]["regid"]
    sp_output_cntl = (int(v["dual_src_blend"]) |
                      (INVALID << 8) | (INVALID << 16) | (INVALID << 24))
    rb_output_cntl = (int(v["dual_src_blend"]) |
                      (int(v["writes_pos"]) << 1) |
                      (int(v["writes_smask"]) << 2) |
                      (int(v["writes_stencilref"]) << 3))
    return {"sp_ps_cntl_0": cntl0, "sp_ps_instr_size": v["instrlen"],
            "sp_ps_initial_tex_load_cntl": initial,
            "sp_ps_initial_tex_load_cmd": cmds,
            "sp_reg_prog_id": [prog0, prog1, prog2, prog3],
            "sp_ps_output_cntl": sp_output_cntl,
            "sp_ps_output_reg": [out], "sp_ps_output_mask": 0xf,
            "rb_ps_output_cntl": rb_output_cntl, "rb_ps_output_mask": 0xf}

def link_state(link):
    vs = variants[link["vs"]]
    assert not vs["multi_pos_output"]
    assert not vs["writes_psize"] and not vs["writes_viewport"]
    assert vs["clip_mask"] == 0 and vs["cull_mask"] == 0
    entries = list(link["vars"])
    pos = next(x for x in vs["outputs"] if x["slot"] == 0)
    assert pos["view"] == 0 and pos["aliased_components"] == 0
    position_loc = link["max_loc"]
    entries.append({"slot": 0, "regid": pos["regid"], "compmask": 0xf,
                    "loc": position_loc})
    max_loc = position_loc + 4
    sp_out = []
    for i in range(0, len(entries), 2):
        a = entries[i]["regid"] | (entries[i]["compmask"] << 8)
        b = 0
        if i + 1 < len(entries):
            b = (entries[i+1]["regid"] | (entries[i+1]["compmask"] << 8)) << 16
        sp_out.append(a | b)
    sp_dst = []
    for i in range(0, len(entries), 4):
        word = 0
        for j, e in enumerate(entries[i:i+4]): word |= e["loc"] << (8*j)
        sp_dst.append(word)
    varmask = link["varmask"]
    fs = variants[link["fs"]]
    return {"sp_vs_output_cntl": len(entries), "sp_vs_output_reg": sp_out,
            "sp_vs_vpc_dest_reg": sp_dst,
            "vpc_vs_cntl": max_loc | (position_loc << 8) | (0xff << 16),
            "pc_vs_cntl": max_loc,
            "vpc_varying_lm_transfer_cntl_disable": [(~x) & 0xffffffff for x in varmask],
            "vpc_ps_cntl": fs["total_in"] | (0xff << 8) |
                           ((1 if fs["total_in"] else 0) << 16) | (0xff << 24)}

out = {
  "schema_version": d["schema_version"],
  "mesa_metadata_sha256": __import__("hashlib").sha256(
      (ROOT / "mesa-metadata.json").read_bytes()).hexdigest(),
  "mesa_sha": d["mesa_sha"], "gpu_id": 618,
  "provenance": {
    "xml": "src/freedreno/registers/adreno/a6xx.xml",
    "driver": ["src/freedreno/vulkan/tu_shader.cc", "src/freedreno/vulkan/tu_pipeline.cc"],
    "note": "Values are register payload dwords, not register addresses or packet headers."
  },
  "variants": {name: xs_state(v) for name, v in variants.items()},
  "links": {x["name"]: link_state(x) for x in d["links"]},
}
(ROOT / "packed-state.json").write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
