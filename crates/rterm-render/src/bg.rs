//! Background-quad pass: paints solid coloured rectangles per cell before the
//! glyph pass, so cell `bg` colours and the cursor render as opaque blocks.

use bytemuck::{Pod, Zeroable};
use rterm_core::{Cell, CellAttrs, CursorShape};

use crate::palette::{color_to_rgb, cursor_color, default_bg, default_fg, rgb_to_linear_rgba};
use crate::PaneDraw;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
    /// Corner radius in pixels. `0.0` = sharp rectangle (the pane-cell
    /// path); positive values round the four corners using a signed
    /// distance field in the fragment shader.
    corner_radius: f32,
    _pad: [f32; 3],
}

/// Public free-form quad — caller-provided rectangle painted by the
/// background pass. Used for tab-bar fills, menu/overlay panels, and any
/// other UI chrome that wants a solid backdrop independent of the pane
/// cell grid. Color is **linear RGBA** (pre-converted from sRGB). Build
/// via `BgQuad::from_srgb` to avoid colour-space mistakes.
#[derive(Debug, Clone, Copy)]
pub struct BgQuad {
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub color: [f32; 4],
    /// Corner radius in pixels. `0.0` for plain rectangles.
    pub corner_radius: f32,
}

impl BgQuad {
    /// Convenience constructor that takes an sRGB byte triple +
    /// alpha and converts to the linear-space colour the shader
    /// expects.
    pub fn from_srgb(pos: [f32; 2], size: [f32; 2], rgb: [u8; 3], alpha: f32) -> Self {
        BgQuad {
            pos,
            size,
            color: rgb_to_linear_rgba(rgb, alpha.clamp(0.0, 1.0)),
            corner_radius: 0.0,
        }
    }

    /// Same as `from_srgb` but with rounded corners (pixel radius).
    pub fn from_srgb_rounded(
        pos: [f32; 2],
        size: [f32; 2],
        rgb: [u8; 3],
        alpha: f32,
        radius: f32,
    ) -> Self {
        BgQuad {
            pos,
            size,
            color: rgb_to_linear_rgba(rgb, alpha.clamp(0.0, 1.0)),
            corner_radius: radius.max(0.0),
        }
    }
}

impl From<BgQuad> for Instance {
    fn from(q: BgQuad) -> Instance {
        Instance {
            pos: q.pos,
            size: q.size,
            color: q.color,
            corner_radius: q.corner_radius,
            _pad: [0.0; 3],
        }
    }
}

impl Instance {
    /// Plain (sharp-cornered) instance — used by the cell-grid hot path
    /// where every Cell pushes a colored rectangle with no SDF math.
    #[inline]
    fn sharp(pos: [f32; 2], size: [f32; 2], color: [f32; 4]) -> Self {
        Instance { pos, size, color, corner_radius: 0.0, _pad: [0.0; 3] }
    }
}

const INITIAL_CAPACITY: u64 = 256;
const SHADER: &str = r#"
struct Uniforms {
    viewport: vec2<f32>,
    _pad:     vec2<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VOut {
    @builtin(position) clip:    vec4<f32>,
    @location(0)       color:   vec4<f32>,
    /// Pixel offset from the rectangle's centre — used by the
    /// fragment shader to compute the rounded-rect SDF.
    @location(1)       local:   vec2<f32>,
    /// Half-extent of the rectangle in pixels.
    @location(2)       half_sz: vec2<f32>,
    /// Corner radius in pixels (0 = sharp).
    @location(3)       radius:  f32,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) i_pos:    vec2<f32>,
    @location(1) i_size:   vec2<f32>,
    @location(2) i_color:  vec4<f32>,
    @location(3) i_radius: f32,
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
    out.clip    = vec4<f32>(ndc, 0.0, 1.0);
    out.color   = i_color;
    out.local   = (c - vec2<f32>(0.5)) * i_size;
    out.half_sz = i_size * 0.5;
    out.radius  = i_radius;
    return out;
}

fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    if (in.radius <= 0.5) {
        return in.color;
    }
    let r = min(in.radius, min(in.half_sz.x, in.half_sz.y));
    let d = sd_round_box(in.local, in.half_sz, r);
    // 1px-wide anti-alias band around the SDF zero crossing.
    let aa = clamp(0.5 - d, 0.0, 1.0);
    return vec4<f32>(in.color.rgb, in.color.a * aa);
}
"#;

pub struct BgLayer {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    capacity: u64,
    /// Number of instances in the "main" pane pass — drawn BEFORE the
    /// per-pane glyph pass so cell backgrounds sit underneath text.
    main_count: u32,
    /// Number of instances in the "overlay" pass — drawn AFTER the
    /// pane glyph pass so menu/settings panels visually cover any
    /// text that would otherwise bleed through. Stored back-to-back
    /// in the same buffer right after the main range.
    overlay_count: u32,
    viewport: [f32; 2],
    /// Reused per-frame staging buffer for the instance attributes.
    /// `prepare` clears + fills this rather than allocating a fresh `Vec`
    /// each frame (saves ~rows × cols × sizeof(Instance) of pressure on
    /// the allocator at 60 fps for a typical pane).
    instances: Vec<Instance>,
}

impl BgLayer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rterm-bg-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rterm-bg-bgl"),
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

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rterm-bg-uniform"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rterm-bg-bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rterm-bg-pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                // pos
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
                // size
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
                // color
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 2 },
                // corner_radius — single float, the trailing _pad is GPU-padding.
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32, offset: 32, shader_location: 3 },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rterm-bg-pipeline"),
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
            label: Some("rterm-bg-instances"),
            size: INITIAL_CAPACITY * std::mem::size_of::<Instance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            bind_group,
            uniform_buffer,
            instance_buffer,
            capacity: INITIAL_CAPACITY,
            main_count: 0,
            overlay_count: 0,
            viewport: [1.0, 1.0],
            instances: Vec::with_capacity(INITIAL_CAPACITY as usize),
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.viewport = [width.max(1) as f32, height.max(1) as f32];
    }

    /// Build instance buffer from one or more pane draws. Each pane gets its
    /// own (cell_w, line_h) — typically uniform for monospace, but kept per
    /// pane for future flexibility. `dim_alpha` (when Some) appends a
    /// full-viewport black quad with the given alpha — used to dim the
    /// background behind modal overlays like the help cheat-sheet.
    ///
    /// Free-quad layers:
    /// - `before_panes` — drawn before any pane cell; use for tab-bar
    ///   backgrounds, header strips, anything that should sit BEHIND
    ///   pane content.
    /// - `after_panes` — drawn AFTER pane cells and the dim alpha so
    ///   menu / overlay panels appear as solid cards rather than
    ///   semi-transparent text mashed into terminal output.
    // `prepare` is the sole render-side hot path; collapsing its args
    // into a struct would just add an indirection without changing the
    // call site count.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        panes: &[PaneDraw<'_>],
        cell_w: f32,
        line_h: f32,
        dim_alpha: Option<f32>,
        show_scrollbar: bool,
        before_panes: &[BgQuad],
        after_panes: &[BgQuad],
    ) {
        // Reuse the cached Vec across frames — `take` lifts it out so the
        // body can keep mutating it without holding a borrow against
        // `self` (we touch `self.uniforms()` further down). Cleared so
        // capacity survives; restored before exit.
        let mut instances = std::mem::take(&mut self.instances);
        instances.clear();
        // Background strip layer — under everything else.
        for q in before_panes {
            instances.push((*q).into());
        }
        for p in panes {
            let rows = p.terminal.size().rows;
            let reverse_screen = p.terminal.is_reverse_screen();
            // The cursor "blinks" only when the terminal asked it to;
            // steady shapes (DECSCUSR 2/4/6) ignore the App's blink phase.
            let blink_state = p.blink_on || !p.terminal.cursor_should_blink();
            let cursor_active = p.focused
                && p.scroll_offset == 0
                && p.terminal.cursor_visible()
                && blink_state;
            let cursor = p.terminal.cursor();
            // Clamp the cursor column to the last visible cell for rendering.
            // After printing at column `size.cols - 1` the terminal leaves the
            // logical cursor at `size.cols` (the "pending wrap" state). The
            // row only has `size.cols` cells, so without clamping no cell
            // matches `cursor.col` and the cursor briefly disappears.
            let cursor_col_render =
                cursor.col.min(p.terminal.size().cols.saturating_sub(1));
            let cursor_shape = p.terminal.cursor_shape();
            for r in 0..rows {
                let Some(row) = p.terminal.visible_row(p.scroll_offset, r) else { continue };
                for (c, cell) in row.iter().enumerate() {
                    // Cursor sits on the WIDE half (cursor.col) but the
                    // following WIDE_SPACER cell needs the same inversion
                    // so a CJK / emoji glyph under the block cursor shows
                    // a contiguous cursor across both halves instead of
                    // splitting the inversion mid-glyph.
                    let is_cursor = cursor_active
                        && cursor.row == r
                        && (cursor_col_render as usize == c
                            || (c > 0
                                && cursor_col_render as usize == c - 1
                                && cell.attrs.contains(CellAttrs::WIDE_SPACER)));
                    // Selection highlight follows the same "WIDE_SPACER
                    // inherits its left-half neighbour's state" rule as the
                    // cursor block, otherwise a selection ending mid-CJK
                    // visibly chops the glyph in half.
                    let is_selected = p
                        .selection
                        .as_ref()
                        .map(|s| {
                            s.contains(r, c as u16)
                                || (c > 0
                                    && cell.attrs.contains(CellAttrs::WIDE_SPACER)
                                    && s.contains(r, (c - 1) as u16))
                        })
                        .unwrap_or(false);

                    let cell_x = p.rect.left + c as f32 * cell_w;
                    let cell_y = p.rect.top + r as f32 * line_h;

                    // Base cell background (selection or explicit bg colour).
                    // For Block cursor the cursor-inverted variant is already
                    // returned by cell_bg_color; for thin shapes we want the
                    // *normal* bg underneath and the cursor stripe on top.
                    let bg_for_layer = if matches!(cursor_shape, CursorShape::Block) {
                        cell_bg_color(cell, is_cursor, is_selected, reverse_screen)
                    } else {
                        cell_bg_color(cell, false, is_selected, reverse_screen)
                    };
                    if let Some(color) = bg_for_layer {
                        instances.push(Instance::sharp([cell_x, cell_y], [cell_w, line_h], color));
                    }

                    // Thin cursor shapes get an overlay strip in fg colour.
                    if is_cursor && !matches!(cursor_shape, CursorShape::Block) {
                        // DECSCNM swap parity with `cell_bg_color`
                        // and `StyleKey::from_cell`: REVERSE attr
                        // XOR reverse_screen.
                        let reverse =
                            cell.attrs.contains(CellAttrs::REVERSE) ^ reverse_screen;
                        let fg_term = if reverse { cell.bg } else { cell.fg };
                        let fg_fallback = if reverse { default_bg() } else { default_fg() };
                        let rgb = p
                            .terminal
                            .cursor_color()
                            .or_else(cursor_color)
                            .unwrap_or_else(|| color_to_rgb(fg_term, fg_fallback));
                        let color = rgb_to_linear_rgba(rgb, 1.0);
                        let (cx, cy, cw, ch) = match cursor_shape {
                            CursorShape::Underline => {
                                let h = (line_h * 0.15).clamp(2.0, 3.0);
                                (cell_x, cell_y + line_h - h, cell_w, h)
                            }
                            CursorShape::Bar => {
                                let w = (cell_w * 0.15).clamp(2.0, 3.0);
                                (cell_x, cell_y, w, line_h)
                            }
                            // `Block` is excluded by the outer `if
                            // !matches!(.., Block)` gate; any future
                            // CursorShape variant added to rterm-core
                            // would land here. Render it as a thin
                            // underline (the closest unobtrusive shape)
                            // rather than `unreachable!()`, which would
                            // panic in the render hot path.
                            _ => {
                                let h = (line_h * 0.15).clamp(2.0, 3.0);
                                (cell_x, cell_y + line_h - h, cell_w, h)
                            }
                        };
                        instances.push(Instance::sharp([cx, cy], [cw, ch], color));
                    }

                    // SGR text decorations — drawn UNDER the hyperlink stripe
                    // so an OSC 8 link inside an underlined span still shows
                    // its accent blue. DECSCNM mirrors the same XOR swap so
                    // a decoration on a reverse-screen cell paints the
                    // visually-correct colour rather than the original fg.
                    let reverse =
                        cell.attrs.contains(CellAttrs::REVERSE) ^ reverse_screen;
                    let fg_term = if reverse { cell.bg } else { cell.fg };
                    let fg_fallback = if reverse { default_bg() } else { default_fg() };
                    let fg_rgb = color_to_rgb(fg_term, fg_fallback);
                    if cell.attrs.contains(CellAttrs::UNDERLINE) && !is_cursor && !is_selected {
                        let h = (line_h * 0.08).clamp(1.0, 2.0);
                        let base_y = cell_y + line_h - h - 1.0;
                        let color = rgb_to_linear_rgba(fg_rgb, 1.0);
                        if cell.attrs.contains(CellAttrs::UNDERLINE_DOUBLE) {
                            // Two thin parallel stripes.
                            instances.push(Instance::sharp([cell_x, base_y - h - 1.0], [cell_w, h], color));
                            instances.push(Instance::sharp([cell_x, base_y], [cell_w, h], color));
                        } else if cell.attrs.contains(CellAttrs::UNDERLINE_CURLY) {
                            // Approximate a wavy underline with a dotted row
                            // of short dashes alternating in vertical phase.
                            // Cheap but readable on retina-class scales.
                            let seg_w = (cell_w / 4.0).max(1.0);
                            for k in 0..4 {
                                let dx = cell_x + seg_w * k as f32;
                                let dy = if k % 2 == 0 { base_y } else { base_y - h };
                                instances.push(Instance::sharp([dx, dy], [seg_w, h], color));
                            }
                        } else {
                            instances.push(Instance::sharp([cell_x, base_y], [cell_w, h], color));
                        }
                    }
                    if cell.attrs.contains(CellAttrs::STRIKETHROUGH) && !is_cursor && !is_selected {
                        let h = (line_h * 0.08).clamp(1.0, 2.0);
                        instances.push(Instance::sharp(
                            [cell_x, cell_y + line_h * 0.5 - h * 0.5],
                            [cell_w, h],
                            rgb_to_linear_rgba(fg_rgb, 1.0),
                        ));
                    }
                    if cell.attrs.contains(CellAttrs::OVERLINE) && !is_cursor && !is_selected {
                        let h = (line_h * 0.08).clamp(1.0, 2.0);
                        instances.push(Instance::sharp(
                            [cell_x, cell_y],
                            [cell_w, h],
                            rgb_to_linear_rgba(fg_rgb, 1.0),
                        ));
                    }

                    // Hyperlink underline (skip if cell is cursor/selection).
                    if cell.hyperlink != 0 && !is_cursor && !is_selected {
                        let stripe_h = (line_h * 0.08).clamp(1.0, 2.0);
                        instances.push(Instance::sharp(
                            [cell_x, cell_y + line_h - stripe_h],
                            [cell_w, stripe_h],
                            rgb_to_linear_rgba([86, 156, 214], 1.0),
                        ));
                    }
                }
            }
        }

        // Per-pane scrollbar indicator on the right edge. Visible only when
        // the pane has scrollback; brighter when the user is scrolled into
        // history. Width is a fixed 2 px so it never steals cell space.
        // `show_scrollbar` (config) gates the entire pass.
        if show_scrollbar {
            for p in panes {
                // Alt-screen apps (vim, less, htop) own the whole viewport;
                // primary's scrollback isn't reachable while alt is active,
                // so an indicator there would just be visual noise.
                if p.terminal.is_on_alt_screen() {
                    continue;
                }
                let total = p.terminal.scrollback_len();
                if total == 0 {
                    continue;
                }
                let track_w = 2.0_f32;
                let track_x = p.rect.left + p.rect.width - track_w;
                let track_y = p.rect.top;
                let track_h = p.rect.height;
                if track_h < 2.0 || track_w <= 0.0 {
                    continue;
                }
                let scrolled = p.scroll_offset > 0;
                let track_color: [f32; 4] = if scrolled {
                    [0.20, 0.22, 0.28, 0.55]
                } else {
                    [0.18, 0.20, 0.26, 0.25]
                };
                instances.push(Instance::sharp(
                    [track_x, track_y],
                    [track_w, track_h],
                    track_color,
                ));
                // Thumb represents the visible window over (scrollback+grid).
                let rows = p.terminal.size().rows as f32;
                let view_total = total as f32 + rows;
                let view_top = (total as f32) - p.scroll_offset as f32;
                let thumb_top = (view_top / view_total).clamp(0.0, 1.0);
                let thumb_h = (rows / view_total).clamp(0.05, 1.0);
                let ty = track_y + thumb_top * track_h;
                let th = (thumb_h * track_h).max(8.0).min(track_h);
                let thumb_color: [f32; 4] = if scrolled {
                    [0.85, 0.78, 0.42, 0.85]
                } else {
                    [0.55, 0.58, 0.66, 0.55]
                };
                instances.push(Instance::sharp(
                    [track_x, ty],
                    [track_w, th],
                    thumb_color,
                ));
            }
        }

        // Full-viewport dim overlay belongs to the MAIN pass — drawn
        // after all cell content but before pane glyphs, so the dim
        // sits under both panes and overlay text.
        if let Some(a) = dim_alpha {
            instances.push(Instance::sharp(
                [0.0, 0.0],
                self.viewport,
                [0.0, 0.0, 0.0, a.clamp(0.0, 1.0)],
            ));
        }
        // Everything pushed so far lives in the "main" pass — drawn
        // BEFORE the pane glyph pass. Record the split point.
        let main_count = instances.len() as u32;
        // Overlay panels start the "overlay" range. These quads are
        // drawn AFTER pane glyphs so the menu/settings/context-menu
        // backdrop visually covers any pane text under it.
        for q in after_panes {
            instances.push((*q).into());
        }

        self.main_count = main_count;
        self.overlay_count = instances.len() as u32 - main_count;
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&self.uniforms()));

        if instances.is_empty() {
            // Put the empty Vec back so its capacity is reused next frame.
            self.instances = instances;
            return;
        }

        let needed = instances.len() as u64;
        if needed > self.capacity {
            // Allocate at the rounded-up capacity, NOT the current `needed`.
            // The previous version used `create_buffer_init` with the live
            // `instances` slice as contents, so the buffer's actual byte
            // size was `instances.len() * sizeof(Instance)` even though
            // `self.capacity` advertised a larger `next_power_of_two`. A
            // subsequent frame with `instances.len() > prev_len` but still
            // `<= self.capacity` would take the cheap `write_buffer` path
            // and overrun the under-sized GPU buffer (wgpu validation
            // panics with "Copy of 0..N would end up overrunning ...").
            let new_cap = needed.next_power_of_two().max(INITIAL_CAPACITY);
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rterm-bg-instances"),
                size: new_cap * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
        }
        queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
        self.instances = instances;
    }

    fn uniforms(&self) -> Uniforms {
        Uniforms { viewport: self.viewport, _pad: [0.0, 0.0] }
    }

    /// Draw the MAIN range — pane cell backgrounds, cursor, selection,
    /// SGR decorations, scrollbar, dim. Call this BEFORE the pane text
    /// pass.
    pub fn draw_main<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) {
        if self.main_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        pass.draw(0..6, 0..self.main_count);
    }

    /// Draw the OVERLAY range — modal panel backdrops (settings,
    /// context-menu, palette, help, rename). Call this AFTER the pane
    /// text pass, BEFORE the overlay text pass, so menu chrome covers
    /// pane glyphs that would otherwise bleed through.
    pub fn draw_overlay<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) {
        if self.overlay_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        pass.draw(0..6, self.main_count..self.main_count + self.overlay_count);
    }
}

/// Background colour for a cell as linear RGBA. Returns `None` when the cell
/// would be drawn on the clear colour anyway (default bg, no cursor, no sel).
fn cell_bg_color(
    cell: &Cell,
    is_cursor: bool,
    is_selected: bool,
    reverse_screen: bool,
) -> Option<[f32; 4]> {
    // `REVERSE` attr and DECSCNM ?5 both invert. Even count
    // cancels (XOR), matching the StyleKey path in lib.rs so
    // the fg-text and bg-fill agree on which is "on top".
    let reverse =
        cell.attrs.contains(CellAttrs::REVERSE) ^ reverse_screen;
    // After REVERSE swap, the on-screen background is the cell's fg.
    let bg_term = if reverse { cell.fg } else { cell.bg };
    let fg_term = if reverse { cell.bg } else { cell.fg };

    // The Default fallback for `fg_term` depends on whether
    // reverse is in effect: under reverse, `fg_term = cell.bg`,
    // and a `Default` value there means "the original default
    // background color" → resolve with `default_bg()` so the
    // cursor / selection block actually inverts visually
    // instead of collapsing to default_fg() on both sides.
    let fg_fallback = if reverse { default_bg() } else { default_fg() };
    if is_cursor {
        // Cursor block uses the config-supplied cursor colour when set,
        // otherwise the cell's foreground (xterm-style inversion).
        let rgb = cursor_color().unwrap_or_else(|| color_to_rgb(fg_term, fg_fallback));
        return Some(rgb_to_linear_rgba(rgb, 1.0));
    }

    if is_selected {
        // Selection: invert — show what fg would have been as a solid block.
        let rgb = color_to_rgb(fg_term, fg_fallback);
        return Some(rgb_to_linear_rgba(rgb, 1.0));
    }

    match bg_term {
        rterm_core::Color::Default => {
            // With REVERSE / DECSCNM active, "default bg" must
            // still paint a real colour (default_fg) so the
            // cell visibly inverts. Without reverse the cell
            // falls through to the global clear colour as
            // before — that's the cheap path for the typical
            // grid where most cells use default bg.
            if reverse {
                let rgb = default_fg();
                Some(rgb_to_linear_rgba(rgb, 1.0))
            } else {
                None
            }
        }
        other => {
            let rgb = color_to_rgb(other, default_bg());
            Some(rgb_to_linear_rgba(rgb, 1.0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a Cell with the given fg/bg/attrs.
    fn cell_with(fg: rterm_core::Color, bg: rterm_core::Color, attrs: CellAttrs) -> Cell {
        Cell {
            fg,
            bg,
            attrs,
            ..Cell::default()
        }
    }

    #[test]
    fn cell_bg_color_paints_default_under_reverse_screen() {
        // A default-fg / default-bg cell normally falls through
        // to the global clear colour (returns None). With
        // DECSCNM (?5) set, it must instead paint default_fg so
        // the cell visibly inverts.
        let c = cell_with(
            rterm_core::Color::Default,
            rterm_core::Color::Default,
            CellAttrs::empty(),
        );
        // No reverse — None.
        assert!(cell_bg_color(&c, false, false, false).is_none());
        // Reverse on — Some, with default_fg color.
        let painted = cell_bg_color(&c, false, false, true)
            .expect("reverse cell must paint");
        let fg_rgb = default_fg();
        let expected = rgb_to_linear_rgba(fg_rgb, 1.0);
        assert_eq!(painted, expected);
    }

    #[test]
    fn cell_bg_color_reverse_attr_xor_reverse_screen_cancels() {
        // A cell with SGR REVERSE set AND DECSCNM ?5 active
        // should render the same as a plain cell — two
        // inversions cancel. Pin this so the XOR semantics
        // stay consistent with the glyph path in lib.rs.
        let plain = cell_with(
            rterm_core::Color::Default,
            rterm_core::Color::Default,
            CellAttrs::empty(),
        );
        let double_reverse = cell_with(
            rterm_core::Color::Default,
            rterm_core::Color::Default,
            CellAttrs::REVERSE,
        );
        // Plain cell, no reverse → None (default fall-through).
        assert!(cell_bg_color(&plain, false, false, false).is_none());
        // Plain cell + reverse_screen → Some (cell paints).
        assert!(cell_bg_color(&plain, false, false, true).is_some());
        // REVERSE attr alone → Some (acts as if reversed).
        assert!(cell_bg_color(&double_reverse, false, false, false).is_some());
        // REVERSE attr + reverse_screen → both XOR-cancel → None.
        assert!(
            cell_bg_color(&double_reverse, false, false, true).is_none(),
            "double inversion must cancel back to fall-through",
        );
    }

    #[test]
    fn cell_bg_color_cursor_inverts_correctly_under_reverse_screen() {
        // On a default cell with reverse_screen, the cursor block
        // should paint default_bg (the "new" fg colour under
        // inversion). Without the visibility fix this returned
        // default_fg, colliding with the inverted background.
        let c = cell_with(
            rterm_core::Color::Default,
            rterm_core::Color::Default,
            CellAttrs::empty(),
        );
        let painted = cell_bg_color(&c, true, false, true)
            .expect("cursor block must paint");
        // If a fixed cursor_color is configured we can't predict
        // the value; without one, expect default_bg.
        if cursor_color().is_none() {
            let expected = rgb_to_linear_rgba(default_bg(), 1.0);
            assert_eq!(painted, expected, "cursor must use default_bg under reverse");
        }
    }
}
