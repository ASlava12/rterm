//! `GpuState` — the wgpu surface / device / queue plus the three render
//! layers (`bg` quads, inline `images`, `text`). Owns adapter + surface
//! init (with the WSL2 backend / present-mode overrides), resize,
//! opacity, and the per-frame `render` that sequences the bg-quad →
//! image → glyph → overlay passes. Extracted verbatim from `lib.rs`;
//! behaviour unchanged.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use winit::window::Window;

use crate::bg::{self, BgLayer};
use crate::image_pass;
use crate::{
    alpha_mode_is_transparent, flash_clear_color, is_wsl, palette, pick_alpha_mode, HeaderDraw,
    HeaderRightDraw, HeaderTabsDraw, HeaderTabsGhostDraw, OverlayDraw, PaneDraw, PreeditDraw,
    StatusBarDraw, TextLayer, PAD,
};

pub struct GpuState {
    pub text: TextLayer,
    bg: BgLayer,
    /// Inline-image pipeline. Owned alongside `bg` / `text`, drawn
    /// between the bg-quad pass and the overlay pass so images sit
    /// on top of cell backgrounds and under modal panels.
    images: image_pass::ImageLayer,
    clear_color: wgpu::Color,
    pub(crate) config: wgpu::SurfaceConfiguration,
    surface: wgpu::Surface<'static>,
    queue: wgpu::Queue,
    device: wgpu::Device,
    pub(crate) window: Arc<Window>,
}

impl GpuState {
    pub async fn new(
        window: Arc<Window>,
        font_size: f32,
        font_family: String,
        opacity: f32,
    ) -> Result<Self> {
        let size = window.inner_size();
        // Explicitly disable validation + the Vulkan loader's debug log
        // spam. `InstanceDescriptor::default()` enables `VALIDATION | DEBUG`
        // in debug builds, which has two real costs on Linux: (1) requests
        // `VK_LAYER_KHRONOS_validation` (typically not installed → a warn)
        // and (2) the loader pipes its layer/ICD enumeration to stderr at
        // INFO level — hundreds of lines before the window can open under
        // `cargo run`. We render glyphs, not GPU compute; validation isn't
        // load-bearing for us. End users can opt back in via `WGPU_DEBUG=1`
        // or `RUST_LOG=wgpu_hal=info` if they're chasing a render bug.
        let flags = match std::env::var("WGPU_DEBUG").ok().as_deref() {
            Some("1") | Some("true") => wgpu::InstanceFlags::debugging(),
            _ => wgpu::InstanceFlags::empty(),
        };
        // Backend selection: default to `all()` so GL is in the pool as
        // a fallback. `Backends::PRIMARY` excludes GL, which means on
        // platforms where Vulkan hangs/breaks (WSL2 mesa, headless
        // containers without ICDs) wgpu has nothing left to try and
        // the window never opens. `WGPU_BACKEND=vulkan|gl|dx12|metal`
        // honoured for explicit control.
        let backends = match std::env::var("WGPU_BACKEND").ok().as_deref() {
            Some("vulkan") | Some("vk") => wgpu::Backends::VULKAN,
            Some("gl") | Some("opengl") | Some("gles") => wgpu::Backends::GL,
            Some("metal") => wgpu::Backends::METAL,
            Some("dx12") => wgpu::Backends::DX12,
            Some("primary") => wgpu::Backends::PRIMARY,
            Some("secondary") => wgpu::Backends::SECONDARY,
            _ => {
                // WSL2 ships mesa drivers but Vulkan there frequently
                // stalls during instance init (no proper GPU pass-
                // through). Default to GL only so the window opens.
                // Users with working Vulkan can opt back in via
                // `WGPU_BACKEND=vulkan`.
                if is_wsl() {
                    tracing::info!(
                        "detected WSL2 — defaulting to GL backend \
                         (set WGPU_BACKEND=vulkan to override)",
                    );
                    wgpu::Backends::GL
                } else {
                    wgpu::Backends::all()
                }
            }
        };
        tracing::info!(?backends, "initialising wgpu instance");
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            flags,
            ..wgpu::InstanceDescriptor::default()
        });
        tracing::info!("wgpu instance created");
        let surface = instance.create_surface(window.clone()).context("create_surface")?;
        tracing::info!("requesting GPU adapter");
        // Prefer a hardware adapter; if the platform has none (CI runner,
        // bare-bones container, broken drivers), retry with the explicit
        // software fallback (llvmpipe on Linux, WARP on Windows) so the
        // user gets a working window instead of a hard "no compatible
        // GPU adapter" error.
        let adapter = match instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
        {
            Some(a) => a,
            None => {
                tracing::warn!(
                    "no hardware GPU adapter — falling back to software (slower)",
                );
                instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::default(),
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: true,
                    })
                    .await
                    .ok_or_else(|| anyhow!("no compatible GPU adapter, even with fallback"))?
            }
        };
        let info = adapter.get_info();
        // Log the full adapter identity, not just the friendly name.
        // When a user reports a render glitch the maintainers need
        // (vendor, device, driver, driver_info, device_type) to
        // correlate with known driver bugs (e.g. Qualcomm Adreno on
        // Windows-on-ARM has a different set of DX12 quirks than
        // Intel Arc on x86_64).
        tracing::info!(
            backend = ?info.backend,
            adapter = %info.name,
            vendor = format_args!("0x{:04x}", info.vendor),
            device = format_args!("0x{:04x}", info.device),
            device_type = ?info.device_type,
            driver = %info.driver,
            driver_info = %info.driver_info,
            "GPU adapter selected; requesting device",
        );
        // Take the adapter's full limits rather than `downlevel_defaults`.
        // The downlevel preset caps `max_texture_dimension_2d` at 2048,
        // which immediately fails `Surface::configure` when the user
        // maximises / fullscreens on any modern monitor (a 2560×1440
        // surface won't fit a 2048×2048 texture limit). Using the
        // adapter's own limits lets us track real hardware capability
        // (typically 8192–16384 on dGPU and 4096+ on iGPU); the surface
        // size is also clamped to those limits below, so a downlevel-
        // only adapter still gets a working — if letterboxed — surface
        // instead of a crash.
        let adapter_limits = adapter.limits();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("rterm-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: adapter_limits.clone(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .context("request_device")?;
        tracing::info!("GPU device ready");

        let caps = surface.get_capabilities(&adapter);
        // Echo the surface's full capability matrix so a render-
        // glitch log (Adreno on Windows-ARM, llvmpipe on WSL2,
        // ancient Intel iGPU, ...) shows what we actually had to
        // pick from. Without this the reader has to guess whether
        // the chosen format/present-mode/alpha-mode was first-
        // choice or a fallback after we filtered.
        tracing::info!(
            formats = ?caps.formats,
            present_modes = ?caps.present_modes,
            alpha_modes = ?caps.alpha_modes,
            "surface capabilities",
        );
        // Prefer an sRGB format; fall back to the first advertised one.
        // `caps.formats[0]` would panic on the (pathological) empty
        // list, so go through `first()` with a clear error instead.
        let format = match caps.formats.iter().copied().find(|f| f.is_srgb()) {
            Some(f) => f,
            None => *caps
                .formats
                .first()
                .ok_or_else(|| anyhow!("surface advertises no texture formats"))?,
        };
        // Pick an alpha-supporting composite mode when the user wants
        // transparency; otherwise stick with the platform default.
        let alpha_mode = pick_alpha_mode(opacity, &caps.alpha_modes);
        if opacity < 1.0 && !alpha_mode_is_transparent(alpha_mode) {
            // No alpha-capable mode → the surface composites opaque no
            // matter what we put in the clear alpha. Warn loudly rather
            // than silently ignoring the configured opacity, since the
            // usual cause is the environment (WSL2/WSLg, some GL
            // backends) and not the config.
            tracing::warn!(
                requested_opacity = opacity,
                available_alpha_modes = ?caps.alpha_modes,
                "window opacity < 1.0 requested but the GPU surface \
                 advertises no alpha-capable composite mode — the window \
                 will render OPAQUE. This is common on WSL2/WSLg and some \
                 GL backends; transparency needs a compositor that honours \
                 per-pixel window alpha (a native Wayland/X11 compositor, \
                 macOS, or Windows with DWM)."
            );
        }

        // Present mode: pick whatever the surface advertises that
        // matches our preference. `AutoVsync` is the right default
        // everywhere except WSL2, where Mesa's GL path can deadlock
        // waiting for vsync under heavy llvmpipe load — fall back to
        // `Fifo` there (still vsync-safe but with a different timing
        // path). `WGPU_PRESENT_MODE=fifo|mailbox|immediate|autovsync|autonovsync`
        // overrides for debugging.
        let preferred_present_mode = match std::env::var("WGPU_PRESENT_MODE")
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("fifo") => wgpu::PresentMode::Fifo,
            Some("mailbox") => wgpu::PresentMode::Mailbox,
            Some("immediate") => wgpu::PresentMode::Immediate,
            Some("autonovsync") => wgpu::PresentMode::AutoNoVsync,
            Some("autovsync") => wgpu::PresentMode::AutoVsync,
            _ => {
                if is_wsl() {
                    wgpu::PresentMode::Fifo
                } else {
                    wgpu::PresentMode::AutoVsync
                }
            }
        };
        let present_mode = if caps.present_modes.contains(&preferred_present_mode) {
            preferred_present_mode
        } else {
            // Surface doesn't expose what we wanted — first available
            // mode is always valid per the wgpu spec.
            caps.present_modes.first().copied().unwrap_or(wgpu::PresentMode::Fifo)
        };
        tracing::info!(?present_mode, "selected present mode");
        // Clamp the requested surface size to the adapter's
        // max_texture_dimension_2d. Some GPUs (and especially
        // virtualised Wayland/llvmpipe setups) advertise only 2048,
        // which means a 2560×1440 fullscreen surface would fail
        // validation. The clamp keeps configure() valid; the small
        // letterbox on hyper-conservative hardware is far better than
        // a crash.
        let max_dim = device.limits().max_texture_dimension_2d.max(1);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.clamp(1, max_dim),
            height: size.height.clamp(1, max_dim),
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        tracing::info!(
            format = ?format,
            requested = ?(size.width, size.height),
            configured = ?(config.width, config.height),
            max_dim,
            "configuring surface",
        );
        surface.configure(&device, &config);
        tracing::info!("building text layer");
        let mut text = TextLayer::new(&device, &queue, format, font_size, font_family);
        text.resize(&queue, config.width, config.height);
        // HiDPI layout diagnostic: the physical/logical numbers behind
        // scale-factor bugs (grid not filling the window, cursor/buttons
        // at the wrong x). Plain INFO so it shows without RUST_LOG.
        {
            let (cols, rows) = text.cells_for(config.width, config.height, PAD);
            tracing::info!(
                scale_factor = window.scale_factor(),
                inner_size = ?(size.width, size.height),
                surface = ?(config.width, config.height),
                cell_width = text.cell_width(),
                line_height = text.line_height(),
                grid = ?(cols, rows),
                "layout diagnostic",
            );
        }
        tracing::info!("building bg layer");
        let mut bg = BgLayer::new(&device, format);
        bg.resize(config.width, config.height);
        let mut images = image_pass::ImageLayer::new(&device, format);
        images.resize(config.width, config.height);
        tracing::info!("gpu state ready");

        // Default surface clear matches DEFAULT_BG; pre-multiply RGB if the
        // surface expects pre-multiplied alpha.
        let bg_rgb = palette::default_bg();
        let mut r = palette::srgb_byte_to_linear(bg_rgb[0]) as f64;
        let mut g = palette::srgb_byte_to_linear(bg_rgb[1]) as f64;
        let mut b = palette::srgb_byte_to_linear(bg_rgb[2]) as f64;
        let a = opacity as f64;
        if matches!(alpha_mode, wgpu::CompositeAlphaMode::PreMultiplied) {
            r *= a;
            g *= a;
            b *= a;
        }
        let clear_color = wgpu::Color { r, g, b, a };

        Ok(Self {
            surface,
            device,
            queue,
            config,
            window,
            clear_color,
            text,
            bg,
            images,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        // Same clamp as the initial configure(): keep the surface size
        // within the adapter's max_texture_dimension_2d, otherwise
        // wgpu's validator rejects fullscreen / maximise transitions on
        // downlevel adapters with a hard crash (observed on Wayland +
        // GNOME with mesa-software / llvmpipe-style 2048 limits).
        let max_dim = self.device.limits().max_texture_dimension_2d.max(1);
        let cw = w.min(max_dim);
        let ch = h.min(max_dim);
        if cw != w || ch != h {
            tracing::warn!(
                requested = ?(w, h),
                configured = ?(cw, ch),
                max_dim,
                "resize clamped to adapter max texture dimension",
            );
        }
        self.config.width = cw;
        self.config.height = ch;
        self.surface.configure(&self.device, &self.config);
        self.text.resize(&self.queue, cw, ch);
        self.bg.resize(cw, ch);
        self.images.resize(cw, ch);
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    /// Recompute the surface clear colour for a new opacity. The window's
    /// `with_transparent` hint and surface alpha mode are set at create
    /// time, so a runtime opacity change from 1.0 → <1.0 only visibly
    /// blends through to the desktop on compositors that allow alpha on
    /// originally-opaque surfaces. We still update the clear colour so
    /// the value is consistent if the window was created translucent.
    pub fn set_opacity(&mut self, opacity: f32) {
        let opacity = if opacity.is_finite() {
            opacity.clamp(0.0, 1.0) as f64
        } else {
            return;
        };
        let bg_rgb = palette::default_bg();
        let mut r = palette::srgb_byte_to_linear(bg_rgb[0]) as f64;
        let mut g = palette::srgb_byte_to_linear(bg_rgb[1]) as f64;
        let mut b = palette::srgb_byte_to_linear(bg_rgb[2]) as f64;
        if matches!(self.config.alpha_mode, wgpu::CompositeAlphaMode::PreMultiplied) {
            r *= opacity;
            g *= opacity;
            b *= opacity;
        }
        self.clear_color = wgpu::Color { r, g, b, a: opacity };
        self.window.request_redraw();
    }

    /// Submit a frame that just clears the surface to `clear_color` — no
    /// text, no bg-quad, nothing that touches `Terminal` data. Called on
    /// the very first `RedrawRequested` so that Wayland compositors see a
    /// committed buffer and respond with `configure` (which fires the
    /// `Resized` event that kicks the normal render loop). Without this
    /// the egg-and-chicken protocol stalls: client waits for configure to
    /// know its size, compositor waits for a frame to map the surface,
    /// and the window never appears. On X11/Win/Mac this is just a cheap
    /// extra frame.
    pub fn render_clear_only(&mut self) -> std::result::Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("rterm-clear-only") },
        );
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rterm-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        panes: &[PaneDraw<'_>],
        header: Option<&HeaderDraw<'_>>,
        header_right: Option<&HeaderRightDraw<'_>>,
        header_tabs: Option<&HeaderTabsDraw<'_>>,
        header_tabs_ghost: Option<&HeaderTabsGhostDraw<'_>>,
        status_bar: Option<&StatusBarDraw<'_>>,
        preedit: Option<&PreeditDraw<'_>>,
        overlay: Option<&OverlayDraw<'_>>,
        flash: f32,
        show_scrollbar: bool,
        before_panes: &[bg::BgQuad],
        after_panes: &[bg::BgQuad],
    ) -> std::result::Result<(), wgpu::SurfaceError> {
        let cell_w = self.text.cell_width();
        let line_h = self.text.line_height();

        let dim = overlay.map(|_| 0.65);
        static R1: std::sync::Once = std::sync::Once::new();
        R1.call_once(|| tracing::debug!("render: entering bg.prepare"));
        self.bg.prepare(
            &self.device,
            &self.queue,
            panes,
            cell_w,
            line_h,
            dim,
            show_scrollbar,
            before_panes,
            after_panes,
        );

        // Build the inline-image quad list. Walks every pane's
        // `image_placements()` and projects each placement's
        // absolute (scrollback ++ grid) row coords into the
        // visible viewport using the same `abs_row + offset -
        // sb_len` mapping the selection / search paths use. Quads
        // that fall outside the pane rect are silently skipped —
        // the renderer doesn't try to clip mid-quad on the GPU
        // side, so an image partially scrolled off-screen
        // disappears at the edge rather than getting cut on the
        // pane boundary. Acceptable for v1; a future pass could
        // emit per-row sub-quads to support smooth edge clipping.
        let mut image_quads: Vec<image_pass::ImageQuad> = Vec::new();
        let mut live_keys: std::collections::HashSet<image_pass::CacheKey> =
            std::collections::HashSet::new();
        for p in panes {
            let placements = p.terminal.image_placements();
            if placements.is_empty() {
                continue;
            }
            let on_alt = p.terminal.is_on_alt_screen();
            let sb_len = if on_alt {
                0
            } else {
                p.terminal.scrollback_len()
            };
            let grid_rows = p.terminal.size().rows as i64;
            for pl in placements {
                // Keep the texture (and any `failed` marker) cached for
                // EVERY placement that still exists, not just the ones
                // currently on screen. Registering the key before the
                // viewport cull means scrolling an image one row out of
                // view no longer frees its GPU texture and forces a
                // full re-decode (tens of ms) when it scrolls back —
                // and a permanently-corrupt payload isn't re-decoded
                // every time it re-enters view.
                let key = (p.pane_uid, pl.image_id);
                live_keys.insert(key);
                // Project absolute row into the viewport.
                let viewport_row =
                    pl.abs_row + p.scroll_offset as i64 - sb_len as i64;
                if viewport_row + pl.rows as i64 <= 0 {
                    continue; // fully above the visible window
                }
                if viewport_row >= grid_rows {
                    continue; // fully below
                }
                let pos = [
                    p.rect.left + pl.col as f32 * cell_w,
                    p.rect.top + viewport_row as f32 * line_h,
                ];
                let size = [pl.cols as f32 * cell_w, pl.rows as f32 * line_h];
                // One-shot trace per image so we can verify
                // projection math (abs_row → viewport_row, then
                // → pixel coords) is sane without spamming a
                // frame loop's worth of log lines.
                static FIRST_QUAD: std::sync::Once = std::sync::Once::new();
                FIRST_QUAD.call_once(|| {
                    tracing::info!(
                        pane_uid = p.pane_uid,
                        image_id = pl.image_id,
                        abs_row = pl.abs_row,
                        sb_len,
                        scroll_offset = p.scroll_offset,
                        viewport_row,
                        grid_rows,
                        pos_x = pos[0],
                        pos_y = pos[1],
                        size_w = size[0],
                        size_h = size[1],
                        pane_top = p.rect.top,
                        pane_height = p.rect.height,
                        "image_pass: first quad projection",
                    );
                });
                image_quads.push(image_pass::ImageQuad {
                    key,
                    pos,
                    size,
                    // Scissor to the owning pane's pixel rect so
                    // an image scrolled above its pane (or one
                    // taller than the pane height) doesn't paint
                    // into the header strip / status bar / a
                    // neighbouring pane.
                    clip: [p.rect.left, p.rect.top, p.rect.width, p.rect.height],
                });
            }
        }
        // GC textures for image ids that no longer have placements
        // (FIFO-evicted, RIS, or just panes that closed).
        self.images.sweep(&live_keys);
        // Closure that the image pass uses to fetch the source
        // bytes for a (pane_uid, image_id) pair. Walks the panes
        // to find the matching `Terminal`, then asks for its
        // image — we can't precompute a HashMap of images because
        // the renderer doesn't own the `Terminal` (the pane
        // mutex guards do, scoped to the closure).
        self.images.prepare(
            &self.device,
            &self.queue,
            &image_quads,
            |(pane_uid, image_id)| {
                panes
                    .iter()
                    .find(|p| p.pane_uid == pane_uid)
                    .and_then(|p| p.terminal.image(image_id))
                    .cloned()
            },
        );
        static R2: std::sync::Once = std::sync::Once::new();
        R2.call_once(|| tracing::debug!("render: entering text.prepare"));
        if let Err(e) = self.text.prepare(
            &self.device,
            &self.queue,
            panes,
            header,
            header_right,
            header_tabs,
            header_tabs_ghost,
            status_bar,
            preedit,
            overlay,
            (self.config.width, self.config.height),
        ) {
            tracing::warn!("text prepare failed: {e:#}");
        }
        static R3: std::sync::Once = std::sync::Once::new();
        R3.call_once(|| tracing::debug!("render: acquiring swapchain"));

        let output = self.surface.get_current_texture()?;
        static R4: std::sync::Once = std::sync::Once::new();
        R4.call_once(|| tracing::debug!("render: swapchain acquired"));
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("rterm-encoder") });
        {
            // For a brief bell flash, lift the clear color a touch so the
            // bell registers even on a fully-painted screen (see
            // `flash_clear_color` for the soft-neutral-pulse rationale).
            // `flash` is the fade intensity (1.0 at the BEL, easing to 0).
            let clear = if flash > 0.0 {
                flash_clear_color(self.clear_color, flash as f64)
            } else {
                self.clear_color
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rterm-main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Layered render so overlay panels sit ABOVE pane glyphs:
            //   1. bg.draw_main    — pane backgrounds, cursor, dim
            //   2. images.render   — inline image quads (iTerm2 /
            //                        Kitty), drawn over cell bg so
            //                        the underlying default-bg
            //                        colour doesn't show through
            //                        partially-transparent images
            //                        but BEFORE text so glyphs that
            //                        coincidentally overlap (which
            //                        shouldn't happen — parser
            //                        advances past image rows — but
            //                        is harmless if it does) sit
            //                        on top of the bitmap.
            //   3. text.render_main — pane + header glyphs
            //   4. bg.draw_overlay  — modal backdrop panels
            //   5. text.render_overlay — modal text
            // Without (4) sandwiched between (3) and (5) the overlay
            // panel ends up under both text passes and pane glyphs
            // bleed through the menu — the original visual bug.
            self.bg.draw_main(&mut pass);
            self.images.render(&mut pass);
            if let Err(e) = self.text.render_main(&mut pass) {
                tracing::warn!("text render main failed: {e:#}");
            }
            self.bg.draw_overlay(&mut pass);
            if let Err(e) = self.text.render_overlay(&mut pass) {
                tracing::warn!("text render overlay failed: {e:#}");
            }
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }
}
