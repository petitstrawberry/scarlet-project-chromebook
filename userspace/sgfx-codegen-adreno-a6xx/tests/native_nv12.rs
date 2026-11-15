#![cfg(feature="programmable")]
mod native_support;
use native_support::*;
use sgfx_codegen_adreno_a6xx::*;
use sgfx_core::ir::*;
fn conversion()->YcbcrConversion {YcbcrConversion {matrix:YcbcrMatrix::Bt709,range:YcbcrRange::Limited,chroma_x:ChromaLocation::Midpoint,chroma_y:ChromaLocation::Midpoint}}
#[test]
fn nv12_real_conversion_shader_reads_exact_two_planes_and_reuses_identical_tables() {
    let mut c=fixed_nv12_case(conversion());
    c.operations.insert(c.operations.len()-1,Operation::Draw {vertex_count:3,first_vertex:0});
    let a=c.compile().unwrap();
    let source:Vec<_>=a.fixups.iter().filter(|f|f.object==ObjectRef::External(SOURCE)).collect();
    assert_eq!(source.len(),2);
    assert_eq!((source[0].object_offset,source[0].required_size,source[0].access,source[0].encoding),(0,1024,Access::READ,AddressEncoding::GpuVa49TexDescriptor));
    assert_eq!((source[1].object_offset,source[1].required_size,source[1].access,source[1].encoding),(4096,512,Access::READ,AddressEncoding::GpuVa64));
    assert_eq!(source[1].word_offset-source[0].word_offset,3);
    let i=source[0].word_offset as usize;
    assert_eq!(&a.words[i-4..i],&[(0x8e<<22)|0x6880|(1<<16)|(1<<18),16|(16<<15),(64<<7)|(1<<29),1]);
    assert_eq!(a.words[i+2],64<<8);
    assert_eq!(&a.words[i+5..i+12],&[0;7]);
    assert!(a.accesses.iter().filter(|r|r.object==ObjectRef::External(SOURCE)).all(|r|r.access==Access::READ));
    // Move only UV, leaving dimensions/Y/words identical: descriptor key must
    // include its second relocation authority rather than reusing old UV.
    let mut changed=c.resources.iter().find(|r|r.id==SOURCE).unwrap().clone();
    changed.id=ObjectId::new(77);
    if let ResourceKind::Image(i)=&mut changed.kind {i.planes[1].offset=8192;}
    changed.size=12288;c.resources.push(changed);
    c.operations.insert(c.operations.len()-1,Operation::SetTexture(ObjectId::new(77)));
    c.operations.insert(c.operations.len()-1,Operation::Draw {vertex_count:3,first_vertex:0});
    let b=c.compile().unwrap();
    assert!(b.fixups.iter().any(|f|f.object==ObjectRef::External(ObjectId::new(77))&&f.object_offset==8192&&f.required_size==512&&f.access==Access::READ));
}
#[test]
fn nv12_invalid_plane_metadata_and_ordinary_unconverted_binding_stay_rejected() {
    for bad in 0..6 {
        let mut c=fixed_nv12_case(conversion());
        let r=c.resources.iter_mut().find(|r|r.id==SOURCE).unwrap();
        if let ResourceKind::Image(i)=&mut r.kind {match bad {
            0=>i.planes[1].offset=512,
            1=>i.planes[1].size=511,
            2=>i.planes[1].stride=128,
            3=>i.conversion=None,
            4=>i.usage=TextureUsage::SAMPLED|TextureUsage::COPY_DST,
            _=>i.planes[0].offset=64,
        }}
        assert!(c.compile().is_err(),"bad NV12 layout={bad}");
    }
    let mut c=fixed_nv12_case(conversion());
    // The ordinary programmable binding lacks a conversion lowering and must
    // remain rejected even though its SGFX texture declaration is texture_2d.
    for o in &mut c.operations {if let Operation::SetFixedNativePipeline(p)=o {*o=Operation::SetProgrammablePipeline(p.clone());}}
    c.operations.retain(|o|!matches!(o,Operation::SetTexture(_)|Operation::SetSampler(_)));
    c.operations.insert(c.operations.len()-2,Operation::SetBindGroup {index:0,bindings:vec![
        NativeBinding {binding:0,resource:NativeBindingResource::Texture {object:SOURCE,view:None}},
        NativeBinding {binding:1,resource:NativeBindingResource::Sampler(sampler())}]});
    assert!(c.compile().is_err());
}
#[test]
fn generic_shader_actual_nir_conversion_requires_the_exact_bound_import_profile() {
    use std::sync::Arc;
    let c=generic_nv12_case();assert!(c.compile().is_ok());
    let a=c.compile().unwrap();assert_eq!(a.fixups.iter().filter(|f|f.object==ObjectRef::External(SOURCE)).count(),2);
    for change in 0..7 {
        let mut bad=generic_nv12_case();
        match change {
            0..=3=>{let r=bad.resources.iter_mut().find(|r|r.id==SOURCE).unwrap();let ResourceKind::Image(i)=&mut r.kind else {unreachable!()};let mut p=i.conversion.unwrap();match change {0=>p.matrix=YcbcrMatrix::Bt601,1=>p.range=YcbcrRange::Full,2=>p.chroma_x=ChromaLocation::Cosited,_=>p.chroma_y=ChromaLocation::Cosited};i.conversion=Some(p);}
            4=>{for o in &mut bad.operations {if let Operation::SetProgrammablePipeline(p)=o {Arc::make_mut(&mut Arc::make_mut(p).fragment.artifact).metadata.extra.remove("sgfx_nv12_profile");}}}
            5=>{for o in &mut bad.operations {if let Operation::SetBindGroup {bindings,..}=o {bindings[0].resource=NativeBindingResource::Texture {object:SOURCE,view:None};}}}
            _=>{for o in &mut bad.operations {if let Operation::SetBindGroup {bindings,..}=o {bindings[1].resource=NativeBindingResource::Sampler(SamplerDesc::new(FilterMode::Nearest,FilterMode::Nearest,AddressMode::ClampToBorder,AddressMode::ClampToEdge));}}}
        }
        assert!(bad.compile().is_err(),"profile change={change}");
    }
}
