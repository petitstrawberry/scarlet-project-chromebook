// Logical-texel reconstruction oracle; conversion belongs to the backend.
@group(0) @binding(0) var image:texture_2d<f32>;
@group(0) @binding(1) var gather_a:sampler;
@group(0) @binding(2) var gather_b:sampler;
@group(0) @binding(3) var nearest_a:sampler;
@group(0) @binding(4) var nearest_b:sampler;
@vertex fn vs(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32> {
    let x=f32((i<<1u)&2u);let y=f32(i&2u);
    return vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0);
}
fn axis_x(i:u32)->i32 { switch i {
case 0u: {return -129;}
case 1u: {return -2;}
case 2u: {return -1;}
case 3u: {return 0;}
case 4u: {return 1;}
case 5u: {return 2;}
case 6u: {return 3;}
case 7u: {return 31;}
case 8u: {return 32;}
case 9u: {return 63;}
case 10u: {return 64;}
case 11u: {return 125;}
case 12u: {return 126;}
case 13u: {return 127;}
case 14u: {return 128;}
default: {return 257;}
} }
fn axis_y(i:u32)->i32 { switch i {
case 0u: {return -129;}
case 1u: {return -1;}
case 2u: {return 0;}
case 3u: {return 1;}
case 4u: {return 63;}
case 5u: {return 126;}
case 6u: {return 127;}
default: {return 128;}
} }

@fragment fn fs(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32> {
    let column=u32(p.x);
    let q=vec2<i32>(axis_x(column&15u),axis_y(column>>4u));
    let dimensions=vec2<f32>(textureDimensions(image));
    // .75 preserves each nearest logical texel and its specified cosited
    // chroma choice, while staying away from exact nearest-sampler ties.
    let uv=(vec2<f32>(q)+vec2<f32>(0.75))/dimensions;
    switch u32(p.y) {
    case 0u: {return textureLoad(image,q,0);}
    case 1u: {return textureLoad(image,q,1);}
    case 2u: {return textureLoad(image,q,-1);}
    case 3u: {return textureSampleLevel(image,nearest_a,uv,0.0);}
    case 4u: {return textureSampleLevel(image,nearest_b,uv,0.0);}
    case 5u: {return textureGather(0,image,gather_a,uv);}
    case 6u: {return textureGather(1,image,gather_a,uv);}
    case 7u: {return textureGather(2,image,gather_a,uv);}
    case 8u: {return textureGather(3,image,gather_a,uv);}
    case 9u: {return textureGather(0,image,gather_b,uv,vec2<i32>(1,-1));}
    case 10u: {return textureGather(1,image,gather_b,uv,vec2<i32>(1,-1));}
    case 11u: {return textureGather(2,image,gather_b,uv,vec2<i32>(1,-1));}
    case 12u: {return textureGather(3,image,gather_b,uv,vec2<i32>(1,-1));}
    case 13u: {return textureGather(0,image,gather_a,uv,vec2<i32>(-2,1));}
    case 14u: {return textureGather(1,image,gather_a,uv,vec2<i32>(-2,1));}
    case 15u: {return textureGather(2,image,gather_a,uv,vec2<i32>(-2,1));}
    case 16u: {return textureGather(3,image,gather_a,uv,vec2<i32>(-2,1));}
    case 17u: {return textureGather(0,image,gather_b,uv,vec2<i32>(-8,7));}
    case 18u: {return textureGather(1,image,gather_b,uv,vec2<i32>(-8,7));}
    case 19u: {return textureGather(2,image,gather_b,uv,vec2<i32>(-8,7));}
    case 20u: {return textureGather(3,image,gather_b,uv,vec2<i32>(-8,7));}
    case 21u: {return textureLoad(image,q+vec2<i32>(1,-1),0);}
    case 22u: {return textureLoad(image,q+vec2<i32>(-1,1),0);}
    default: {return vec4<f32>(dimensions/128.0,f32(textureNumLevels(image)),1.0);}
    }
}

@fragment fn fs_border(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32> {
    let column=u32(p.x);
    let q=vec2<i32>(axis_x(column&15u),axis_y(column>>4u));
    let dimensions=vec2<f32>(textureDimensions(image));
    let uv=(vec2<f32>(q)+vec2<f32>(0.75))/dimensions;
    switch u32(p.y)%6u {
    case 0u: {return textureSampleLevel(image,gather_a,uv,0.0);}
    case 1u: {return textureSampleLevel(image,gather_a,uv,1.0);}
    case 2u: {return textureSampleBias(image,gather_a,uv,0.0);}
    case 3u: {return textureSampleGrad(image,gather_a,uv,vec2<f32>(1.0/128.0,0.0),vec2<f32>(0.0,1.0/128.0));}
    case 4u: {return textureSampleLevel(image,gather_a,uv,0.0,vec2<i32>(1,-1));}
    default: {return textureSample(image,gather_a,uv);}
    }
}
@group(0) @binding(5) var slot_4:sampler;
@group(0) @binding(6) var slot_5:sampler;
@group(0) @binding(7) var slot_6:sampler;
@group(0) @binding(8) var slot_7:sampler;
@group(0) @binding(9) var slot_8:sampler;
@group(0) @binding(10) var slot_9:sampler;
@group(0) @binding(11) var slot_10:sampler;
@group(0) @binding(12) var slot_11:sampler;
@group(0) @binding(13) var slot_12:sampler;
@group(0) @binding(14) var slot_13:sampler;
@group(0) @binding(15) var slot_14:sampler;
@group(0) @binding(16) var slot_15:sampler;
@fragment fn fs_slots(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32> {
    let uv=vec2<f32>(0.75)/vec2<f32>(textureDimensions(image));
    var color=textureLoad(image,vec2<i32>(0),0);
    color+=textureSampleLevel(image,gather_a,uv,0.0);
    color+=textureSampleLevel(image,gather_b,uv,0.0);
    color+=textureSampleLevel(image,nearest_a,uv,0.0);
    color+=textureSampleLevel(image,nearest_b,uv,0.0);
    color+=textureSampleLevel(image,slot_4,uv,0.0);
    color+=textureSampleLevel(image,slot_5,uv,0.0);
    color+=textureSampleLevel(image,slot_6,uv,0.0);
    color+=textureSampleLevel(image,slot_7,uv,0.0);
    color+=textureSampleLevel(image,slot_8,uv,0.0);
    color+=textureSampleLevel(image,slot_9,uv,0.0);
    color+=textureSampleLevel(image,slot_10,uv,0.0);
    color+=textureSampleLevel(image,slot_11,uv,0.0);
    color+=textureSampleLevel(image,slot_12,uv,0.0);
    color+=textureSampleLevel(image,slot_13,uv,0.0);
    color+=textureSampleLevel(image,slot_14,uv,0.0);
    color+=textureSampleLevel(image,slot_15,uv,0.0);
    return color/17.0;
}
