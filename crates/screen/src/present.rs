//! GPU presenter: attaches a wgpu surface to an existing window (anything
//! implementing raw-window-handle 0.6) and draws the post-LUT frame through
//! the band-limited grid/scaling pass.
//!
//! Pacing: emulation speed is owned by the audio clock, never the display.
//! A 60/120/144 Hz panel, ProMotion/G-Sync/FreeSync, or a throttling
//! compositor must not change game speed; the presenter just shows the
//! latest frame whenever it's asked to. Vsync is therefore a pure
//! PRESENTATION choice (tear-free vs lowest latency): off =
//! PresentMode::AutoNoVsync, on = PresentMode::AutoVsync, switchable live
//! via [`Presenter::set_vsync`] — neither setting ever paces the emulator.
//!
//! Color: the surface preferentially is 8-bit UNORM and receives
//! display-encoded bytes (the LUT already encoded for the chosen target).
//! Some GL/Vulkan/compositor combinations only expose *Srgb surface
//! formats; on those the surface view re-encodes whatever we write, so the
//! shader pre-applies the inverse (the exact piecewise sRGB EOTF) to its
//! final color — the hardware re-encode then reproduces the intended
//! bytes (flag in the params buffer, slot 13). On macOS we tag the
//! underlying layer with the matching colorspace so the compositor
//! color-matches our output to the actual panel — that is what makes the
//! simulation hold on wide-gamut (P3) laptop screens without us measuring
//! anything. On platforms without compositor color management the sRGB
//! assumption applies (with a manual wide-gamut override in the config).

use std::sync::Arc;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::profile::DisplayTarget;

/// Per-frame grid/scaling parameters; resolved from config + screen kind.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GridParams {
    /// 0 = plain sharp scaling, 1 = full physical grid.
    pub strength: f32,
    /// Published parameterization of the band-limited grid for this panel
    /// class: gain/gamma/tint and aperture half-widths.
    pub gain: f32,
    pub gamma: f32,
    pub tint: f32,
    pub stripe_halfwidth: f32,
    pub row_halfwidth: f32,
    pub bgr: bool,
}

impl GridParams {
    /// The published grid parameter set for this panel family (gain 1.5,
    /// linearization 2.2, stripe self-transmission 0.75, BGR stripe order,
    /// stripe aperture half-width 1.5 subpixels, row half-width 0.63).
    pub fn with_strength(strength: f32) -> GridParams {
        GridParams {
            strength: strength.clamp(0.0, 1.0),
            gain: 1.5,
            gamma: 2.2,
            tint: 0.75,
            stripe_halfwidth: 1.5,
            row_halfwidth: 0.63,
            bgr: true,
        }
    }
}

pub struct Presenter {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    texture: wgpu::Texture,
    uniforms: wgpu::Buffer,
    config: wgpu::SurfaceConfiguration,
    /// True when the surface format is an sRGB view (only *Srgb formats
    /// were available): the shader must pre-decode its display-encoded
    /// output so the hardware re-encode is an identity overall.
    srgb_surface: bool,
    src_size: (u32, u32),
}

impl Presenter {
    /// Create a presenter on `window`'s surface.
    ///
    /// `vsync` selects the present mode only (false = AutoNoVsync, true =
    /// AutoVsync): presentation pacing, never emulation pacing — game
    /// speed is owned by the audio clock (see module docs).
    ///
    /// The surface takes an owning `Arc` clone of `window`, so the window is
    /// guaranteed to outlive the surface — no `unsafe`, no scope contract.
    pub fn new<W>(
        window: Arc<W>,
        src_w: u32,
        src_h: u32,
        target: DisplayTarget,
        vsync: bool,
    ) -> Result<Presenter, String>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e| format!("no graphics adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("screen-presenter"),
            ..Default::default()
        }))
        .map_err(|e| format!("graphics device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        // Display-encoded bytes in, display-encoded bytes out: prefer a
        // UNORM (non-sRGB-view) format so nothing re-encodes behind our
        // back. Some GL/Vulkan/compositor combos only expose *Srgb
        // formats — accept those and have the shader pre-decode its final
        // color (piecewise sRGB EOTF) so the hardware re-encode reproduces
        // the intended bytes.
        let format = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ]
        .into_iter()
        .find(|f| caps.formats.contains(f))
        .ok_or_else(|| format!("no 8-bit surface format in {:?}", caps.formats))?;
        let srgb_surface = format.is_srgb();
        // Opaque when supported (we always write alpha 1 and want no
        // compositor blending); otherwise whatever the platform prefers.
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes[0]
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            // Guard against a zero-sized (e.g. minimized) start; present()
            // resizes to the real window size on the first frame.
            width: (src_w * 3).max(1),
            height: (src_h * 3).max(1),
            present_mode: if vsync {
                wgpu::PresentMode::AutoVsync
            } else {
                wgpu::PresentMode::AutoNoVsync
            },
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats: vec![],
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screen-src"),
            size: wgpu::Extent3d {
                width: src_w,
                height: src_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screen-params"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("screen-bind"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("screen-bind"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniforms.as_entire_binding(),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("screen-grid"),
            source: wgpu::ShaderSource::Wgsl(include_str!("grid.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("screen-pipeline"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("screen-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let p = Presenter {
            surface,
            device,
            queue,
            pipeline,
            bind_group,
            texture,
            uniforms,
            config,
            srgb_surface,
            src_size: (src_w, src_h),
        };
        p.surface.configure(&p.device, &p.config);
        // Color-match the layer to the panel (macOS only); the surface +
        // CAMetalLayer now exist on the window's view.
        #[cfg(target_os = "macos")]
        macos::tag_layer_colorspace(macos::ns_view_ptr(window.as_ref()), target);
        // macOS consumes `target` above. Elsewhere the LUT was already encoded
        // for the resolved primaries (see `display::resolve`) and we declare
        // nothing to the compositor — Wayland treats untagged surfaces as sRGB.
        let _ = target;
        Ok(p)
    }

    /// Switch presentation mode live (false = AutoNoVsync, true =
    /// AutoVsync). Pacing principle: emulation speed is owned by the audio
    /// clock; vsync only affects presentation (tearing vs latency), never
    /// game speed — see the module header.
    pub fn set_vsync(&mut self, on: bool) {
        let mode = if on {
            wgpu::PresentMode::AutoVsync
        } else {
            wgpu::PresentMode::AutoNoVsync
        };
        if self.config.present_mode != mode {
            self.config.present_mode = mode;
            self.config.width = self.config.width.max(1);
            self.config.height = self.config.height.max(1);
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// Upload the frame and draw it at the window's current size.
    /// `window_physical` is the surface size in physical pixels (winit's
    /// `inner_size()` reports this directly and cross-platform).
    pub fn present(
        &mut self,
        rgba: &[[u8; 4]],
        window_physical: (u32, u32),
        grid: &GridParams,
    ) -> Result<(), String> {
        let (w, h) = (window_physical.0.max(1), window_physical.1.max(1));
        if (w, h) != (self.config.width, self.config.height) {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }

        let (sw, sh) = self.src_size;
        debug_assert_eq!(rgba.len(), (sw * sh) as usize);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck_cast(rgba),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(sw * 4),
                rows_per_image: Some(sh),
            },
            wgpu::Extent3d {
                width: sw,
                height: sh,
                depth_or_array_layers: 1,
            },
        );

        // Aspect-correct letterbox (source is 3:2), centered.
        let scale = ((w as f32) / (sw as f32)).min((h as f32) / (sh as f32));
        let dst = ((sw as f32) * scale, (sh as f32) * scale);
        let origin = (((w as f32) - dst.0) * 0.5, ((h as f32) - dst.1) * 0.5);
        let params: [f32; 16] = [
            sw as f32,
            sh as f32,
            origin.0,
            origin.1,
            dst.0,
            dst.1,
            grid.strength,
            grid.gain,
            grid.gamma,
            grid.tint,
            grid.stripe_halfwidth,
            grid.row_halfwidth,
            if grid.bgr { 1.0 } else { 0.0 },
            if self.srgb_surface { 1.0 } else { 0.0 },
            0.0,
            0.0,
        ];
        self.queue
            .write_buffer(&self.uniforms, 0, as_bytes(&params));

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            // Outdated/Lost: the surface needs reconfiguring (resize, mode
            // change, GL context dance). OutOfMemory: some drivers report
            // it transiently for swapchain churn — reconfigure ONCE and
            // retry; if it fails again the Err propagates and the caller
            // falls back to CPU blit loudly.
            Err(
                wgpu::SurfaceError::Outdated
                | wgpu::SurfaceError::Lost
                | wgpu::SurfaceError::OutOfMemory,
            ) => {
                self.surface.configure(&self.device, &self.config);
                self.surface
                    .get_current_texture()
                    .map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("screen-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
        frame.present();
        Ok(())
    }
}

fn bytemuck_cast(rgba: &[[u8; 4]]) -> &[u8] {
    // [[u8;4]] is layout-identical to a flat byte slice.
    unsafe { std::slice::from_raw_parts(rgba.as_ptr().cast(), rgba.len() * 4) }
}

fn as_bytes(v: &[f32; 16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), 64) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn grid_shader_parses_and_validates() {
        let module = naga::front::wgsl::parse_str(include_str!("grid.wgsl"))
            .unwrap_or_else(|e| panic!("WGSL parse: {e}"));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("WGSL validate: {e:?}"));
    }

    #[test]
    fn uniform_struct_matches_written_buffer() {
        // present() writes a [f32; 16] (64-byte buffer); the WGSL Params
        // struct is 3 vec2f + 8 scalars = 14 floats / 56 bytes, and field
        // ORDER is what the Rust array encodes — this guards the count so a
        // drifted struct fails loudly instead of misreading params.
        // Slot 13 (the 8th scalar, last field) is srgb_surface: the
        // sRGB-format-surface compensation flag set by Presenter.
        let wgsl = include_str!("grid.wgsl");
        let fields = wgsl
            .split("struct Params {")
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        let vec2s = fields.matches("vec2f").count();
        let scalars = fields.matches("f32").count();
        assert_eq!(vec2s, 3, "Params vec2 count drifted");
        assert_eq!(vec2s * 2 + scalars, 14, "Params float count drifted");
        // The flag must be the LAST declared field (Rust array slot 13).
        let last_field = fields
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                l.split_once(':')
                    .filter(|(name, _)| !name.contains("//"))
                    .map(|(name, _)| name.trim())
            })
            .last();
        assert_eq!(
            last_field,
            Some("srgb_surface"),
            "srgb_surface must occupy params slot 13"
        );
    }
}

#[cfg(target_os = "macos")]
mod macos {
    //! Colorspace tagging: a bare metal layer is NOT color-matched by the
    //! compositor — its bytes go to the panel raw, which on a P3 laptop
    //! screen oversaturates everything (the classic "vivid emulator"
    //! artifact). Tagging the layer with the space our LUT encodes for
    //! makes the compositor color-match per-monitor, which is exactly the
    //! per-user-display calibration this feature is about.

    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::CFString;
    use objc2_core_graphics::CGColorSpace;
    use objc2_quartz_core::CAMetalLayer;

    use crate::profile::DisplayTarget;

    pub fn ns_view_ptr(window: &impl raw_window_handle::HasWindowHandle) -> *mut std::ffi::c_void {
        match window.window_handle().map(|h| h.as_raw()) {
            Ok(raw_window_handle::RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr(),
            _ => std::ptr::null_mut(),
        }
    }

    pub fn tag_layer_colorspace(ns_view: *mut std::ffi::c_void, target: DisplayTarget) {
        if ns_view.is_null() {
            return;
        }
        unsafe {
            let view = ns_view as *mut AnyObject;
            let layer: Option<Retained<AnyObject>> = msg_send![&*view, layer];
            let Some(layer) = layer else { return };
            let Ok(layer) = layer.downcast::<CAMetalLayer>() else {
                return;
            };
            let name = match target {
                DisplayTarget::Auto | DisplayTarget::Srgb => {
                    CFString::from_static_str("kCGColorSpaceSRGB")
                }
                DisplayTarget::DisplayP3 => CFString::from_static_str("kCGColorSpaceDisplayP3"),
            };
            if let Some(cs) = CGColorSpace::with_name(Some(&name)) {
                layer.setColorspace(Some(&cs));
            }
        }
    }
}
