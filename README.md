# ROSE Zone Converter for Bevy 0.16

This tool converts ROSE Online terrain files (`.ZON`, `.HIM`, `.TIL`) into high-quality, Bevy-friendly assets using native PBR features.

## Features

- **High-Quality Mesh**: Generates 65x65 vertex meshes per block with accurate normals.
- **Texture Baking**: Bakes multi-layer tile textures (with rotation and alpha blending) into a single high-resolution Albedo map per block.
- **Normal Map Generation**: Automatically generates high-detail normal maps from heightmaps to maximize lighting quality.
- **Lightmap Support**: Converts original terrain lightmaps to standard PNGs.
- **Bevy Native**: Outputs standard PNGs and binary mesh data that can be easily loaded into Bevy 0.16 using `StandardMaterial`.

## Usage

```bash
cargo run --release -- --data-root <PATH_TO_3DDATA_FOLDER> --resolution 2048
```

- `--data-root`: Path to the ROSE Online `3Ddata` folder. The tool will recursively find all zones and output converted assets in-place.
- `--resolution`: Resolution of the baked Albedo and Normal maps (default: 2048).
- `--zone`: (Optional) Process only a specific zone directory (relative to `data_root`).

## Loading in Bevy

The output includes:
- `block_X_Y.mesh.bin`: Raw vertex data.
- `block_X_Y_albedo.png`: Baked Albedo.
- `block_X_Y_normal.png`: Generated Normal Map.
- `block_X_Y_lightmap.png`: (Optional) Lightmap.

### Example Loading Code (Bevy 0.16)

```rust
// 1. Load the baked textures
let albedo = asset_server.load("block_X_Y_albedo.png");
let normal = asset_server.load("block_X_Y_normal.png");

// 2. Create a StandardMaterial
let material = materials.add(StandardMaterial {
    base_color_texture: Some(albedo),
    normal_map_texture: Some(normal),
    perceptual_roughness: 0.8,
    metallic: 0.0,
    ..default()
});

// 3. Load the mesh (requires a custom loader or manual parsing of .mesh.bin)
// Binary format:
// [u32 vertex_count]
// [positions: [f32; 3] * vertex_count]
// [normals: [f32; 3] * vertex_count]
// [uvs: [f32; 2] * vertex_count]
// [u32 has_tangents] (1 or 0)
// [tangents: [f32; 4] * vertex_count] (if has_tangents == 1)
// [u32 index_count]
// [indices: u32 * index_count]
```

## Quality Improvements

To maximize quality, this converter:
1.  Uses **Lanczos3** filtering for texture resizing during baking.
2.  Calculates **per-pixel normals** via generated normal maps, providing much smoother lighting than the original game's per-vertex normals.
3.  Supports **Bevy's native PBR pipeline**, allowing the terrain to react realistically to dynamic lights, shadows, and environment maps.
