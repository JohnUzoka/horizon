//! Merged water (`SectionMesh::merge_water`) drawn by the production terrain shader beside the
//! quads it merged: the same picture from above and below, with fog and without.
use crate::mesher::{SectionMesh, TerrainVertex};
use bytemuck::Zeroable;
use wgpu::util::DeviceExt;

const SIZE: u32 = 256;
/// The water sprite's layer, past the first: a merged face's flag read as part of the layer
/// picks another, magenta one.
const WATER: u16 = 5;
const LAYERS: u32 = 8;
/// The clear colour, exact in eight bits.
const CLEAR: [u8; 4] = [0, 51, 0, 255];

/// fluid.rs's quads for a lake at height `y`: shores along a diagonal and a cove, a row in shade,
/// another biome's tint, and the underside but under a dock.
fn lake(y: f32) -> Vec<TerrainVertex> {
    // The top's corners (nw, sw, se, ne), then its underside's (nw, ne, se, sw).
    let corners: [[[u8; 2]; 4]; 2] = [
        [[0, 0], [0, 1], [1, 1], [1, 0]],
        [[0, 0], [1, 0], [1, 1], [0, 1]],
    ];
    let mut quads = Vec::new();
    for z in 0..16u8 {
        for x in 0..16u8 {
            if x + z < 5 || (9..12).contains(&x) && (4..7).contains(&z) {
                continue;
            }
            let light = if z == 10 && x < 8 { 0x0070 } else { 0x00F0 };
            let color = if x >= 13 { 0xFF3F_76E4 } else { 0xFF44_AFF5 };
            for (side, corners) in corners.iter().enumerate() {
                if side == 1 && (2..6).contains(&x) && z >= 12 {
                    continue;
                }
                quads.extend(corners.map(|[dx, dz]| {
                    TerrainVertex::new(
                        [f32::from(x + dx), y, f32::from(z + dz)],
                        [f32::from(dx), f32::from(dz)],
                        WATER,
                        color,
                        light,
                    )
                }));
            }
        }
    }
    quads
}

#[test]
fn merged_water_draws_as_the_quads_it_merged() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let Ok((device, queue)) = pollster::block_on(adapter.request_device(&Default::default()))
    else {
        eprintln!("no GPU device: skipped");
        return;
    };
    eprintln!("merged water GPU: {}", adapter.get_info().name);
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(include_str!("../terrain.wgsl").into()),
    });
    // gfx.rs's terrain bindings.
    let vf = wgpu::ShaderStages::VERTEX_FRAGMENT;
    let texture_entry = |binding, view_dimension| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vf,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension,
            multisampled: false,
        },
        count: None,
    };
    let sampler_entry = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vf,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    let mut entries = vec![
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: vf,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        texture_entry(1, wgpu::TextureViewDimension::D2Array),
        sampler_entry(2),
        texture_entry(3, wgpu::TextureViewDimension::D2),
        sampler_entry(4),
    ];
    entries.extend((5..12).map(|binding| texture_entry(binding, wgpu::TextureViewDimension::D2Array)));
    let bindings = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &entries,
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&bindings)],
        ..Default::default()
    });
    let instance_attributes = wgpu::vertex_attr_array![5=>Float32x4];
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_terrain"),
            compilation_options: Default::default(),
            buffers: &[
                Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<TerrainVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &super::TERRAIN_ATTRIBUTES,
                }),
                Some(wgpu::VertexBufferLayout {
                    array_stride: 16,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &instance_attributes,
                }),
            ],
        },
        // As gfx.rs's translucent pipeline, but without the depth test: a pixel two faces cover,
        // where an edge ended along another, is blended twice.
        primitive: wgpu::PrimitiveState {
            cull_mode: Some(wgpu::Face::Back),
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_translucent"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let texture = |size: wgpu::Extent3d, texels: &[u8]| {
        device.create_texture_with_data(
            &queue,
            &wgpu::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            texels,
        )
    };
    // Every texel of the water sprite its own, half see-through; the other layers magenta.
    let sprite_texels: Vec<u8> = (0..LAYERS)
        .flat_map(|layer| {
            (0..256u32).flat_map(move |i| {
                let (u, v) = ((i % 16) as u8, (i / 16) as u8);
                if layer == u32::from(WATER) {
                    [u * 16 + 8, v * 16 + 8, if (u ^ v) & 1 == 0 { 240 } else { 40 }, 160]
                } else {
                    [255, 0, 255, 255]
                }
            })
        })
        .collect();
    let sprites = texture(
        wgpu::Extent3d {
            width: 16,
            height: 16,
            depth_or_array_layers: LAYERS,
        },
        &sprite_texels,
    )
    .create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    // Brighter with sky light, the lightmap's y.
    let lightmap_texels: Vec<u8> = (0..256u32)
        .flat_map(|i| {
            let s = (i / 16) as u8 * 17;
            [s, s, s, 255]
        })
        .collect();
    let lightmap = texture(
        wgpu::Extent3d {
            width: 16,
            height: 16,
            depth_or_array_layers: 1,
        },
        &lightmap_texels,
    )
    .create_view(&Default::default());
    // gfx.rs's block sprite sampler.
    let sprite_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });
    let lightmap_sampler = device.create_sampler(&Default::default());
    let size = wgpu::Extent3d {
        width: SIZE,
        height: SIZE,
        depth_or_array_layers: 1,
    };
    // The picture of `quads` in the section at camera-relative `origin`, looking at `target`.
    let draw = |quads: &[TerrainVertex], origin: [f32; 3], target: glam::Vec3, fog: bool| {
        let projection = glam::Mat4::perspective_rh(70f32.to_radians(), 1.0, 0.05, 500.0);
        let view = glam::Mat4::look_at_rh(glam::Vec3::ZERO, target, glam::Vec3::Y);
        let globals = super::Globals {
            view_proj: (projection * view).to_cols_array_2d(),
            // Fog over the first 40 blocks, in a colour far from the water's.
            fog_color: if fog { [0.9, 0.1, 0.1, 1.0] } else { [0.0; 4] },
            fog: [0.0, 40.0, 1000.0, 2000.0],
            ..super::Globals::zeroed()
        };
        let buffer = |contents: &[u8], usage| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents,
                usage,
            })
        };
        let globals = buffer(bytemuck::bytes_of(&globals), wgpu::BufferUsages::UNIFORM);
        let mut group_entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&sprites),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&sprite_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&lightmap),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::Sampler(&lightmap_sampler),
            },
        ];
        group_entries.extend((5..12).map(|binding| wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::TextureView(&sprites),
        }));
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bindings,
            entries: &group_entries,
        });
        // translucency.rs's order: v, v+1, v+2, v+2, v+3, v.
        let indices: Vec<u32> = (0..quads.len() as u32 / 4)
            .flat_map(|q| [4 * q, 4 * q + 1, 4 * q + 2, 4 * q + 2, 4 * q + 3, 4 * q])
            .collect();
        let vertices = buffer(bytemuck::cast_slice(quads), wgpu::BufferUsages::VERTEX);
        let index_buffer = buffer(bytemuck::cast_slice(&indices), wgpu::BufferUsages::INDEX);
        let instance = buffer(
            bytemuck::cast_slice(&[super::section_instance(origin, 0, 1.0)]),
            wgpu::BufferUsages::VERTEX,
        );
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.2,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.set_vertex_buffer(1, instance.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
        }
        let read = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(SIZE * SIZE * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &read,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(SIZE * 4),
                    rows_per_image: None,
                },
            },
            size,
        );
        queue.submit([encoder.finish()]);
        read.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let pixels: Vec<[u8; 4]> = bytemuck::cast_slice(&read.slice(..).get_mapped_range().unwrap()).to_vec();
        pixels
    };
    let y = 15.0 - 1.0 / 9.0 - 0.001;
    let quads = lake(y);
    let mut mesh = SectionMesh {
        translucent: quads.clone(),
        ..Default::default()
    };
    assert!(mesh.merge_water());
    assert!(mesh.translucent.len() * 2 < quads.len());
    // From five blocks above the lake and from under it, two to eighteen blocks out.
    for (name, origin, fog) in [
        ("above", [-8.0, -20.0, -18.0], false),
        ("above, fogged", [-8.0, -20.0, -18.0], true),
        ("below, fogged", [-8.0, -10.0, -18.0], true),
    ] {
        let target = glam::Vec3::new(0.0, origin[1] + y, origin[2] + 8.0);
        let before = draw(&quads, origin, target, fog);
        let after = draw(&mesh.translucent, origin, target, fog);
        let water = before.iter().filter(|&&p| p != CLEAR).count();
        let differ = before
            .iter()
            .zip(&after)
            .filter(|(a, b)| a.iter().zip(b.iter()).any(|(a, b)| a.abs_diff(*b) > 1))
            .count();
        eprintln!("{name}: {water} pixels of water, {differ} differ");
        assert!(water * 5 > (SIZE * SIZE) as usize, "{name}: the lake is out of view");
        assert!(differ * 500 <= water, "{name}: {differ} of {water} pixels differ");
    }
}
