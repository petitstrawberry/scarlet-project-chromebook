#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Cross-check packed-state.json against compiler-emitted Mesa metadata."""
import argparse
import hashlib
import json
from pathlib import Path

parser = argparse.ArgumentParser(
    description="cross-check A618 packed state against compiler-emitted Mesa metadata"
)
parser.add_argument(
    "--pack",
    type=Path,
    default=Path(__file__).resolve().parent,
    help="directory containing mesa-metadata.json and packed-state.json",
)
args = parser.parse_args()
r = args.pack.resolve()
mb = (r / "mesa-metadata.json").read_bytes()
m = json.loads(mb)
p = json.loads((r / "packed-state.json").read_text())
assert m["schema_version"] == p["schema_version"] == 2
assert p["mesa_metadata_sha256"] == hashlib.sha256(mb).hexdigest()
mv = {x["name"]: x for x in m["variants"]}
assert set(mv) == set(p["variants"])
assert {x["name"] for x in m["links"]} == set(p["links"])

for name, v in mv.items():
    s = p["variants"][name]
    if v["stage"] == "vertex":
        expected = ((v["max_half_reg"] + 1) << 1) | ((v["max_reg"] + 1) << 7)
        expected |= v["branchstack_hw"] << 13
        expected |= int(v["mergedregs"]) << 20
        expected |= int(v["early_preamble"]) << 21
        assert s["sp_vs_cntl_0"] == expected
        assert s["sp_vs_instr_size"] == v["instrlen"]
        assert len(s["vfd_dest_cntl"]) == v["attr_in"]
    else:
        expected = ((v["max_half_reg"] + 1) << 1) | ((v["max_reg"] + 1) << 7)
        expected |= v["branchstack_hw"] << 13
        expected |= int(v["threadsize"] == 128) << 20
        expected |= int(bool(v["total_in"])) << 22
        expected |= int(v["need_full_quad"]) << 23
        expected |= 1 << 24  # Turnip's deliberate inoutregoverlap policy
        expected |= int(v["need_pixlod"]) << 26
        expected |= int(v["early_preamble"]) << 28
        expected |= int(v["mergedregs"]) << 31
        assert v["double_threadsize"] == (v["threadsize"] == 128)
        assert s["sp_ps_cntl_0"] == expected
        assert s["sp_ps_instr_size"] == v["instrlen"]
        assert (s["sp_ps_initial_tex_load_cntl"] & 7) == v["num_sampler_prefetch"]
        assert ((s["sp_ps_initial_tex_load_cntl"] >> 4) & 1) == int(v["prefetch_end_of_quad"])
        assert len(s["sp_ps_initial_tex_load_cmd"]) == v["num_sampler_prefetch"]
        assert (s["rb_ps_output_cntl"] & 1) == int(v["dual_src_blend"])
        assert ((s["rb_ps_output_cntl"] >> 1) & 1) == int(v["writes_pos"])
        assert ((s["rb_ps_output_cntl"] >> 2) & 1) == int(v["writes_smask"])
        assert ((s["rb_ps_output_cntl"] >> 3) & 1) == int(v["writes_stencilref"])

for link in m["links"]:
    s = p["links"][link["name"]]
    assert s["sp_vs_output_cntl"] == len(link["vars"]) + 1
    assert len(s["vpc_varying_lm_transfer_cntl_disable"]) == 4

print("PASS: schema/hash chain and all packed-state metadata dependencies agree")
