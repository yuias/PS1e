//! GPU display of the emulated frame: the expanded RGBA frame is uploaded as a
//! texture and drawn into the panel rectangle by a scaler shader, so the
//! window can show it with a filter of choice (nearest, linear, sharp
//! bilinear, Lanczos-3) at any size. Runs inside egui's wgpu render pass
//! through a paint callback.

use std::sync::Arc;

use eframe::egui_wgpu::{self, wgpu};

/// How the frame is resampled to the panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScaleMode {
    /// Pixel duplication; blocky at non-integer sizes.
    Nearest,
    /// Bilinear; soft.
    Linear,
    /// Nearest to the largest integer multiple, then bilinear: crisp pixel
    /// edges without the blockiness of pure nearest.
    #[default]
    Sharp,
    /// Lanczos-3 windowed sinc: sharp smooth upscale.
    Lanczos,
}

impl ScaleMode {
    /// Every mode, in the order the settings combo lists them.
    pub const ALL: [ScaleMode; 4] = [
        ScaleMode::Nearest,
        ScaleMode::Linear,
        ScaleMode::Sharp,
        ScaleMode::Lanczos,
    ];

    /// Name shown in the settings combo.
    pub fn label(self) -> &'static str {
        match self {
            ScaleMode::Nearest => "Nearest",
            ScaleMode::Linear => "Linear",
            ScaleMode::Sharp => "Sharp",
            ScaleMode::Lanczos => "Lanczos",
        }
    }

    fn index(self) -> u32 {
        match self {
            ScaleMode::Nearest => 0,
            ScaleMode::Linear => 1,
            ScaleMode::Sharp => 2,
            ScaleMode::Lanczos => 3,
        }
    }
}

const SHADER: &str = r#"
struct Uniforms {
    src_size: vec2<f32>,
    dst_size: vec2<f32>,
    mode: u32,
    // Non-zero when the render target is an sRGB format: the frame's bytes
    // are display-encoded already, so they must be linearised on the way out
    // for the hardware encode to restore them.
    srgb_target: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var frame: texture_2d<f32>;
@group(0) @binding(2) var samp_linear: sampler;
@group(0) @binding(3) var samp_nearest: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle covering the viewport (egui sets it to the panel rect).
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var out: VsOut;
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    out.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(p.x, 1.0 - p.y);
    return out;
}

fn lanczos(x: f32) -> f32 {
    let ax = abs(x);
    if ax < 1e-4 {
        return 1.0;
    }
    if ax >= 3.0 {
        return 0.0;
    }
    let px = 3.14159265 * ax;
    return 3.0 * sin(px) * sin(px / 3.0) / (px * px);
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var c = scale(in.uv);
    if u.srgb_target != 0u {
        c = vec4<f32>(to_linear(c.rgb), c.a);
    }
    return c;
}

fn scale(uv: vec2<f32>) -> vec4<f32> {
    switch u.mode {
        case 0u: {
            return textureSample(frame, samp_nearest, uv);
        }
        case 2u: {
            // Sharp bilinear: pretend the source was first scaled up by the
            // largest integer factor with nearest, then filter linearly.
            let scale = max(floor(u.dst_size / u.src_size), vec2<f32>(1.0, 1.0));
            let texel = uv * u.src_size;
            let c = floor(texel);
            let f = clamp((fract(texel) - 0.5) * scale + 0.5, vec2<f32>(0.0), vec2<f32>(1.0));
            return textureSample(frame, samp_linear, (c + f) / u.src_size);
        }
        case 3u: {
            // Lanczos-3 over the 6x6 texel neighbourhood.
            let texel = uv * u.src_size - 0.5;
            let c = floor(texel);
            let f = texel - c;
            var sum = vec4<f32>(0.0);
            var wsum = 0.0;
            let dims = vec2<i32>(textureDimensions(frame));
            for (var j = -2; j <= 3; j++) {
                let wy = lanczos(f32(j) - f.y);
                let y = clamp(i32(c.y) + j, 0, dims.y - 1);
                for (var i = -2; i <= 3; i++) {
                    let w = wy * lanczos(f32(i) - f.x);
                    let x = clamp(i32(c.x) + i, 0, dims.x - 1);
                    sum += textureLoad(frame, vec2<i32>(x, y), 0) * w;
                    wsum += w;
                }
            }
            return clamp(sum / wsum, vec4<f32>(0.0), vec4<f32>(1.0));
        }
        default: {
            return textureSample(frame, samp_linear, uv);
        }
    }
}
"#;

/// GPU objects kept across frames in egui's callback resources.
struct Resources {
    pipeline: wgpu::RenderPipeline,
    srgb_target: bool,
    layout: wgpu::BindGroupLayout,
    sampler_linear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    texture: Option<(wgpu::Texture, u32, u32)>,
    bind_group: Option<wgpu::BindGroup>,
    /// Sequence number of the frame currently in the texture.
    uploaded_seq: u64,
}

/// Create the pipeline once and register it with the egui renderer.
pub fn init(render_state: &egui_wgpu::RenderState) {
    let device = &render_state.device;
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("psx display"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("psx display"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("psx display"),
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("psx display"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: render_state.target_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });
    let sampler = |filter: wgpu::FilterMode| {
        device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("psx display"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: filter,
            min_filter: filter,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        })
    };
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("psx display uniforms"),
        size: 32,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let resources = Resources {
        pipeline,
        srgb_target: render_state.target_format.is_srgb(),
        layout,
        sampler_linear: sampler(wgpu::FilterMode::Linear),
        sampler_nearest: sampler(wgpu::FilterMode::Nearest),
        uniforms,
        texture: None,
        bind_group: None,
        uploaded_seq: u64::MAX,
    };
    render_state
        .renderer
        .write()
        .callback_resources
        .insert(resources);
}

/// One frame's draw: uploads the pixels when they changed, then draws the
/// scaler quad into the callback rect.
pub struct DisplayCallback {
    /// RGBA8 pixels, `width * height * 4` bytes.
    pub rgba: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    /// Changes whenever `rgba` does, so unchanged frames are not re-uploaded.
    pub seq: u64,
    pub mode: ScaleMode,
    /// Size of the destination rect in physical pixels.
    pub dst_size: [f32; 2],
}

impl egui_wgpu::CallbackTrait for DisplayCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<Resources>() else {
            return Vec::new();
        };
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 || self.rgba.len() < (w * h * 4) as usize {
            return Vec::new();
        }
        let fits = matches!(&r.texture, Some((_, tw, th)) if *tw == w && *th == h);
        if !fits {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("psx frame"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            r.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("psx display"),
                layout: &r.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: r.uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&r.sampler_linear),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&r.sampler_nearest),
                    },
                ],
            }));
            r.texture = Some((texture, w, h));
            r.uploaded_seq = u64::MAX;
        }
        if r.uploaded_seq != self.seq
            && let Some((texture, _, _)) = &r.texture
        {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &self.rgba[..(w * h * 4) as usize],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            r.uploaded_seq = self.seq;
        }
        let mut bytes = [0u8; 32];
        for (i, v) in [w as f32, h as f32, self.dst_size[0], self.dst_size[1]]
            .iter()
            .enumerate()
        {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
        }
        bytes[16..20].copy_from_slice(&self.mode.index().to_ne_bytes());
        bytes[20..24].copy_from_slice(&u32::from(r.srgb_target).to_ne_bytes());
        queue.write_buffer(&r.uniforms, 0, &bytes);
        Vec::new()
    }

    fn paint(
        &self,
        _info: eframe::egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(r) = resources.get::<Resources>() else {
            return;
        };
        let Some(bind_group) = &r.bind_group else {
            return;
        };
        render_pass.set_pipeline(&r.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}
