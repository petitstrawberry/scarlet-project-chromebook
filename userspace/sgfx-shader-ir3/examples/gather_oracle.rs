//! Emit source WGSL/SPIR-V pairs and CPU reference pixels for parent GPU tests.
//! Usage: cargo run --example gather_oracle -- OUTPUT_DIRECTORY [FIXTURE_DIRECTORY]
//! No GPU calls. Native equality is verified; execution is a separate parent test.
use sgfx_shader_ir3::{Source, Stage, compile};
use std::{error::Error, path::Path};
fn pair(out: &Path, name: &str, source: &str, stage: Stage) -> Result<(), Box<dyn Error>> {
    let module = naga::front::wgsl::parse_str(source)?;
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::PUSH_CONSTANT,
    )
    .validate(&module)?;
    let shader_stage = match stage {
        Stage::Vertex => naga::ShaderStage::Vertex,
        Stage::Fragment => naga::ShaderStage::Fragment,
        Stage::Compute => naga::ShaderStage::Compute,
    };
    let words = naga::back::spv::write_vec(
        &module,
        &info,
        &naga::back::spv::Options {
            flags: naga::back::spv::WriterFlags::empty(),
            ..Default::default()
        },
        Some(&naga::back::spv::PipelineOptions {
            shader_stage,
            entry_point: "main".into(),
        }),
    )?;
    std::fs::write(out.join(format!("{name}.wgsl")), source)?;
    std::fs::write(
        out.join(format!("{name}.source.spv")),
        words
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    )?;
    let wgsl = compile(Source::Wgsl(source), stage, "main")?;
    let spirv = compile(Source::SpirV(&words), stage, "main")?;
    assert_eq!(wgsl.allocation, spirv.allocation, "{name} ISA mismatch");
    assert_eq!(
        wgsl.sampled_bindings()?,
        spirv.sampled_bindings()?,
        "{name} interface mismatch"
    );
    for (kind, shader) in [("wgsl", wgsl), ("spirv", spirv)] {
        std::fs::write(out.join(format!("{name}.{kind}.bin")), &shader.allocation)?;
        std::fs::write(
            out.join(format!("{name}.{kind}.json")),
            serde_json::to_vec_pretty(&shader.metadata)?,
        )?;
        std::fs::write(
            out.join(format!("{name}.{kind}.bindings.json")),
            serde_json::to_vec_pretty(&shader.bindings)?,
        )?;
    }
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: gather_oracle OUTPUT_DIRECTORY")?;
    let out = Path::new(&path);
    std::fs::create_dir_all(out)?;
    pair(
        out,
        "fullscreen",
        "@vertex fn main(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32>{let x=f32((i<<1u)&2u);let y=f32(i&2u);return vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0);}",
        Stage::Vertex,
    )?;
    let mut pixels = Vec::new();
    for y in 0..4u8 {
        for x in 0..4u8 {
            pixels.extend_from_slice(&[
                13 + 11 * x + 23 * y,
                37 + 17 * x + 7 * y,
                19 + 3 * x + 29 * y,
                251 - 13 * x - 19 * y,
            ]);
        }
    }
    std::fs::write(out.join("input.rgba8"), &pixels)?;
    let depths: Vec<f32> = (0..4)
        .flat_map(|y| (0..4).map(move |x| [0.125, 0.5, 0.625, 0.875][(x + 2 * y) % 4]))
        .collect();
    std::fs::write(
        out.join("input.depth32f"),
        depths
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .collect::<Vec<_>>(),
    )?;
    let mut cases = Vec::new();
    for (name, component, offset, depth, compare) in [
        ("red", 0, [0, 0], false, None),
        ("green", 1, [0, 0], false, None),
        ("blue", 2, [0, 0], false, None),
        ("alpha", 3, [0, 0], false, None),
        ("red_offset", 0, [1, -1], false, None),
        ("depth_regular", 0, [0, 0], true, None),
        ("depth_less", 0, [0, 0], true, Some("Less")),
        ("depth_less_equal", 0, [0, 0], true, Some("LessEqual")),
    ] {
        let ty = if depth {
            "texture_depth_2d"
        } else {
            "texture_2d<f32>"
        };
        let args = if compare.is_some() {
            "t,s,vec2<f32>(0.5),0.5".to_string()
        } else if depth {
            "t,s,vec2<f32>(0.5)".to_string()
        } else {
            format!(
                "{component},t,s,vec2<f32>(0.5),vec2<i32>({},{})",
                offset[0], offset[1]
            )
        };
        let sampler = if compare.is_some() {
            "sampler_comparison"
        } else {
            "sampler"
        };
        let op = if compare.is_some() {
            "textureGatherCompare"
        } else {
            "textureGather"
        };
        let source = format!(
            "@group(0) @binding(0) var t:{ty};@group(0) @binding(1) var s:{sampler};@fragment fn main()->@location(0) vec4<f32>{{return {op}({args});}}"
        );
        pair(out, name, &source, Stage::Fragment)?;
        // WGSL gather order: (u_min,v_max),(u_max,v_max),
        // (u_max,v_min),(u_min,v_min). uv=.5 on 4x4 gives base=(1,1).
        let coordinates = [
            [1 + offset[0], 2 + offset[1]],
            [2 + offset[0], 2 + offset[1]],
            [2 + offset[0], 1 + offset[1]],
            [1 + offset[0], 1 + offset[1]],
        ];
        let expected = coordinates.map(|[x, y]| {
            let i = y as usize * 4 + x as usize;
            if !depth {
                return pixels[i * 4 + component];
            }
            match compare {
                Some("Less") => u8::from(0.5 < depths[i]) * 255,
                Some("LessEqual") => u8::from(0.5 <= depths[i]) * 255,
                None => (depths[i] * 255.0).round() as u8,
                _ => unreachable!(),
            }
        });
        std::fs::write(
            out.join(format!("{name}.expected.rgba8")),
            expected.repeat(16),
        )?;
        cases.push(serde_json::json!({"name":name,"entry":"main","input":if depth {"input.depth32f"}else{"input.rgba8"},"sampled_view_depth":depth,"sampler_compare":compare,"component":component,"offset":offset,"texel_order":coordinates,"expected_rgba8":expected,"expected_bgra8":[expected[2],expected[1],expected[0],expected[3]],"tolerance_per_channel":1,"pixels":16,"wgsl_source":format!("{name}.wgsl"),"spirv_source":format!("{name}.source.spv"),"native_code_equal":true}));
    }
    let manifest = serde_json::json!({"compiler_cache_version":sgfx_shader_ir3::COMPILER_CACHE_VERSION,"gpu_executed":false,"reference":"https://www.w3.org/TR/WGSL/#texturegather","vertex":{"wgsl":"fullscreen.wgsl","spirv":"fullscreen.source.spv","entry":"main","vertices":3},"texture":{"width":4,"height":4,"mip_levels":1,"layers":1,"rgba8_bytes_are":"SGFX logical RGBA upload order, not raw GPU BGRA allocation"},"bindings":[{"group":0,"binding":0,"kind":"sampled texture"},{"group":0,"binding":1,"kind":"ordinary sampler unless case.sampler_compare is non-null"}],"sampler":{"address":"ClampToEdge","min":"Nearest","mag":"Nearest","mip":"Nearest","lod_min":0,"lod_max":0},"render":{"width":4,"height":4,"format":"Bgra8Unorm","blend":false,"depth_test":false,"topology":"TriangleList","cull":"None"},"procedure":"Create separate WGSL and source.spv modules for each case, same pipeline/layout/resources; draw both; read back BGRA8; compare all 16 pixels to expected_bgra8 and compare the two results. Do not feed *.bin as shader source. Native artifacts use legacy compiler slot allocation, runtime uses parent canonical map.","cases":cases});
    std::fs::write(
        out.join("oracle.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    // Only portable source/input/oracle files are embedded in the diagnostic.
    // Native allocation dumps remain private validation artifacts.
    if let Some(fixtures) = std::env::args().nth(2) {
        std::fs::create_dir_all(&fixtures)?;
        for entry in std::fs::read_dir(out)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".wgsl")
                || name_str.ends_with(".source.spv")
                || name_str.ends_with(".rgba8")
                || name_str.ends_with(".depth32f")
                || name_str == "oracle.json"
            {
                std::fs::copy(entry.path(), Path::new(&fixtures).join(name))?;
            }
        }
    }
    println!(
        "9 WGSL/SPIR-V native pairs match; 8 CPU pixel oracles written; GPU execution pending"
    );
    Ok(())
}
