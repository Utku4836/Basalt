//! One texture holding every rasterized glyph, packed row by row: each glyph goes to
//! the right of the previous one, and a new row starts above the tallest glyph of the
//! current row once it runs out of width.

use crossfont::{BitmapBuffer, RasterizedGlyph};
use wgpu::{Device, Queue, Texture, TextureView};

pub const ATLAS_SIZE: u32 = 2048;

/// Where a glyph sits in the atlas and how it is placed in its cell.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Glyph {
    /// Distance from the baseline-adjusted cell bottom to the glyph top, in pixels.
    pub top: i16,
    pub left: i16,
    pub width: i16,
    pub height: i16,
    /// Left, top, width and height in texture coordinates.
    pub uv: [f32; 4],
}

#[derive(Debug, PartialEq)]
pub enum AtlasFull {
    /// No room left; clear the atlas and load the glyphs in use again.
    Full,
    /// Larger than the whole atlas.
    TooLarge,
}

pub struct Atlas {
    texture: Texture,
    view: TextureView,
    size: u32,
    /// Where the next glyph in the current row starts.
    row_extent: u32,
    /// Top of the current row.
    row_baseline: u32,
    /// Tallest glyph in the current row.
    row_tallest: u32,
}

impl Atlas {
    pub fn new(device: &Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Self { texture, view, size: ATLAS_SIZE, row_extent: 0, row_baseline: 0, row_tallest: 0 }
    }

    pub fn view(&self) -> &TextureView {
        &self.view
    }

    pub fn clear(&mut self) {
        self.row_extent = 0;
        self.row_baseline = 0;
        self.row_tallest = 0;
    }

    pub fn insert(&mut self, queue: &Queue, glyph: &RasterizedGlyph) -> Result<Glyph, AtlasFull> {
        let (width, height) = (glyph.width.max(0) as u32, glyph.height.max(0) as u32);
        if width > self.size || height > self.size {
            return Err(AtlasFull::TooLarge);
        }
        if !self.room_in_row(width, height) {
            self.advance_row()?;
        }
        if !self.room_in_row(width, height) {
            return Err(AtlasFull::Full);
        }
        let (x, y) = (self.row_extent, self.row_baseline);
        if width > 0 && height > 0 {
            // Coverage masks come as RGB; the texture keeps four bytes per pixel.
            let rgba = match &glyph.buffer {
                BitmapBuffer::Rgb(rgb) => rgb.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
                BitmapBuffer::Rgba(rgba) => rgba.clone(),
            };
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &rgba,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width * 4), rows_per_image: None },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }
        self.row_extent = x + width;
        self.row_tallest = self.row_tallest.max(height);
        let size = self.size as f32;
        Ok(Glyph {
            top: glyph.top as i16,
            left: glyph.left as i16,
            width: width as i16,
            height: height as i16,
            uv: [x as f32 / size, y as f32 / size, width as f32 / size, height as f32 / size],
        })
    }

    fn room_in_row(&self, width: u32, height: u32) -> bool {
        self.row_extent + width <= self.size && height < self.size - self.row_baseline
    }

    fn advance_row(&mut self) -> Result<(), AtlasFull> {
        let next = self.row_baseline + self.row_tallest;
        if next >= self.size {
            return Err(AtlasFull::Full);
        }
        self.row_baseline = next;
        self.row_extent = 0;
        self.row_tallest = 0;
        Ok(())
    }
}
