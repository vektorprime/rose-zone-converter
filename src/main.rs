use anyhow::{Context, Result};
use bevy::prelude::*;
use bevy::render::mesh::{Indices, PrimitiveTopology};
use bevy::render::render_asset::RenderAssetUsages;
use clap::Parser;
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgba};
use rayon::prelude::*;
use rose_file_readers::{HimFile, RoseFile, RoseFileReader, TilFile, ZonFile, ZonTileRotation};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the ROSE Online 3Ddata folder
    #[arg(short, long)]
    data_root: PathBuf,

    /// Resolution of the baked albedo texture per block
    #[arg(short, long, default_value_t = 2048)]
    resolution: u32,

    /// Process only a specific zone directory (relative to data_root)
    #[arg(short, long)]
    zone: Option<String>,
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    if !args.data_root.exists() {
        anyhow::bail!("3Ddata root directory does not exist: {:?}", args.data_root);
    }

    let data_root = args.data_root.canonicalize()?;
    println!("Using 3Ddata root: {:?}", data_root);

    if let Some(zone_rel_path) = &args.zone {
        let zone_path = data_root.join(zone_rel_path.replace('\\', "/"));
        process_zone_dir(&args, &data_root, &zone_path)?;
    } else {
        // Recursively find all .ZON files
        println!("Searching for zones in {:?}...", data_root);
        let zone_dirs: Vec<PathBuf> = WalkDir::new(&data_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map_or(false, |ext| ext.to_ascii_uppercase() == "ZON")
            })
            .map(|e| e.path().parent().unwrap().to_path_buf())
            .collect();

        println!("Found {} zones", zone_dirs.len());
        zone_dirs.par_iter().for_each(|zone_dir| {
            println!("\nProcessing zone: {:?}", zone_dir);
            if let Err(e) = process_zone_dir(&args, &data_root, zone_dir) {
                eprintln!("Error processing zone {:?}: {}", zone_dir, e);
            }
        });
    }

    Ok(())
}

fn process_zone_dir(args: &Args, data_root: &Path, zone_dir: &Path) -> Result<()> {
    // Find the .ZON file in the zone directory
    let zon_path = std::fs::read_dir(zone_dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .map_or(false, |ext| ext.to_ascii_uppercase() == "ZON")
        })
        .context("Could not find .ZON file in zone directory")?;

    println!("Loading Zone: {:?}", zon_path);
    let zon_bytes = std::fs::read(&zon_path)?;
    let zon: ZonFile = RoseFile::read(RoseFileReader::from(&zon_bytes), &Default::default())
        .map_err(|e| anyhow::anyhow!("Failed to parse ZON file: {:?}", e))?;

    // Collect all valid blocks (those with a .HIM file)
    let blocks: Vec<(u32, u32)> = (0..64u32)
        .flat_map(|by| (0..64u32).map(move |bx| (bx, by)))
        .filter(|(bx, by)| zone_dir.join(format!("{}_{}.HIM", bx, by)).exists())
        .collect();

    let total = blocks.len();
    let done = AtomicUsize::new(0);
    println!("  Found {} blocks to process", total);

    // Process blocks in parallel
    blocks.par_iter().for_each(|&(block_x, block_y)| {
        if let Err(e) = process_block(args, data_root, zone_dir, &zon, block_x, block_y) {
            eprintln!("    Error processing block {}_{}: {}", block_x, block_y, e);
        }
        let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
        println!(
            "  [{}/{}] Finished block {}_{}",
            completed, total, block_x, block_y
        );
    });

    Ok(())
}

fn process_block(
    args: &Args,
    data_root: &Path,
    zone_dir: &Path,
    zon: &ZonFile,
    block_x: u32,
    block_y: u32,
) -> Result<()> {
    let him_path = zone_dir.join(format!("{}_{}.HIM", block_x, block_y));
    let til_path = zone_dir.join(format!("{}_{}.TIL", block_x, block_y));

    let him_bytes = std::fs::read(&him_path)?;
    let him: HimFile = RoseFile::read(RoseFileReader::from(&him_bytes), &Default::default())
        .map_err(|e| anyhow::anyhow!("Failed to parse HIM file: {:?}", e))?;

    let til = if til_path.exists() {
        let til_bytes = std::fs::read(&til_path)?;
        Some(
            RoseFile::read(RoseFileReader::from(&til_bytes), &Default::default())
                .map_err(|e| anyhow::anyhow!("Failed to parse TIL file: {:?}", e))?,
        )
    } else {
        None
    };

    // 1. Generate Mesh
    let mesh = generate_mesh(&him);
    let mesh_path = zone_dir.join(format!("block_{}_{}.mesh.bin", block_x, block_y));
    if mesh_path.exists() {
        println!("    Overwriting existing mesh: {:?}", mesh_path);
    }
    save_mesh(&mesh, &mesh_path)?;

    // 2. Bake Albedo
    if let Some(til) = &til {
        let albedo = bake_albedo(args, data_root, zon, til)?;
        let albedo_path = zone_dir.join(format!("block_{}_{}_albedo.png", block_x, block_y));
        if albedo_path.exists() {
            println!("    Overwriting existing albedo: {:?}", albedo_path);
        }
        albedo.save(albedo_path)?;
    }

    // 3. Generate Normal Map
    let normal_map = generate_normal_map(&him, args.resolution);
    let normal_map_path = zone_dir.join(format!("block_{}_{}_normal.png", block_x, block_y));
    if normal_map_path.exists() {
        println!("    Overwriting existing normal map: {:?}", normal_map_path);
    }
    normal_map.save(normal_map_path)?;

    // 4. Convert Lightmap (if exists)
    let lm_path = zone_dir.join(format!("{}_{}/LM_TERRAIN.DDS", block_x, block_y));
    if lm_path.exists() {
        let lm_img = image::open(&lm_path)?;
        lm_img.save(zone_dir.join(format!("block_{}_{}_lightmap.png", block_x, block_y)))?;
    }

    // 5. Save Material Info (RON)
    let material_info = format!(
        "(
            base_color_texture: Some(\"block_{}_{}_albedo.png\"),
            normal_map_texture: Some(\"block_{}_{}_normal.png\"),
            perceptual_roughness: 0.8,
            metallic: 0.0,
        )",
        block_x, block_y, block_x, block_y
    );
    let mat_path = zone_dir.join(format!("block_{}_{}.mat.ron", block_x, block_y));
    if mat_path.exists() {
        println!("    Overwriting existing material: {:?}", mat_path);
    }
    std::fs::write(mat_path, material_info)?;

    Ok(())
}

fn generate_mesh(him: &HimFile) -> Mesh {
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut uvs = Vec::new();
    let mut indices = Vec::new();

    let width = him.width as u32;
    let height = him.height as u32;

    for y in 0..height {
        for x in 0..width {
            let h = him.get_clamped(x as i32, y as i32) / 100.0;
            positions.push([x as f32 * 2.5, h, y as f32 * 2.5]);

            let eps = 0.5;
            let h_l = sample_him(him, x as f32 - eps, y as f32) / 100.0;
            let h_r = sample_him(him, x as f32 + eps, y as f32) / 100.0;
            let h_t = sample_him(him, x as f32, y as f32 - eps) / 100.0;
            let h_b = sample_him(him, x as f32, y as f32 + eps) / 100.0;
            // Y component accounts for the 2.5 world-unit vertex spacing
            let n = Vec3::new(h_l - h_r, 2.0 * eps * 2.5, h_t - h_b).normalize();
            normals.push([n.x, n.y, n.z]);

            uvs.push([
                x as f32 / (width - 1) as f32,
                y as f32 / (height - 1) as f32,
            ]);
        }
    }

    for y in 0..height - 1 {
        for x in 0..width - 1 {
            let i = y * width + x;
            indices.push(i);
            indices.push(i + width);
            indices.push(i + 1);

            indices.push(i + 1);
            indices.push(i + width);
            indices.push(i + width + 1);
        }
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(indices));

    if let Err(e) = mesh.generate_tangents() {
        eprintln!("      Warning: Failed to generate tangents: {}", e);
    }

    mesh
}

fn save_mesh(mesh: &Mesh, path: &Path) -> Result<()> {
    use bevy::render::mesh::VertexAttributeValues;
    let mut data = Vec::new();
    let positions = match mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap() {
        VertexAttributeValues::Float32x3(v) => v,
        _ => panic!("Expected Float32x3 positions"),
    };
    let normals = match mesh.attribute(Mesh::ATTRIBUTE_NORMAL).unwrap() {
        VertexAttributeValues::Float32x3(v) => v,
        _ => panic!("Expected Float32x3 normals"),
    };
    let uvs = match mesh.attribute(Mesh::ATTRIBUTE_UV_0).unwrap() {
        VertexAttributeValues::Float32x2(v) => v,
        _ => panic!("Expected Float32x2 UVs"),
    };
    let tangents = mesh.attribute(Mesh::ATTRIBUTE_TANGENT).map(|t| match t {
        VertexAttributeValues::Float32x4(v) => v,
        _ => panic!("Expected Float32x4 tangents"),
    });
    let indices = match mesh.indices().unwrap() {
        Indices::U32(i) => i,
        _ => panic!("Expected U32 indices"),
    };

    data.extend_from_slice(&(positions.len() as u32).to_le_bytes());
    for p in positions {
        for v in p {
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    for n in normals {
        for v in n {
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    for u in uvs {
        for v in u {
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    if let Some(tangents) = tangents {
        data.extend_from_slice(&1u32.to_le_bytes());
        for t in tangents {
            for v in t {
                data.extend_from_slice(&v.to_le_bytes());
            }
        }
    } else {
        data.extend_from_slice(&0u32.to_le_bytes());
    }
    data.extend_from_slice(&(indices.len() as u32).to_le_bytes());
    for i in indices {
        data.extend_from_slice(&i.to_le_bytes());
    }

    std::fs::write(path, data)?;
    Ok(())
}

fn bake_albedo(
    args: &Args,
    data_root: &Path,
    zon: &ZonFile,
    til: &TilFile,
) -> Result<DynamicImage> {
    let res = args.resolution;
    let mut baked = ImageBuffer::new(res, res);

    let tile_res = res / 16;
    let mut texture_cache = std::collections::HashMap::new();

    for tile_y in 0..16 {
        for tile_x in 0..16 {
            let tile_idx = til.get_clamped(tile_x as usize, tile_y as usize) as usize;
            if tile_idx >= zon.tiles.len() {
                continue;
            }

            let tile_info = &zon.tiles[tile_idx];
            let layer1_tex_idx = (tile_info.layer1 + tile_info.offset1) as usize;
            let layer2_tex_idx = (tile_info.layer2 + tile_info.offset2) as usize;

            let layer1_path = &zon.tile_textures[layer1_tex_idx];
            let layer2_path = &zon.tile_textures[layer2_tex_idx];

            let layer1_img = load_tile_texture(data_root, layer1_path, &mut texture_cache)?;
            let layer2_img = load_tile_texture(data_root, layer2_path, &mut texture_cache)?;

            let layer2_rotated = apply_tile_rotation(&layer2_img, &tile_info.rotation);

            let l1 =
                layer1_img.resize_exact(tile_res, tile_res, image::imageops::FilterType::Lanczos3);
            let l2 = layer2_rotated.resize_exact(
                tile_res,
                tile_res,
                image::imageops::FilterType::Lanczos3,
            );

            for y in 0..tile_res {
                for x in 0..tile_res {
                    let c1 = l1.get_pixel(x, y);
                    let c2 = l2.get_pixel(x, y);

                    let a2 = c2[3] as f32 / 255.0;
                    let r = (c1[0] as f32 * (1.0 - a2) + c2[0] as f32 * a2) as u8;
                    let g = (c1[1] as f32 * (1.0 - a2) + c2[1] as f32 * a2) as u8;
                    let b = (c1[2] as f32 * (1.0 - a2) + c2[2] as f32 * a2) as u8;

                    baked.put_pixel(
                        tile_x * tile_res + x,
                        tile_y * tile_res + y,
                        Rgba([r, g, b, 255]),
                    );
                }
            }
        }
    }

    Ok(DynamicImage::ImageRgba8(baked))
}

fn load_tile_texture(
    data_root: &Path,
    rel_path: &str,
    cache: &mut std::collections::HashMap<String, DynamicImage>,
) -> Result<DynamicImage> {
    if !cache.contains_key(rel_path) {
        let normalized_rel_path = rel_path.replace('\\', "/");

        // Handle the case where rel_path starts with "3DDATA/" and data_root is also the 3Ddata folder
        let final_rel_path = if normalized_rel_path.to_uppercase().starts_with("3DDATA/") {
            &normalized_rel_path[7..]
        } else {
            &normalized_rel_path
        };

        let mut full_path = data_root.join(final_rel_path);

        if !full_path.exists() {
            // Try case-insensitive search or common variations
            let mut found = false;
            if let Some(parent) = full_path.parent() {
                if parent.exists() {
                    if let Ok(entries) = std::fs::read_dir(parent) {
                        for entry in entries.filter_map(|e| e.ok()) {
                            if entry.file_name().to_string_lossy().to_uppercase()
                                == full_path
                                    .file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .to_uppercase()
                            {
                                full_path = entry.path();
                                found = true;
                                break;
                            }
                        }
                    }
                }
            }

            if !found {
                eprintln!("      Warning: Texture not found: {:?}", full_path);
                // Try one more thing: maybe data_root is the parent of 3Ddata
                let alt_path = data_root
                    .parent()
                    .unwrap_or(data_root)
                    .join(&normalized_rel_path);
                if alt_path.exists() {
                    full_path = alt_path;
                } else {
                    // Fallback to a magenta texture if missing
                    let img = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(
                        128,
                        128,
                        Rgba([255, 0, 255, 255]),
                    ));
                    cache.insert(rel_path.to_string(), img);
                    return Ok(cache.get(rel_path).unwrap().clone());
                }
            }
        }

        // Use a more robust DDS loader if it's a DDS file
        let img = if full_path
            .extension()
            .map_or(false, |ext| ext.to_ascii_uppercase() == "DDS")
        {
            match load_dds(&full_path) {
                Ok(img) => img,
                Err(e) => {
                    eprintln!(
                        "      Warning: Failed to load DDS {:?}: {}. Using image crate fallback.",
                        full_path, e
                    );
                    image::open(&full_path).map_err(|e| {
                        anyhow::anyhow!("Failed to open texture {:?}: {}", full_path, e)
                    })?
                }
            }
        } else {
            image::open(&full_path)
                .map_err(|e| anyhow::anyhow!("Failed to open texture {:?}: {}", full_path, e))?
        };

        cache.insert(rel_path.to_string(), img);
    }
    Ok(cache.get(rel_path).unwrap().clone())
}

fn load_dds(path: &Path) -> Result<DynamicImage> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 128 || &bytes[0..4] != b"DDS " {
        anyhow::bail!("Not a valid DDS file");
    }

    let header = &bytes[4..128];
    let height = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    let width = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    let pf_flags = u32::from_le_bytes([header[76], header[77], header[78], header[79]]);
    let pf_fourcc = &header[80..84];
    let pf_bit_count = u32::from_le_bytes([header[84], header[85], header[86], header[87]]);
    let pf_r_mask = u32::from_le_bytes([header[88], header[89], header[90], header[91]]);
    let pf_g_mask = u32::from_le_bytes([header[92], header[93], header[94], header[95]]);
    let pf_b_mask = u32::from_le_bytes([header[96], header[97], header[98], header[99]]);
    let pf_a_mask = u32::from_le_bytes([header[100], header[101], header[102], header[103]]);

    let data_offset = 128;

    if pf_flags & 0x4 != 0 {
        // DDPF_FOURCC
        match pf_fourcc {
            b"DXT1" => return decompress_dxt1(&bytes[data_offset..], width, height),
            b"DXT3" => return decompress_dxt3(&bytes[data_offset..], width, height),
            b"DXT5" => return decompress_dxt5(&bytes[data_offset..], width, height),
            _ => {}
        }
    } else if pf_flags & 0x40 != 0 {
        // DDPF_RGB
        if pf_bit_count == 16 {
            if pf_r_mask == 0xF800 && pf_g_mask == 0x07E0 && pf_b_mask == 0x001F {
                return convert_rgb565_to_rgba8(&bytes[data_offset..], width, height);
            } else if pf_r_mask == 0x0F00
                && pf_g_mask == 0x00F0
                && pf_b_mask == 0x000F
                && pf_a_mask == 0xF000
            {
                return convert_rgba4444_to_rgba8(&bytes[data_offset..], width, height);
            }
        }
    }

    // Fallback to image crate
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Dds)?;
    Ok(img)
}

fn decompress_dxt1(data: &[u8], width: u32, height: u32) -> Result<DynamicImage> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let blocks_x = (width + 3) / 4;
    let blocks_y = (height + 3) / 4;

    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let offset = ((by * blocks_x + bx) * 8) as usize;
            if offset + 8 > data.len() {
                break;
            }
            let color0 = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let color1 = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
            let bits = u32::from_le_bytes([
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);

            let mut colors = [[0u8; 4]; 4];
            colors[0] = decode_rgb565(color0);
            colors[1] = decode_rgb565(color1);
            if color0 > color1 {
                colors[2] = lerp_color(colors[0], colors[1], 2, 1);
                colors[3] = lerp_color(colors[0], colors[1], 1, 2);
            } else {
                colors[2] = lerp_color(colors[0], colors[1], 1, 1);
                colors[3] = [0, 0, 0, 0];
            }

            for py in 0..4 {
                for px in 0..4 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let color_idx = ((bits >> ((py * 4 + px) * 2)) & 0x3) as usize;
                        let dest_offset = ((y * width + x) * 4) as usize;
                        rgba[dest_offset..dest_offset + 4].copy_from_slice(&colors[color_idx]);
                    }
                }
            }
        }
    }
    Ok(DynamicImage::ImageRgba8(
        ImageBuffer::from_raw(width, height, rgba).unwrap(),
    ))
}

fn decompress_dxt3(data: &[u8], width: u32, height: u32) -> Result<DynamicImage> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let blocks_x = (width + 3) / 4;
    let blocks_y = (height + 3) / 4;

    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let offset = ((by * blocks_x + bx) * 16) as usize;
            if offset + 16 > data.len() {
                break;
            }

            let alpha_bits = &data[offset..offset + 8];
            let color0 = u16::from_le_bytes([data[offset + 8], data[offset + 9]]);
            let color1 = u16::from_le_bytes([data[offset + 10], data[offset + 11]]);
            let bits = u32::from_le_bytes([
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ]);

            let mut colors = [[0u8; 4]; 4];
            colors[0] = decode_rgb565(color0);
            colors[1] = decode_rgb565(color1);
            colors[2] = lerp_color(colors[0], colors[1], 2, 1);
            colors[3] = lerp_color(colors[0], colors[1], 1, 2);

            for py in 0..4 {
                for px in 0..4 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let color_idx = ((bits >> ((py * 4 + px) * 2)) & 0x3) as usize;
                        let alpha_idx = (py * 4 + px) as usize;
                        let alpha = (alpha_bits[alpha_idx / 2] >> ((alpha_idx % 2) * 4)) & 0xF;
                        let alpha = (alpha << 4) | alpha;

                        let dest_offset = ((y * width + x) * 4) as usize;
                        rgba[dest_offset..dest_offset + 3]
                            .copy_from_slice(&colors[color_idx][0..3]);
                        rgba[dest_offset + 3] = alpha;
                    }
                }
            }
        }
    }
    Ok(DynamicImage::ImageRgba8(
        ImageBuffer::from_raw(width, height, rgba).unwrap(),
    ))
}

fn decompress_dxt5(data: &[u8], width: u32, height: u32) -> Result<DynamicImage> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let blocks_x = (width + 3) / 4;
    let blocks_y = (height + 3) / 4;

    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let offset = ((by * blocks_x + bx) * 16) as usize;
            if offset + 16 > data.len() {
                break;
            }

            let a0 = data[offset];
            let a1 = data[offset + 1];
            let a_bits = u64::from_le_bytes([
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
                0,
                0,
            ]);

            let mut alphas = [0u8; 8];
            alphas[0] = a0;
            alphas[1] = a1;
            if a0 > a1 {
                for i in 1..7 {
                    alphas[i + 1] = (((7 - i) * a0 as usize + i * a1 as usize) / 7) as u8;
                }
            } else {
                for i in 1..5 {
                    alphas[i + 1] = (((5 - i) * a0 as usize + i * a1 as usize) / 5) as u8;
                }
                alphas[6] = 0;
                alphas[7] = 255;
            }

            let color0 = u16::from_le_bytes([data[offset + 8], data[offset + 9]]);
            let color1 = u16::from_le_bytes([data[offset + 10], data[offset + 11]]);
            let bits = u32::from_le_bytes([
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ]);

            let mut colors = [[0u8; 4]; 4];
            colors[0] = decode_rgb565(color0);
            colors[1] = decode_rgb565(color1);
            colors[2] = lerp_color(colors[0], colors[1], 2, 1);
            colors[3] = lerp_color(colors[0], colors[1], 1, 2);

            for py in 0..4 {
                for px in 0..4 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let color_idx = ((bits >> ((py * 4 + px) * 2)) & 0x3) as usize;
                        let alpha_idx = ((a_bits >> ((py * 4 + px) * 3)) & 0x7) as usize;

                        let dest_offset = ((y * width + x) * 4) as usize;
                        rgba[dest_offset..dest_offset + 3]
                            .copy_from_slice(&colors[color_idx][0..3]);
                        rgba[dest_offset + 3] = alphas[alpha_idx];
                    }
                }
            }
        }
    }
    Ok(DynamicImage::ImageRgba8(
        ImageBuffer::from_raw(width, height, rgba).unwrap(),
    ))
}

fn decode_rgb565(c: u16) -> [u8; 4] {
    let r = ((c >> 11) & 0x1F) as u8;
    let g = ((c >> 5) & 0x3F) as u8;
    let b = (c & 0x1F) as u8;
    [
        (r << 3) | (r >> 2),
        (g << 2) | (g >> 4),
        (b << 3) | (b >> 2),
        255,
    ]
}

fn lerp_color(c0: [u8; 4], c1: [u8; 4], w0: usize, w1: usize) -> [u8; 4] {
    [
        ((c0[0] as usize * w0 + c1[0] as usize * w1) / (w0 + w1)) as u8,
        ((c0[1] as usize * w0 + c1[1] as usize * w1) / (w0 + w1)) as u8,
        ((c0[2] as usize * w0 + c1[2] as usize * w1) / (w0 + w1)) as u8,
        255,
    ]
}

fn convert_rgb565_to_rgba8(data: &[u8], width: u32, height: u32) -> Result<DynamicImage> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for i in 0..(width * height) as usize {
        let c = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
        rgba[i * 4..i * 4 + 4].copy_from_slice(&decode_rgb565(c));
    }
    Ok(DynamicImage::ImageRgba8(
        ImageBuffer::from_raw(width, height, rgba).unwrap(),
    ))
}

fn convert_rgba4444_to_rgba8(data: &[u8], width: u32, height: u32) -> Result<DynamicImage> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for i in 0..(width * height) as usize {
        let c = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
        let a = ((c >> 12) & 0xF) as u8;
        let r = ((c >> 8) & 0xF) as u8;
        let g = ((c >> 4) & 0xF) as u8;
        let b = (c & 0xF) as u8;
        rgba[i * 4] = (r << 4) | r;
        rgba[i * 4 + 1] = (g << 4) | g;
        rgba[i * 4 + 2] = (b << 4) | b;
        rgba[i * 4 + 3] = (a << 4) | a;
    }
    Ok(DynamicImage::ImageRgba8(
        ImageBuffer::from_raw(width, height, rgba).unwrap(),
    ))
}

fn apply_tile_rotation(img: &DynamicImage, rotation: &ZonTileRotation) -> DynamicImage {
    match rotation {
        ZonTileRotation::None => img.clone(),
        ZonTileRotation::FlipHorizontal => img.fliph(),
        ZonTileRotation::FlipVertical => img.flipv(),
        ZonTileRotation::Flip => img.rotate180(),
        ZonTileRotation::Clockwise90 => img.rotate90(),
        ZonTileRotation::CounterClockwise90 => img.rotate270(),
        _ => img.clone(),
    }
}

fn generate_normal_map(him: &HimFile, res: u32) -> DynamicImage {
    let mut img = ImageBuffer::new(res, res);

    for y in 0..res {
        for x in 0..res {
            let fx = (x as f32 / (res - 1) as f32) * (him.width - 1) as f32;
            let fy = (y as f32 / (res - 1) as f32) * (him.height - 1) as f32;

            let eps = 0.5;
            let h_l = sample_him(him, fx - eps, fy) / 100.0;
            let h_r = sample_him(him, fx + eps, fy) / 100.0;
            let h_t = sample_him(him, fx, fy - eps) / 100.0;
            let h_b = sample_him(him, fx, fy + eps) / 100.0;

            let dx = (h_r - h_l) / (2.0 * eps);
            let dy = (h_b - h_t) / (2.0 * eps);

            // Scale derivatives by 2.5 to match world-space vertex spacing
            let n = Vec3::new(-dx * 2.5, 1.0, -dy * 2.5).normalize();

            let r = ((n.x * 0.5 + 0.5) * 255.0) as u8;
            let g = ((n.y * 0.5 + 0.5) * 255.0) as u8;
            let b = ((n.z * 0.5 + 0.5) * 255.0) as u8;

            img.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }

    DynamicImage::ImageRgba8(img)
}

fn sample_him(him: &HimFile, x: f32, y: f32) -> f32 {
    let ix = x.floor() as i32;
    let iy = y.floor() as i32;
    let fx = x - x.floor();
    let fy = y - y.floor();

    // Catmull-Rom basis: provides C1 continuity so normals are smooth across
    // heightmap cell boundaries (bilinear only gives C0, causing block artifacts).
    fn catmull_rom(t: f32) -> [f32; 4] {
        let t2 = t * t;
        let t3 = t2 * t;
        [
            -0.5 * t3 + t2 - 0.5 * t,
            1.5 * t3 - 2.5 * t2 + 1.0,
            -1.5 * t3 + 2.0 * t2 + 0.5 * t,
            0.5 * t3 - 0.5 * t2,
        ]
    }

    let wx = catmull_rom(fx);
    let wy = catmull_rom(fy);

    let mut value = 0.0;
    for j in 0..4i32 {
        for i in 0..4i32 {
            let h = him.get_clamped(ix - 1 + i, iy - 1 + j);
            value += wx[i as usize] * wy[j as usize] * h;
        }
    }
    value
}
