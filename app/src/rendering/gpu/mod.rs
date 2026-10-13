//! Terminal grids drawn on the GPU, on the device Slint renders with.
//!
//! Each terminal keeps two content textures, its newest grid and the one before, and
//! a display texture that Slint shows as an image. Content is drawn at frame time and
//! only when the screen changed: backgrounds, glyphs and lines as quads in one pass.
//! Composing then places the newest content at its scroll position over the previous
//! one, blurs it along the motion while it glides, and draws the cursor with its trail.

mod atlas;
mod cells;
mod drawn_glyphs;
mod glyphs;
mod motion;

use atlas::Atlas;
use cells::{QUAD_ATTRIBUTES, Quad};
use glyphs::GlyphCache;
use motion::{Cursor, Scroll};
use std::{collections::HashMap, mem::size_of, time::Instant};
use basalt_core::terminal::Screen;
use uuid::Uuid;
use wgpu::{BindGroup, BindGroupLayout, Buffer, Device, Queue, RenderPipeline, Texture, TextureView};

pub use crossfont::Size as FontSize;

/// How a terminal looks besides its cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// Cells in the default background, and anything outside the content.
    pub background: [u8; 3],
    pub cursor: [u8; 3],
    /// Height of the cursor bar in pixels.
    pub cursor_height: f32,
    pub focused: bool,
}

/// Drawn over a terminal's command line: an inline suggestion and its key hint in a
/// faded color, and the risky parts of the command in amber.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overlay {
    pub row: usize,
    /// Where the suggestion starts: the cursor's column.
    pub column: usize,
    pub ghost: String,
    pub hint: String,
    pub color: [u8; 3],
    /// Columns of the command line drawn in `risky_color`.
    pub risky: Vec<std::ops::Range<usize>>,
    pub risky_color: [u8; 3],
    /// A list under (or over) the command line, drawn as terminal lines.
    pub list: Vec<ListItem>,
    pub selected: usize,
    /// The column of the word being completed; the angle marker sits two before it.
    pub list_column: usize,
    /// Selected name, other names, and the progress bar's fill.
    pub list_colors: [[u8; 3]; 3],
    /// Beside the selected row, right of the list.
    pub question: String,
    pub question_color: [u8; 3],
    /// Replaces the question while something runs for the selected row.
    pub progress: Option<Progress>,
}

/// One list row: its name, who made it, a faded detail and a faded note at the right.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListItem {
    pub name: String,
    /// "by Anthropic"; empty when unknown or not asked for.
    pub by: String,
    pub detail: String,
    pub note: String,
}

/// A progress bar for work of unknown length: it eases towards most of the way while
/// the work runs and fills quickly once it is done.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    pub started: Instant,
    pub finished: Option<Instant>,
    /// Seconds to cover about two thirds of the bar.
    pub pace: f32,
}

impl Progress {
    const FILL_SECONDS: f32 = 0.3;

    pub fn new(started: Instant, pace: f32) -> Self {
        Self { started, finished: None, pace }
    }

    fn running(&self, at: Instant) -> f32 {
        0.92 * (1.0 - (-at.saturating_duration_since(self.started).as_secs_f32() / self.pace.max(0.1)).exp())
    }

    /// How full the bar is, 0 to 1.
    pub fn value(&self, now: Instant) -> f32 {
        match self.finished {
            None => self.running(now),
            Some(finished) => {
                let from = self.running(finished);
                let t = (now.saturating_duration_since(finished).as_secs_f32() / Self::FILL_SECONDS).min(1.0);
                from + (1.0 - from) * (1.0 - (1.0 - t).powi(3))
            }
        }
    }

    /// Whether the bar still changes, so frames should keep coming.
    pub fn moving(&self, now: Instant) -> bool {
        self.finished.is_none_or(|finished| now.saturating_duration_since(finished).as_secs_f32() < Self::FILL_SECONDS)
    }
}

/// The angle marker glides between list rows.
#[derive(Clone, Copy, Debug)]
struct Marker {
    from: f32,
    to: f32,
    since: Instant,
}

impl Marker {
    const SECONDS: f32 = 0.09;

    fn at(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.since).as_secs_f32() / Self::SECONDS).min(1.0);
        self.from + (self.to - self.from) * (1.0 - (1.0 - t).powi(3))
    }

    fn moving(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since).as_secs_f32() < Self::SECONDS
    }
}

/// Motion blur covers the distance travelled in one frame at this rate.
const SHUTTER_SECONDS: f32 = 1.0 / 180.0;
const MAX_BLUR: f32 = 24.0;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Pipelines and the atlas, created once Slint's device exists.
struct Gpu {
    device: Device,
    queue: Queue,
    solid: RenderPipeline,
    glyph: RenderPipeline,
    compose: RenderPipeline,
    cursor: RenderPipeline,
    grid_layout: BindGroupLayout,
    compose_layout: BindGroupLayout,
    atlas: Atlas,
    atlas_group: BindGroup,
}

struct Textures {
    width: u32,
    height: u32,
    content: [TextureView; 2],
    display: TextureView,
    image: slint::Image,
    /// Compose inputs with content 0 or content 1 as the newest.
    compose_groups: [BindGroup; 2],
}

/// Buffers that live as long as a terminal.
struct Buffers {
    grid: Buffer,
    grid_group: BindGroup,
    compose: Buffer,
    quads: Option<Buffer>,
}

struct Grid {
    screen: Screen,
    look: Look,
    overlay: Option<Overlay>,
    marker: Option<Marker>,
    textures: Option<Textures>,
    buffers: Option<Buffers>,
    quads: Vec<Quad>,
    /// Which content texture holds the newest grid.
    newest: usize,
    /// The screen, the font or the look changed since the content was drawn.
    stale: bool,
    /// The display matches the content; false when it must be composed again.
    composed: bool,
    /// Moving at the last compose: one more compose lands it at rest.
    moved: bool,
    /// The user scrolled since the content was drawn.
    scrolled: bool,
    /// Display offset of the newest content, to tell how far a scroll moved it.
    offset: usize,
    /// The textures were replaced; nothing relates the next content to the last.
    resized: bool,
    cursor_shown: bool,
    scroll: Scroll,
    cursor: Cursor,
}

pub struct Grids {
    glyphs: GlyphCache,
    gpu: Option<Gpu>,
    grids: HashMap<Uuid, Grid>,
}

impl Grids {
    pub fn new(family: &str, size: FontSize) -> Result<Self, crossfont::Error> {
        Ok(Self { glyphs: GlyphCache::new(family, size)?, gpu: None, grids: HashMap::new() })
    }

    /// Whether the GPU side exists; until then nothing is drawn.
    pub fn ready(&self) -> bool {
        self.gpu.is_some()
    }

    /// Whole-pixel cell size of the current font.
    pub fn cell(&self) -> (u32, u32) {
        self.glyphs.cell()
    }

    /// Change family or size; true when it changed and every grid draws again.
    pub fn set_font(&mut self, family: &str, size: FontSize) -> bool {
        if !self.glyphs.set_font(family, size).unwrap_or(false) {
            return false;
        }
        if let Some(gpu) = &mut self.gpu {
            gpu.atlas.clear();
        }
        for grid in self.grids.values_mut() {
            grid.stale = true;
        }
        true
    }

    /// Create pipelines and the atlas on Slint's device.
    pub fn attach(&mut self, device: &Device, queue: &Queue) {
        self.gpu = Some(Gpu::new(device, queue));
        self.glyphs.clear();
        for grid in self.grids.values_mut() {
            grid.stale = true;
        }
    }

    /// Copy a terminal's screen in to be drawn on the next frame. Returns the image
    /// that shows the terminal; `None` until the GPU side exists.
    pub fn update(&mut self, id: Uuid, read: impl FnOnce(&mut Screen), look: Look) -> Option<slint::Image> {
        let now = Instant::now();
        let grid = self.grids.entry(id).or_insert_with(|| Grid {
            screen: Screen::default(),
            look,
            overlay: None,
            marker: None,
            textures: None,
            buffers: None,
            quads: Vec::new(),
            newest: 0,
            stale: true,
            composed: false,
            moved: false,
            scrolled: false,
            offset: 0,
            resized: true,
            cursor_shown: false,
            scroll: Scroll::new(now),
            cursor: Cursor::new(now),
        });
        read(&mut grid.screen);
        grid.look = look;
        grid.stale = true;
        let gpu = self.gpu.as_ref()?;
        let (cell_width, cell_height) = self.glyphs.cell();
        let width = (grid.screen.columns as u32 * cell_width).max(1);
        let height = (grid.screen.rows as u32 * cell_height).max(1);
        if grid.textures.as_ref().is_none_or(|textures| textures.width != width || textures.height != height) {
            grid.textures = Some(gpu.textures(width, height, grid.buffers(gpu))?);
            grid.resized = true;
        }
        grid.textures.as_ref().map(|textures| textures.image.clone())
    }

    /// The screen last copied in for a terminal.
    pub fn screen(&self, id: &Uuid) -> Option<&Screen> {
        self.grids.get(id).map(|grid| &grid.screen)
    }

    /// What to draw over a terminal's command line; drawn with its next content. True
    /// when it changed, so a frame is needed.
    pub fn set_overlay(&mut self, id: &Uuid, overlay: Option<Overlay>) -> bool {
        match self.grids.get_mut(id) {
            Some(grid) if grid.overlay != overlay => {
                // A new selection in the same list: the marker glides there.
                let now = Instant::now();
                let selected = |overlay: &Option<Overlay>| {
                    overlay.as_ref().filter(|overlay| !overlay.list.is_empty()).map(|overlay| (overlay.list.len(), overlay.selected))
                };
                grid.marker = match (selected(&grid.overlay), selected(&overlay)) {
                    (Some((before, from)), Some((after, to))) if before == after && from != to => {
                        let at = grid.marker.map_or(from as f32, |marker| marker.at(now));
                        Some(Marker { from: at, to: to as f32, since: now })
                    }
                    (_, Some((_, to))) => grid.marker.filter(|marker| marker.to == to as f32),
                    _ => None,
                };
                grid.overlay = overlay;
                grid.stale = true;
                true
            }
            _ => false,
        }
    }

    /// The user scrolled this terminal: its next content glides into place.
    pub fn scrolled(&mut self, id: &Uuid) {
        if let Some(grid) = self.grids.get_mut(id) {
            grid.scrolled = true;
        }
    }

    /// Forget a terminal that is gone.
    pub fn remove(&mut self, id: &Uuid) {
        self.grids.remove(id);
    }

    /// Draw changed content and compose changed or moving grids. True while anything
    /// is still moving, so the next frame should come too.
    pub fn render(&mut self, now: Instant) -> bool {
        let Some(gpu) = &mut self.gpu else {
            return false;
        };
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("terminal grids") });
        let mut moving = false;
        let mut submitted = false;
        let (cell_width, cell_height) = self.glyphs.cell();
        for grid in self.grids.values_mut() {
            if grid.textures.is_none() {
                continue;
            }
            // A gliding marker or a running progress bar draws the content again.
            let animated = grid.marker.is_some_and(|marker| marker.moving(now))
                || grid.overlay.as_ref().and_then(|overlay| overlay.progress).is_some_and(|progress| progress.moving(now));
            if grid.stale || animated || grid.marker.is_some() {
                if !animated {
                    grid.marker = None;
                }
                grid.draw(gpu, &mut self.glyphs, &mut encoder, (cell_width, cell_height), now);
                submitted = true;
            }
            moving |= animated;
            let still = !grid.scroll.moving(now) && !grid.cursor.moving(now);
            if !grid.composed || !still || grid.moved {
                grid.compose(gpu, &mut encoder, now);
                submitted = true;
            }
            grid.moved = !still;
            moving |= !still;
        }
        if submitted {
            gpu.queue.submit(Some(encoder.finish()));
        }
        moving
    }
}

impl Grid {
    fn buffers(&mut self, gpu: &Gpu) -> &Buffers {
        self.buffers.get_or_insert_with(|| {
            let uniform = |size| {
                gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("terminal grid"),
                    size,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            };
            let grid = uniform(16);
            let grid_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("terminal grid"),
                layout: &gpu.grid_layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: grid.as_entire_binding() }],
            });
            Buffers { grid, grid_group, compose: uniform(96), quads: None }
        })
    }

    /// Draw the screen into the other content texture, which becomes the newest, and
    /// start the motion from the previous content to it.
    fn draw(
        &mut self,
        gpu: &mut Gpu,
        glyphs: &mut GlyphCache,
        encoder: &mut wgpu::CommandEncoder,
        (cell_width, cell_height): (u32, u32),
        now: Instant,
    ) {
        self.stale = false;
        self.composed = false;
        let offset = self.screen.display_offset;
        if self.scrolled && !self.resized && offset != self.offset {
            let lines = offset as f32 - self.offset as f32;
            self.scroll.shifted(lines * cell_height as f32, now);
        } else if !self.scrolled || self.resized {
            self.scroll.reset(now);
        }
        self.offset = offset;
        self.scrolled = false;
        let screen = &self.screen;
        let cursor_visible = self.look.focused
            && screen.mode & 1 != 0
            && screen.cursor_line >= 0
            && screen.display_offset == 0
            && (screen.cursor_column as usize) < screen.columns;
        if cursor_visible {
            let height = self.look.cursor_height.max(1.0);
            let rect = [
                screen.cursor_column as f32 * cell_width as f32,
                (screen.cursor_line + 1) as f32 * cell_height as f32 - height,
                cell_width as f32,
                height,
            ];
            self.cursor.move_to(rect, self.resized || !self.cursor_shown, now);
        }
        self.cursor_shown = cursor_visible;
        self.resized = false;

        // Build the quads; a full atlas is cleared once and the grid built again.
        let overlay = self.overlay.as_ref();
        let motion = cells::Motion { marker: self.marker.map(|marker| marker.at(now)), now };
        let built = cells::build(screen, glyphs, &mut gpu.atlas, &gpu.queue, &mut self.quads, overlay, motion, cell_width, cell_height);
        let Ok((glyphs_from, lines_from)) = built.or_else(|_| {
            gpu.atlas.clear();
            glyphs.clear();
            cells::build(screen, glyphs, &mut gpu.atlas, &gpu.queue, &mut self.quads, overlay, motion, cell_width, cell_height)
        }) else {
            return;
        };
        let (Some(textures), Some(buffers)) = (&self.textures, &mut self.buffers) else {
            return;
        };
        gpu.queue.write_buffer(&buffers.grid, 0, bytemuck::cast_slice(&[textures.width as f32, textures.height as f32, 0.0, 0.0]));
        let bytes = (self.quads.len().max(1) * size_of::<Quad>()) as u64;
        if buffers.quads.as_ref().is_none_or(|buffer| buffer.size() < bytes) {
            buffers.quads = Some(gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("terminal grid quads"),
                size: bytes.next_power_of_two(),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let Some(quads) = &buffers.quads else {
            return;
        };
        if !self.quads.is_empty() {
            gpu.queue.write_buffer(quads, 0, bytemuck::cast_slice(&self.quads));
        }
        self.newest = 1 - self.newest;
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terminal grid content"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &textures.content[self.newest],
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(color(self.look.background)), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_bind_group(0, &buffers.grid_group, &[]);
        pass.set_bind_group(1, &gpu.atlas_group, &[]);
        pass.set_vertex_buffer(0, quads.slice(..));
        pass.set_pipeline(&gpu.solid);
        pass.draw(0..4, 0..glyphs_from);
        pass.set_pipeline(&gpu.glyph);
        pass.draw(0..4, glyphs_from..lines_from);
        pass.set_pipeline(&gpu.solid);
        pass.draw(0..4, lines_from..self.quads.len() as u32);
    }

    /// Show the content at its current scroll position, and the cursor.
    fn compose(&mut self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder, now: Instant) {
        self.composed = true;
        let (Some(textures), Some(buffers)) = (&self.textures, &self.buffers) else {
            return;
        };
        let blur = (self.scroll.speed(now) * SHUTTER_SECONDS).min(MAX_BLUR);
        let [r, g, b] = self.look.background.map(|value| f32::from(value) / 255.0);
        let [cr, cg, cb] = self.look.cursor.map(|value| f32::from(value) / 255.0);
        let [top_left, top_right, bottom_left, bottom_right] = self.cursor.corners(now);
        #[rustfmt::skip]
        let uniform: [f32; 24] = [
            textures.width as f32, textures.height as f32, self.scroll.position(now), self.scroll.link(),
            blur, 0.0, 0.0, 0.0,
            r, g, b, 1.0,
            top_left[0], top_left[1], top_right[0], top_right[1],
            bottom_left[0], bottom_left[1], bottom_right[0], bottom_right[1],
            cr, cg, cb, 1.0,
        ];
        gpu.queue.write_buffer(&buffers.compose, 0, bytemuck::cast_slice(&uniform));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terminal grid display"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &textures.display,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_bind_group(0, &textures.compose_groups[self.newest], &[]);
        pass.set_pipeline(&gpu.compose);
        pass.draw(0..3, 0..1);
        if self.cursor_shown {
            pass.set_pipeline(&gpu.cursor);
            pass.draw(0..4, 0..1);
        }
    }
}

fn color([r, g, b]: [u8; 3]) -> wgpu::Color {
    wgpu::Color { r: f64::from(r) / 255.0, g: f64::from(g) / 255.0, b: f64::from(b) / 255.0, a: 1.0 }
}

impl Gpu {
    fn new(device: &Device, queue: &Queue) -> Self {
        let uniform_entry = |binding, visibility| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let texture_entry = |binding, filterable| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let grid_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terminal grid"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX)],
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glyph atlas"),
            entries: &[
                texture_entry(0, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let compose_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terminal grid display"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT),
                texture_entry(1, false),
                texture_entry(2, false),
            ],
        });
        let atlas = Atlas::new(device);
        // Glyphs land on whole pixels, so texels map one to one.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph atlas"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let atlas_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph atlas"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(atlas.view()) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let cells = device.create_shader_module(wgpu::include_wgsl!("grid.wgsl"));
        let cells_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("terminal grid content"),
            bind_group_layouts: &[&grid_layout, &atlas_layout],
            push_constant_ranges: &[],
        });
        let display = device.create_shader_module(wgpu::include_wgsl!("compose.wgsl"));
        let display_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("terminal grid display"),
            bind_group_layouts: &[&compose_layout],
            push_constant_ranges: &[],
        });
        let pipeline = |layout: &wgpu::PipelineLayout,
                        module: &wgpu::ShaderModule,
                        (vertex, fragment): (&str, &str),
                        quads: bool,
                        topology,
                        blend| {
            let buffers = [wgpu::VertexBufferLayout {
                array_stride: size_of::<Quad>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &QUAD_ATTRIBUTES,
            }];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fragment),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some(vertex),
                    compilation_options: Default::default(),
                    buffers: if quads { &buffers } else { &[] },
                },
                primitive: wgpu::PrimitiveState { topology, ..Default::default() },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fragment),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: FORMAT, blend, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview: None,
                cache: None,
            })
        };
        let strip = wgpu::PrimitiveTopology::TriangleStrip;
        let solid = pipeline(&cells_layout, &cells, ("vertex", "solid"), true, strip, Some(wgpu::BlendState::ALPHA_BLENDING));
        // Each colour channel blends by its own coverage; the target stays opaque.
        let subpixel = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Src1,
                dst_factor: wgpu::BlendFactor::OneMinusSrc1,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let glyph = pipeline(&cells_layout, &cells, ("vertex", "glyph"), true, strip, Some(subpixel));
        let compose = pipeline(
            &display_layout,
            &display,
            ("screen", "content"),
            false,
            wgpu::PrimitiveTopology::TriangleList,
            None,
        );
        let cursor =
            pipeline(&display_layout, &display, ("cursor_shape", "cursor"), false, strip, Some(wgpu::BlendState::ALPHA_BLENDING));
        Self {
            device: device.clone(),
            queue: queue.clone(),
            solid,
            glyph,
            compose,
            cursor,
            grid_layout,
            compose_layout,
            atlas,
            atlas_group,
        }
    }

    fn textures(&self, width: u32, height: u32, buffers: &Buffers) -> Option<Textures> {
        let texture = |label, usage| -> Texture {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | usage,
                view_formats: &[],
            })
        };
        let content = [0, 1].map(|_| texture("terminal grid content", wgpu::TextureUsages::empty()).create_view(&Default::default()));
        // Copies out of the display serve tests and screenshots.
        let display = texture("terminal grid display", wgpu::TextureUsages::COPY_SRC);
        let image = slint::Image::try_from(display.clone()).ok()?;
        let compose_groups = [0, 1].map(|newest| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("terminal grid display"),
                layout: &self.compose_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: buffers.compose.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&content[newest]) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&content[1 - newest]) },
                ],
            })
        });
        Some(Textures {
            width,
            height,
            content,
            display: display.create_view(&Default::default()),
            image,
            compose_groups,
        })
    }
}
