fn encode(v:u32)->vec4<f32>{return vec4<f32>(f32(v&255u),f32((v>>8u)&255u),f32((v>>16u)&255u),255.0)/255.0;}
@fragment fn main(@location(0) @interpolate(flat) value:u32)->@location(0) vec4<f32>{return encode(value);}
