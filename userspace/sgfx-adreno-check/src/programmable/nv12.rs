// SPDX-License-Identifier: GPL-2.0-only
//! Public decoder -> immutable shared image -> YCbCr Session API -> real draws.
//! CPU colors check tile interiors; all pixels also compare to canonical NV12.
use super::*;
const SIDE:u32=16;
const SOURCE_SIDE:usize=128;
const REFERENCE:&[u8]=include_bytes!("nv12-fixtures/reference.nv12");
pub(super) fn conversions()->Vec<YcbcrConversion> {
    let mut result=Vec::new();
    for matrix in [YcbcrMatrix::Bt601,YcbcrMatrix::Bt709] {for range in [YcbcrRange::Limited,YcbcrRange::Full] {
        for chroma_x in [ChromaLocation::Cosited,ChromaLocation::Midpoint] {for chroma_y in [ChromaLocation::Cosited,ChromaLocation::Midpoint] {
            result.push(YcbcrConversion {matrix,range,chroma_x,chroma_y});
        }}
    }}result
}
pub(super) fn cpu_rgb(c:YcbcrConversion,x:usize,y:usize)->[f32;4] {
    let luma=REFERENCE[y*SOURCE_SIDE+x] as f32;
    let uv=SOURCE_SIDE*SOURCE_SIDE+(y/2)*SOURCE_SIDE+(x/2)*2;
    let cb=REFERENCE[uv] as f32-128.0;
    let cr=REFERENCE[uv+1] as f32-128.0;
    let (kr,kb)=match c.matrix {YcbcrMatrix::Bt601=>(0.299f32,0.114f32),YcbcrMatrix::Bt709=>(0.2126,0.0722)};
    let kg=1.0-kr-kb;
    let (luma,cb,cr)=match c.range {
        YcbcrRange::Limited=>((luma-16.0)/219.0,cb/224.0,cr/224.0),
        YcbcrRange::Full=>(luma/255.0,cb/255.0,cr/255.0),
    };
    [(luma+2.0*(1.0-kr)*cr).clamp(0.0,1.0),
     (luma-2.0*kb*(1.0-kb)/kg*cb-2.0*kr*(1.0-kr)/kg*cr).clamp(0.0,1.0),
     (luma+2.0*(1.0-kb)*cb).clamp(0.0,1.0),1.0]
}
pub(super) fn vertex_data(native:bool)->Vec<u8> {
    let stride=if native {40} else {24};
    let mut bytes=vec![0xa5;6*stride];
    for (i,[x,y]) in [[-1.0f32,-1.0],[1.0,-1.0],[-1.0,1.0],[-1.0,1.0],[1.0,-1.0],[1.0,1.0]].into_iter().enumerate() {
        let position=[x,y,0.0,1.0];
        let off=i*stride+if native {8} else {0};
        for (j,v) in position[..if native {3}else{4}].iter().enumerate() {bytes[off+4*j..off+4*j+4].copy_from_slice(&v.to_le_bytes());}
        let off=i*stride+if native {24} else {16};
        // Four-source-pixel offset deliberately samples both sides of tile
        // boundaries; linear/nearest and chroma siting affect full output.
        for (j,v) in [x*0.5+0.5+0.03125,y*0.5+0.5+0.03125].into_iter().enumerate() {bytes[off+4*j..off+4*j+4].copy_from_slice(&v.to_le_bytes());}
    }bytes
}
struct Scene {source:TextureId,targets:[TextureId;2],pipelines:[RenderPipelineId;2],vertices:[BufferId;2],samplers:[SamplerId;2]}
fn scene(table:&ResourceTable)->Result<Scene,String> {
    table.enable_abi_commands();
    let source=checked!(table.define_texture(checked!(TextureDesc::new(TextureFormat::Nv12,checked!(Extent2D::new(128,128)),TextureUsage::SAMPLED)))).id();
    let mut targets=Vec::new();let mut pipelines=Vec::new();let mut vertices=Vec::new();
    for native in [false,true] {
        targets.push(checked!(table.define_texture(checked!(TextureDesc::new(TextureFormat::Bgra8Unorm,checked!(Extent2D::new(SIDE,SIDE)),TextureUsage::PRESENT|TextureUsage::RENDER_ATTACHMENT|TextureUsage::COPY_SRC)))).id());
        let attrs=if native {vec![VertexAttribute::new(0,VertexFormat::Float32x3,8),VertexAttribute::new(1,VertexFormat::Float32x2,24)]}
            else {vec![VertexAttribute::new(0,VertexFormat::Float32x4,0),VertexAttribute::new(1,VertexFormat::Float32x2,16)]};
        pipelines.push(checked!(table.define_render_pipeline(checked!(RenderPipelineDesc::new(TextureFormat::Bgra8Unorm,PrimitiveTopology::TriangleList,
            checked!(VertexBufferLayout::new(if native {40}else{24},attrs)),FragmentProgram::Texture(TextureSampleMode::Rgba),BlendState::REPLACE,
            RasterState::new(CullMode::None,FrontFace::CounterClockwise))))).id());
        vertices.push(checked!(table.define_buffer(checked!(BufferDesc::new(if native {240}else{144},BufferUsage::VERTEX|BufferUsage::COPY_DST)))).id());
    }
    let samplers=[FilterMode::Nearest,FilterMode::Linear].map(|f|table.define_sampler(SamplerDesc::new(f,f,AddressMode::ClampToEdge,AddressMode::ClampToEdge)).map(|r|r.id()));
    Ok(Scene {source,targets:targets.try_into().unwrap(),pipelines:pipelines.try_into().unwrap(),vertices:vertices.try_into().unwrap(),samplers:[checked!(samplers[0]),checked!(samplers[1])]})
}
fn record<'r,'d>(table:&'r ResourceTable,s:&Scene,filter:usize,data:&'d [Vec<u8>;2])->Result<CommandBuffer<'r,'d>,String> {
    let mut encoder=CommandEncoder::new(table);
    for n in 0..2 {
        let vertices=checked!(table.buffer_ref(s.vertices[n]));
        checked!(encoder.write_buffer(vertices,0,&data[n]));
        let target=checked!(table.texture_ref(s.targets[n]));
        let mut pass=checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(table,target,checked!(PixelRect::new(0,0,SIDE,SIDE)),LoadOp::Clear(checked!(Color::rgba(0.0,0.0,0.0,1.0))),StoreOp::Store))));
        checked!(pass.set_pipeline(checked!(table.render_pipeline_ref(s.pipelines[n]))));
        checked!(pass.set_vertex_buffer(vertices,0));
        checked!(pass.set_uniforms(DrawUniforms::new(Transform::identity(),checked!(Color::rgba(1.0,1.0,1.0,1.0)))));
        checked!(pass.set_texture(checked!(table.texture_ref(s.source))));
        checked!(pass.set_sampler(checked!(table.sampler_ref(s.samplers[filter]))));
        checked!(pass.draw(6,0));checked!(pass.end());
    }
    Ok(checked!(encoder.finish()))
}
#[cfg(target_os="scarlet")]
pub fn run(path:&str)->Result<(),String> {
    use scarlet_video_client::{DecoderOptions,ScarletVideoDecoder,DecodedOutput,VideoFormat};
    println!("BEGIN nv12 public-producer=video shared-image=true");
    let mut decoder=ScarletVideoDecoder::open_with_options(DecoderOptions::new().with_shared_images(true))
        .map_err(|e|format!("NV12 producer prerequisite: {e}"))?;
    let frame=decoder.decode_output(VideoFormat::H264,include_bytes!("nv12-fixtures/frame.h264"),1)
        .map_err(|e|format!("NV12 producer prerequisite decode: {e}"))?.ok_or("NV12 producer returned no frame")?;
    let image=match frame {DecodedOutput::Image(i)=>i,DecodedOutput::Nv12(_)=>return Err("NV12 producer lacks shared-image capability".into())};
    let descriptor=image.descriptor();
    if descriptor.width!=128||descriptor.height!=128||descriptor.format!=u32::from_le_bytes(*b"NV12") {return Err(format!("unexpected producer image {descriptor:?}"));}
    let handle=image.into_handle();drop(decoder);
    let table=ResourceTable::new();let s=scene(&table)?;let data=[vertex_data(false),vertex_data(true)];
    let commands=[record(&table,&s,0,&data)?,record(&table,&s,1,&data)?];
    let target_slots=s.targets.map(|id|table.texture_ref(id).unwrap().slot() as u32);
    let mut session=crate::abi_session::Session::new(path,&table,&target_slots)?;
    // Rebind all16 conversions in one stable resource/session table, twice.
    // This catches both incorrect shader variants and stale imported storage.
    for sweep in 0..2 {for (index,c) in conversions().into_iter().enumerate() {
        let owned=handle.duplicate().map_err(|e|format!("producer duplicate: {e:?}"))?;
        let conversion=abi::YcbcrConversion {size:core::mem::size_of::<abi::YcbcrConversion>() as u32,reserved:0,
            matrix:match c.matrix {YcbcrMatrix::Bt601=>abi::YCBCR_MATRIX_BT601,YcbcrMatrix::Bt709=>abi::YCBCR_MATRIX_BT709},
            range:match c.range {YcbcrRange::Limited=>abi::YCBCR_RANGE_LIMITED,YcbcrRange::Full=>abi::YCBCR_RANGE_FULL},
            chroma_x:match c.chroma_x {ChromaLocation::Cosited=>abi::YCBCR_CHROMA_COSITED,ChromaLocation::Midpoint=>abi::YCBCR_CHROMA_MIDPOINT},
            chroma_y:match c.chroma_y {ChromaLocation::Cosited=>abi::YCBCR_CHROMA_COSITED,ChromaLocation::Midpoint=>abi::YCBCR_CHROMA_MIDPOINT}};
        session.import_ycbcr(table.texture_ref(s.source).unwrap().slot() as u32,conversion,||{let raw=owned.as_raw();core::mem::forget(owned);raw})?;
        for filter in 0..2 {
            session.submit(&commands[filter])?;
            let mut pixels=[vec![0xa5;(SIDE*SIDE*4) as usize],vec![0xa5;(SIDE*SIDE*4) as usize]];
            for n in 0..2 {session.readback(target_slots[n],&mut pixels[n],SIDE*4,checked!(PixelRect::new(0,0,SIDE,SIDE)))?;}
            check_pixels(c,&pixels)?;
            println!("PASS nv12 conversion={index} sweep={sweep} filter={filter} canonical+native pixels=512 cpu-interiors=32 readonly-public-import=true producer-dropped=true");
        }
        // Both Session::submit calls above waited for COMPLETE. Re-import is
        // deliberately explicit: release the previous imported mapping via
        // the public ABI before binding another conversion to this stable ID.
        // The original producer handle still leases the immutable backing.
        session.release_import(table.texture_ref(s.source).unwrap().slot() as u32)?;
    }}
    println!("PASS nv12 ALL conversions=16 sweeps=2 submits=64 pixels=32768");Ok(())
}
fn check_pixels(c:YcbcrConversion,pixels:&[Vec<u8>;2])->Result<(),String> {
    for (index,(&a,&b)) in pixels[0].iter().zip(&pixels[1]).enumerate() {
        if a.abs_diff(b)>2 {return Err(format!("NV12 native/canonical byte={index} canonical={a} native={b}"));}
    }
    // Coordinates deliberately away from any Y/UV tile boundary. This CPU
    // comparison is independent of the canonical GPU shader and descriptor.
    for y in [1usize,5,9,13] {for x in [1usize,5,9,13] {
        let sx=(8*x+8).min(127);let sy=(128-(8*y+4)+4).min(127);
        let rgb=cpu_rgb(c,sx,sy);let want=[rgb[2],rgb[1],rgb[0],rgb[3]].map(|v|(v*255.0).round() as u8);
        let off=(y*SIDE as usize+x)*4;
        for image in pixels {for k in 0..4 {if image[off+k].abs_diff(want[k])>2 {return Err(format!("NV12 CPU pixel={x},{y} channel={k} got={} want={} source={sx},{sy}",image[off+k],want[k]));}}}
    }}Ok(())
}
#[cfg(not(target_os="scarlet"))]
pub fn run(_path:&str)->Result<(),String> {Err("NV12 GPU oracle requires Scarlet public video producer".into())}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_nv12_scene_encodes_both_real_fixed_paths_and_both_filters() {
        let table=ResourceTable::new();let s=scene(&table).unwrap();let data=[vertex_data(false),vertex_data(true)];
        // Session::new uses BackendApi::create_session, whose mapped target
        // contract requires PRESENT as well as RENDER_ATTACHMENT. Encoding
        // alone also accepts offscreen targets and missed this device failure.
        for id in s.targets {
            let d=table.texture(table.texture_ref(id).unwrap()).unwrap();
            assert_eq!(d.format(),TextureFormat::Bgra8Unorm);
            assert!(d.usage().contains(TextureUsage::PRESENT|TextureUsage::RENDER_ATTACHMENT));
        }

        for filter in 0..2 {let c=record(&table,&s,filter,&data).unwrap();assert!(c.abi_batch().unwrap().count>0);}
        assert_eq!(conversions().len(),16);assert_eq!(REFERENCE.len(),128*128*3/2);
        // The software decoder fixture has constant tile interiors, including
        // the neighboring chroma samples used by linear filtering.
        for y in [16usize,48,80,112] {for x in [16usize,48,80,112] {
            for dy in -2isize..=2 {for dx in -2isize..=2 {
                let nx=x.wrapping_add_signed(dx);let ny=y.wrapping_add_signed(dy);
                assert_eq!(REFERENCE[y*128+x],REFERENCE[ny*128+nx]);
                let a=128*128+(y/2)*128+(x/2)*2;let b=128*128+(ny/2)*128+(nx/2)*2;
                assert_eq!(&REFERENCE[a..a+2],&REFERENCE[b..b+2]);
            }}
        }}
    }
}
