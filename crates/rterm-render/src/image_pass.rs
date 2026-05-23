//! Inline-image GPU pass: textured quads for the iTerm2 / Kitty
//! protocol payloads.
//!
//! Architecture
//! -----------
//!
//! Mirrors the bg-quad pipeline in [`crate::bg`] but binds a
//! per-image (texture + sampler) bind group at slot 1 so the
//! fragment shader can sample the source bitmap. Instances are
//! batched by `image_id` — the renderer walks each pane's
//! [`Terminal::image_placements`], looks up (or lazily decodes +
//! uploads) the matching `wgpu::Texture`, and emits one draw call
//! per distinct image with all placements of that image as
//! instances of the shared quad.
//!
//! Cache
//! -----
//!
//! [`ImageLayer`] owns a `HashMap<image_id, ImageCacheEntry>`. The
//! first time an `image_id` shows up in a frame, we decode the
//! bytes via [`crate::image_decode::decode`], create the texture +
//! view + bind group, and stash it. Subsequent frames bind the
//! existing entry directly — texture data never re-uploads.
//!
//! A separate `failed: HashSet<image_id>` records ids that couldn't
//! be decoded (corrupt PNG, oversized RGBA, etc.) so we don't burn
//! CPU re-trying every frame.
//!
//! Lifetime + invalidation
//! -----------------------
//!
//! The cache uses the image's `id` as the key; ids are
//! monotonically assigned by `Terminal::register_image` and don't
//! collide with FIFO-evicted entries (next_image_id only ever
//! increments). When a terminal evicts an image, the placement
//! disappears too, so the renderer just stops emitting draws for
//! the now-orphaned cache entry. The entries themselves get
//! garbage-collected by [`ImageLayer::sweep`], called per-frame
//! against the set of currently-referenced ids.
//!
use std::collections::{HashMap, HashSet};

use bytemuck::{Pod, Zeroable};
use rterm_core::Image;

use crate::image_decode;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Instance {
    /// Top-left pixel position of the image rect.
    pos: [f32; 2],
    /// Width / height of the rect in pixels (cell footprint × cell
    /// metrics, computed per-frame by the caller).
    size: [f32; 2],
}

/// Globally-unique cache key for a single image instance. Composed
/// of `(pane_uid, image_id)` because every pane's Terminal owns its
/// own monotonic `image_id` counter — IDs collide across panes
/// otherwise (pane A's image 1 != pane B's image 1).
pub type CacheKey = (u64, u64);

/// One textured rectangle the renderer wants to draw. Built by the
/// App layer from a `(pane_rect, image_placement, cell_metrics)`
/// triple — the placement itself stays in `rterm-core` and doesn't
/// know about pixel space.
#[derive(Debug, Clone, Copy)]
pub struct ImageQuad {
    /// `(pane_uid, image_id)` — the cache key the texture lives
    /// under. The renderer doesn't see `image_id` in isolation;
    /// see [`CacheKey`].
    pub key: CacheKey,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

const INITIAL_CAPACITY: u64 = 16;

const SHADER: &str = r#"
struct Uniforms {
    viewport: vec2<f32>,
    _pad:     vec2<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(1) @binding(0) var tex:     texture_2d<f32>;
@group(1) @binding(1) var samp:    sampler;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0)       uv:   vec2<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) i_pos:  vec2<f32>,
    @location(1) i_size: vec2<f32>,
) -> VOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let c = corners[vi];
    let pixel = i_pos + c * i_size;
    let ndc = vec2<f32>(
        pixel.x / u.viewport.x *  2.0 - 1.0,
        1.0 - pixel.y / u.viewport.y *  2.0,
    );
    var out: VOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.uv   = c;
    return out;
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

struct ImageCacheEntry {
    /// Owned GPU texture. Not read directly — the bind group below
    /// holds the texture view that the fragment shader samples. The
    /// field exists to extend the texture's lifetime to match its
    /// view; dropping it would invalidate the bind group between
    /// the prepare and render passes.
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct ImageLayer {
    pipeline: wgpu::RenderPipeline,
    viewport_bind_group: wgpu::BindGroup,
    viewport_buffer: wgpu::Buffer,
    sampler: wgpu::Sampler,
    image_bgl: wgpu::BindGroupLayout,
    cache: HashMap<CacheKey, ImageCacheEntry>,
    /// Keys we already tried to decode and failed on. Avoids
    /// burning CPU on the same bad payload every frame.
    failed: HashSet<CacheKey>,
    instance_buffer: wgpu::Buffer,
    capacity: u64,
    /// Per-frame draw groups: `(key, [first..last])` index ranges
    /// into `instance_buffer`. One draw call per group; each call
    /// binds the matching `ImageCacheEntry`'s bind group and
    /// instances the shared quad.
    groups: Vec<(CacheKey, std::ops::Range<u32>)>,
    viewport: [f32; 2],
    /// Scratch buffer reused per-frame for the staging fill of
    /// `instance_buffer` — same trick `BgLayer` uses to avoid
    /// allocating a fresh `Vec<Instance>` on every prepare().
    instances: Vec<Instance>,
}

impl ImageLayer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rterm-image-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        // Group 0: viewport uniform (shared across draws).
        let viewport_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rterm-image-viewport-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        // Group 1: per-image (texture + sampler). Re-used across
        // every image's bind group; layouts can be shared even
        // when the bindings differ.
        let image_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rterm-image-tex-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rterm-image-viewport-uniform"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rterm-image-viewport-bg"),
            layout: &viewport_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            }],
        });

        // Shared bilinear sampler — same one rebinds for every
        // image cache entry. Linear filtering keeps downscaled
        // thumbnails readable without per-image MIP levels.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rterm-image-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rterm-image-pl"),
            bind_group_layouts: &[&viewport_bgl, &image_bgl],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rterm-image-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[instance_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rterm-image-instances"),
            size: INITIAL_CAPACITY * std::mem::size_of::<Instance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            viewport_bind_group,
            viewport_buffer,
            sampler,
            image_bgl,
            cache: HashMap::new(),
            failed: HashSet::new(),
            instance_buffer,
            capacity: INITIAL_CAPACITY,
            groups: Vec::new(),
            viewport: [1.0, 1.0],
            instances: Vec::with_capacity(INITIAL_CAPACITY as usize),
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.viewport = [width.max(1) as f32, height.max(1) as f32];
    }

    /// Drop any cached entries whose `image_id` isn't in
    /// `live_ids`. Called once per frame to GC textures for images
    /// the terminal has evicted (via FIFO cap, RIS, or future
    /// Kitty `a=d` delete actions). Re-uploading on next reference
    /// is a millisecond-scale cost, well below the per-frame
    /// budget, so we don't try to soft-evict here.
    pub fn sweep(&mut self, live: &HashSet<CacheKey>) {
        self.cache.retain(|k, _| live.contains(k));
        self.failed.retain(|k| live.contains(k));
    }

    /// Look up — or lazily upload — the GPU resources for an
    /// image. Returns `false` when the upload failed and the
    /// caller should skip drawing this quad. Cached entries hit
    /// the fast path; first-use does the decode + texture
    /// creation.
    fn ensure_uploaded(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: CacheKey,
        image: &Image,
    ) -> bool {
        if self.cache.contains_key(&key) {
            return true;
        }
        if self.failed.contains(&key) {
            return false;
        }
        let decoded = match image_decode::decode(image) {
            Some(d) => d,
            None => {
                self.failed.insert(key);
                return false;
            }
        };
        // Refuse degenerate dimensions — wgpu requires at least 1×1.
        if decoded.width == 0 || decoded.height == 0 {
            self.failed.insert(key);
            return false;
        }
        let extent = wgpu::Extent3d {
            width: decoded.width,
            height: decoded.height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rterm-image-texture"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // The image-crate decoder produces non-sRGB bytes; the
            // surface itself is sRGB, so the sample-to-target
            // conversion happens via the swap-chain's view format.
            // Using `Rgba8UnormSrgb` here would double-correct and
            // wash the image out.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &decoded.rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(decoded.width * 4),
                rows_per_image: Some(decoded.height),
            },
            extent,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rterm-image-bg"),
            layout: &self.image_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.cache.insert(key, ImageCacheEntry { _texture: texture, bind_group });
        true
    }

    /// Build the per-frame instance buffer from a flat list of
    /// quads. `images_for` is a callback the App layer passes in so
    /// this module doesn't need a `&Terminal` borrow (which would
    /// fight the renderer's pane-locking strategy). The closure
    /// returns the source bytes for a given id; `None` means the
    /// quad is silently dropped.
    pub fn prepare<F>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &[ImageQuad],
        mut image_for: F,
    ) where
        F: FnMut(CacheKey) -> Option<Image>,
    {
        // Reset per-frame state. Pull the scratch Vec out so the
        // ensure_uploaded path can keep &mut self for the cache.
        let mut instances = std::mem::take(&mut self.instances);
        instances.clear();
        self.groups.clear();
        if quads.is_empty() {
            self.instances = instances;
            self.write_uniforms(queue);
            return;
        }

        // Group quads by cache key, preserving first-occurrence
        // order so multi-pane streams don't accidentally interleave
        // (which would force separate draw calls and lose batching).
        let mut by_key: HashMap<CacheKey, Vec<&ImageQuad>> = HashMap::new();
        let mut order: Vec<CacheKey> = Vec::new();
        for q in quads {
            let entry = by_key.entry(q.key).or_default();
            if entry.is_empty() {
                order.push(q.key);
            }
            entry.push(q);
        }

        for key in &order {
            // Upload (or reuse cache) for this image. If decode
            // fails, the key stays in `failed` and is silently
            // skipped from now on.
            let img = match image_for(*key) {
                Some(i) => i,
                None => continue,
            };
            if !self.ensure_uploaded(device, queue, *key, &img) {
                continue;
            }
            let start = instances.len() as u32;
            for q in by_key.get(key).into_iter().flatten() {
                instances.push(Instance { pos: q.pos, size: q.size });
            }
            let end = instances.len() as u32;
            if end > start {
                self.groups.push((*key, start..end));
            }
        }

        // Grow the instance buffer in 2× steps when the per-frame
        // count blows past the current capacity. Capacity persists
        // across frames so we don't churn allocations during
        // steady-state.
        let needed = instances.len() as u64;
        if needed > self.capacity {
            let mut new_cap = self.capacity.max(1);
            while new_cap < needed {
                new_cap *= 2;
            }
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rterm-image-instances"),
                size: new_cap * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
        }
        if !instances.is_empty() {
            queue.write_buffer(
                &self.instance_buffer,
                0,
                bytemuck::cast_slice(&instances),
            );
        }
        self.instances = instances;
        self.write_uniforms(queue);
    }

    fn write_uniforms(&self, queue: &wgpu::Queue) {
        let uniforms = Uniforms {
            viewport: self.viewport,
            _pad: [0.0; 2],
        };
        queue.write_buffer(&self.viewport_buffer, 0, bytemuck::bytes_of(&uniforms));
    }

    /// Execute the recorded draws. Caller has already begun the
    /// render pass and set up the colour attachment.
    pub fn render<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        if self.groups.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.viewport_bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        for (key, range) in &self.groups {
            let Some(entry) = self.cache.get(key) else { continue };
            pass.set_bind_group(1, &entry.bind_group, &[]);
            pass.draw(0..6, range.clone());
        }
    }

}
