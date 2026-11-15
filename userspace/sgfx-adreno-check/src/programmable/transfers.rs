// SPDX-License-Identifier: GPL-2.0-only
//! Small installed-driver transfer checks with independent CPU readback oracles.
use super::*;

fn rect(x: u32, y: u32, width: u32, height: u32) -> Result<PixelRect, String> {
    Ok(checked!(PixelRect::new(x, y, width, height)))
}

fn texture<'a>(
    table: &'a ResourceTable,
    format: TextureFormat,
    width: u32,
    height: u32,
) -> Result<TextureRef<'a>, String> {
    Ok(checked!(table.define_texture(checked!(TextureDesc::new(
        format,
        checked!(Extent2D::new(width, height)),
        TextureUsage::COPY_SRC | TextureUsage::COPY_DST | TextureUsage::RENDER_ATTACHMENT
    )))))
}

// Deliberately provide padding between rows, but none after the final row.
fn padded_rows(tight: &[u8], row_bytes: usize, height: usize) -> Vec<u8> {
    let mut result = vec![0xcd; (row_bytes + 16) * (height - 1) + row_bytes];
    for y in 0..height {
        result[y * (row_bytes + 16)..y * (row_bytes + 16) + row_bytes]
            .copy_from_slice(&tight[y * row_bytes..(y + 1) * row_bytes]);
    }
    result
}

fn compare_bytes(
    label: &str,
    actual: &[u8],
    expected: &[u8],
    row_bytes: usize,
) -> Result<(), String> {
    if actual.len() != expected.len() {
        return Err(format!("{label}: readback size mismatch"));
    }
    if let Some((i, (got, want))) = actual
        .iter()
        .zip(expected)
        .enumerate()
        .find(|(_, (a, b))| a != b)
    {
        return Err(format!(
            "{label}: row={} byte={} got={got:#04x} expected={want:#04x}",
            i / row_bytes,
            i % row_bytes
        ));
    }
    Ok(())
}

fn raw_copy(path: &str, format: TextureFormat) -> Result<(), String> {
    // Float rows exceed CP_MEMCPY's 128-dword chunk limit. Depth crosses tiled
    // row boundaries. Nonzero origins and guard pixels expose pitch/extent bugs.
    let (width, height) = (73u32, 19u32);
    let bpp = format.bytes_per_pixel().ok_or("missing texel size")? as usize;
    let row_bytes = width as usize * bpp;
    let source_bytes: Vec<u8> = (0..width as usize * height as usize)
        .flat_map(|pixel| {
            let x = pixel % width as usize;
            let y = pixel / width as usize;
            match format {
                TextureFormat::Depth32Float => {
                    // Finite, exactly representable depths; D32 is an A2D copy.
                    let values = [0.0f32, 0.125, 0.25, 0.5, 0.75, 1.0, 0.234375];
                    values[(x + y * 3) % values.len()].to_le_bytes().to_vec()
                }
                TextureFormat::Rgba16Float => {
                    // CP copies must preserve NaN payloads, infinities and -0.
                    let bits = [
                        0x0000u16, 0x8000, 0x7e35, 0x7c00, 0xfc00, 0xc000, 0x3555, 0x0001,
                    ];
                    (0..4)
                        .flat_map(|c| bits[(x + 3 * y + c) % bits.len()].to_le_bytes())
                        .collect()
                }
                TextureFormat::Rgba32Float => {
                    let bits = [
                        0x0000_0000u32,
                        0x8000_0000,
                        0x7fc1_2345,
                        0x7f80_0000,
                        0xff80_0000,
                        0xc000_0000,
                        0x3eaa_aaab,
                        0x0000_0001,
                    ];
                    (0..4)
                        .flat_map(|c| bits[(x + 3 * y + c) % bits.len()].to_le_bytes())
                        .collect()
                }
                _ => unreachable!(),
            }
        })
        .collect();
    let guard: Vec<u8> = match format {
        TextureFormat::Depth32Float => 0.875f32.to_le_bytes().to_vec(),
        TextureFormat::Rgba16Float => [0x4400u16, 0xc000, 0x3800, 0x3c00]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect(),
        TextureFormat::Rgba32Float => [4.0f32, -2.0, 0.5, 1.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect(),
        _ => unreachable!(),
    };
    let destination_bytes = guard.repeat(width as usize * height as usize);
    let upload = padded_rows(&source_bytes, row_bytes, height as usize);
    let mut expected = destination_bytes.clone();
    for row in 0..13usize {
        let src = (row + 2) * row_bytes + 3 * bpp;
        let dst = (row + 4) * row_bytes + 2 * bpp;
        expected[dst..dst + 67 * bpp].copy_from_slice(&source_bytes[src..src + 67 * bpp]);
    }
    let table = ResourceTable::new();
    let source = texture(&table, format, width, height)?;
    let destination = texture(&table, format, width, height)?;
    let driver = DriverSession::new(path, &table)?;
    let mut encoder = CommandEncoder::new(&table);
    checked!(encoder.write_texture(
        source,
        checked!(TextureWrite::new(
            rect(0, 0, width, height)?,
            (row_bytes + 16) as u32,
            &upload
        ))
    ));
    checked!(encoder.write_texture(
        destination,
        checked!(TextureWrite::new(
            rect(0, 0, width, height)?,
            row_bytes as u32,
            &destination_bytes
        ))
    ));
    checked!(encoder.resource_barrier(ResourceBarrier::Global));
    checked!(encoder.copy_texture_to_texture(
        source,
        rect(3, 2, 67, 13)?,
        destination,
        rect(2, 4, 67, 13)?
    ));
    driver.submit(&checked!(encoder.finish()))?;
    let mut output = vec![0xa5; source_bytes.len()];
    driver
        .read_texture(source.slot(), &mut output)
        .map_err(|error| format!("{format:?} source readback: {error}"))?;
    compare_bytes(
        &format!("{format:?} upload/source preservation"),
        &output,
        &source_bytes,
        row_bytes,
    )?;
    output.fill(0xa5);
    driver
        .read_texture(destination.slot(), &mut output)
        .map_err(|error| format!("{format:?} destination readback: {error}"))?;
    compare_bytes(
        &format!("{format:?} copy/guard preservation"),
        &output,
        &expected,
        row_bytes,
    )?;
    println!(
        "PASS programmable-transfer-raw format={format:?} pixels={} copied=871 padded-upload=true exact-bits=true",
        width * height * 2
    );
    Ok(())
}

// The finite oracle values are all binary fractions representable in half.
fn half_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    if value == 0.0 {
        return (bits >> 16) as u16;
    }
    let exponent = ((bits >> 23) & 255) as i32 - 127 + 15;
    assert!((1..31).contains(&exponent) && bits & 0x1fff == 0);
    ((bits >> 16) as u16 & 0x8000) | ((exponent as u16) << 10) | ((bits >> 13) as u16 & 1023)
}

fn half_value(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = (bits >> 10) & 31;
    let fraction = bits & 1023;
    match exponent {
        0 => sign * (fraction as f32) / 16_777_216.0,
        31 if fraction == 0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + fraction as f32 / 1024.0) * 2.0f32.powi(exponent as i32 - 15),
    }
}

fn float_pixel(format: TextureFormat, rgba: [f32; 4]) -> Vec<u8> {
    if format == TextureFormat::Rgba16Float {
        rgba.into_iter()
            .flat_map(|v| half_bits(v).to_le_bytes())
            .collect()
    } else {
        rgba.into_iter().flat_map(f32::to_le_bytes).collect()
    }
}

fn pattern(x: u32, y: u32) -> [f32; 4] {
    [
        x as f32 / 8.0 - 1.0,
        y as f32 / 4.0 - 1.0,
        ((x + y) % 3) as f32 / 4.0,
        1.0,
    ]
}

fn filtered_blit(path: &str, format: TextureFormat) -> Result<(), String> {
    let table = ResourceTable::new();
    let source = texture(&table, format, 12, 10)?;
    let destination = texture(&table, format, 8, 7)?;
    let driver = DriverSession::new(path, &table)?;
    let bpp = format.bytes_per_pixel().ok_or("missing texel size")? as usize;
    let source_bytes: Vec<u8> = (0..120)
        .flat_map(|i| float_pixel(format, pattern(i % 12, i / 12)))
        .collect();
    let guard = [4.0, -2.0, 0.5, 1.0];
    let guard_bytes = float_pixel(format, guard);
    let destination_bytes = guard_bytes.repeat(56);
    for flips in [[false, false], [true, true]] {
        let mut encoder = CommandEncoder::new(&table);
        checked!(encoder.write_texture(
            source,
            checked!(TextureWrite::new(
                rect(0, 0, 12, 10)?,
                12 * bpp as u32,
                &source_bytes
            ))
        ));
        checked!(encoder.write_texture(
            destination,
            checked!(TextureWrite::new(
                rect(0, 0, 8, 7)?,
                8 * bpp as u32,
                &destination_bytes
            ))
        ));
        checked!(encoder.resource_barrier(ResourceBarrier::Global));
        checked!(encoder.blit_texture_region_flipped(
            source,
            0,
            rect(2, 2, 8, 6)?,
            destination,
            0,
            rect(2, 2, 4, 3)?,
            FilterMode::Linear,
            flips
        ));
        driver.submit(&checked!(encoder.finish()))?;
        let mut output = vec![0xa5; destination_bytes.len()];
        driver.read_texture(destination.slot(), &mut output)?;
        for (i, pixel) in output.chunks_exact(bpp).enumerate() {
            let (x, y) = (i as u32 % 8, i as u32 / 8);
            if !(2..6).contains(&x) || !(2..5).contains(&y) {
                compare_bytes(
                    &format!("{format:?} blit guard at {x},{y}"),
                    pixel,
                    &guard_bytes,
                    bpp,
                )?;
                continue;
            }
            // 2:1 downsampling: the destination centre lies halfway between
            // four source texel centres. Reversing an axis reverses its pair.
            let u = if flips[0] { 3 - (x - 2) } else { x - 2 };
            let v = if flips[1] { 2 - (y - 2) } else { y - 2 };
            let (sx, sy) = (2 + u * 2, 2 + v * 2);
            let samples = [
                pattern(sx, sy),
                pattern(sx + 1, sy),
                pattern(sx, sy + 1),
                pattern(sx + 1, sy + 1),
            ];
            for channel in 0..4 {
                let expected = samples.iter().map(|p| p[channel]).sum::<f32>() * 0.25;
                let got = if bpp == 8 {
                    half_value(u16::from_le_bytes(
                        pixel[channel * 2..channel * 2 + 2].try_into().unwrap(),
                    ))
                } else {
                    f32::from_le_bytes(pixel[channel * 4..channel * 4 + 4].try_into().unwrap())
                };
                let tolerance = if bpp == 8 { 1.0 / 1024.0 } else { 0.00001 };
                if !got.is_finite() || (got - expected).abs() > tolerance {
                    return Err(format!(
                        "{format:?} linear blit flips={flips:?} pixel={x},{y} channel={channel}: got={got} expected={expected}"
                    ));
                }
            }
        }
        println!(
            "PASS programmable-transfer-linear format={format:?} flips={flips:?} pixels=56 guards=true"
        );
    }
    Ok(())
}

pub(super) fn run(path: &str) -> Result<(), String> {
    if std::env::args().any(|arg| arg == "--compile-only") {
        println!("SKIP programmable-transfers execution (--compile-only)");
        return Ok(());
    }
    println!("BEGIN programmable-transfers");
    let mut failures = Vec::new();
    for format in [
        TextureFormat::Depth32Float,
        TextureFormat::Rgba16Float,
        TextureFormat::Rgba32Float,
    ] {
        if let Err(error) = raw_copy(path, format) {
            println!("FAIL programmable-transfer-raw format={format:?}: {error}");
            failures.push(error);
        }
    }
    for format in [TextureFormat::Rgba16Float, TextureFormat::Rgba32Float] {
        if let Err(error) = filtered_blit(path, format) {
            println!("FAIL programmable-transfer-linear format={format:?}: {error}");
            failures.push(error);
        }
    }
    if failures.is_empty() {
        println!("PASS programmable-transfers cases=5");
        Ok(())
    } else {
        Err(format!(
            "programmable transfers failed: {}",
            failures.join("; ")
        ))
    }
}
