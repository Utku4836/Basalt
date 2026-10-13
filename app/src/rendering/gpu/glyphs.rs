//! Rasterized glyphs by character, face and size. Glyphs are rasterized once with
//! DirectWrite (ClearType coverage masks) and kept in the atlas; box drawing and
//! powerline separators are drawn to fill the cell instead.

use super::atlas::{Atlas, AtlasFull, Glyph};
use super::drawn_glyphs::drawn_glyph;
use crossfont::{
    Error, FontDesc, FontKey, GlyphKey, Metrics, Rasterize, RasterizedGlyph, Rasterizer, Size, Slant, Style, Weight,
};
use std::collections::HashMap;
use wgpu::Queue;

/// The four faces of one family.
#[derive(Clone, Copy)]
struct Faces {
    regular: FontKey,
    bold: FontKey,
    italic: FontKey,
    bold_italic: FontKey,
}

pub struct GlyphCache {
    rasterizer: Rasterizer,
    glyphs: HashMap<GlyphKey, Glyph>,
    family: String,
    size: Size,
    faces: Faces,
    metrics: Metrics,
}

impl GlyphCache {
    pub fn new(family: &str, size: Size) -> Result<Self, Error> {
        let mut rasterizer = Rasterizer::new()?;
        let faces = load_faces(&mut rasterizer, family, size)?;
        let metrics = load_metrics(&mut rasterizer, faces.regular, size)?;
        Ok(Self { rasterizer, glyphs: HashMap::new(), family: family.into(), size, faces, metrics })
    }

    /// Switch family or size. True when anything changed: the atlas then needs a clear.
    pub fn set_font(&mut self, family: &str, size: Size) -> Result<bool, Error> {
        if self.family == family && self.size == size {
            return Ok(false);
        }
        let faces = load_faces(&mut self.rasterizer, family, size)?;
        self.metrics = load_metrics(&mut self.rasterizer, faces.regular, size)?;
        self.faces = faces;
        self.family = family.into();
        self.size = size;
        self.glyphs.clear();
        Ok(true)
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Whole-pixel cell size: the advance and the line height, rounded down.
    pub fn cell(&self) -> (u32, u32) {
        (self.metrics.average_advance.floor().max(1.0) as u32, self.metrics.line_height.floor().max(1.0) as u32)
    }

    /// Forget every glyph; used together with clearing the atlas.
    pub fn clear(&mut self) {
        self.glyphs.clear();
    }

    /// The glyph for `character` in the given face, rasterizing it into the atlas on
    /// first use. Characters no font has show the font's missing-glyph box.
    pub fn get(
        &mut self,
        character: char,
        bold: bool,
        italic: bool,
        zero_width: bool,
        atlas: &mut Atlas,
        queue: &Queue,
    ) -> Result<Glyph, AtlasFull> {
        let font_key = match (bold, italic) {
            (false, false) => self.faces.regular,
            (true, false) => self.faces.bold,
            (false, true) => self.faces.italic,
            (true, true) => self.faces.bold_italic,
        };
        let key = GlyphKey { character, font_key, size: self.size };
        if let Some(glyph) = self.glyphs.get(&key) {
            return Ok(*glyph);
        }
        let rasterized = match drawn_glyph(character, &self.metrics) {
            Some(glyph) => Ok(glyph),
            None => self.rasterizer.get_glyph(key),
        };
        let glyph = match rasterized {
            Ok(glyph) => self.load(glyph, zero_width, atlas, queue)?,
            Err(Error::MissingGlyph(glyph)) => {
                let missing = GlyphKey { character: '\0', ..key };
                match self.glyphs.get(&missing) {
                    Some(glyph) => *glyph,
                    None => {
                        let glyph = self.load(glyph, false, atlas, queue)?;
                        self.glyphs.insert(missing, glyph);
                        glyph
                    }
                }
            }
            Err(_) => Glyph::default(),
        };
        self.glyphs.insert(key, glyph);
        Ok(glyph)
    }

    fn load(
        &self,
        mut glyph: RasterizedGlyph,
        zero_width: bool,
        atlas: &mut Atlas,
        queue: &Queue,
    ) -> Result<Glyph, AtlasFull> {
        // Measured from the cell bottom rather than the baseline.
        glyph.top -= self.metrics.descent as i32;
        // A combining mark is rasterized as if it followed its base character; it is
        // drawn in the base character's cell, so its anchor moves one cell right.
        if zero_width {
            glyph.left += self.metrics.average_advance as i32;
        }
        match atlas.insert(queue, &glyph) {
            Err(AtlasFull::TooLarge) => Ok(Glyph::default()),
            result => result,
        }
    }
}

fn load_faces(rasterizer: &mut Rasterizer, family: &str, size: Size) -> Result<Faces, Error> {
    let describe = |slant, weight| FontDesc::new(family, Style::Description { slant, weight });
    let named = |face: &str| FontDesc::new(family, Style::Specific(face.into()));
    // Text a touch heavier than Regular reads better on a dark grid: the family's own
    // Medium face where it has one, never a smeared Regular.
    let regular = rasterizer
        .load_font(&named("Medium"), size)
        .or_else(|_| rasterizer.load_font(&describe(Slant::Normal, Weight::Normal), size))
        .or_else(|_| rasterizer.load_font(&FontDesc::new("Consolas", Style::Description { slant: Slant::Normal, weight: Weight::Normal }), size))?;
    let italic = rasterizer.load_font(&named("Medium Italic"), size).ok();
    let mut face = |slant, weight| rasterizer.load_font(&describe(slant, weight), size).unwrap_or(regular);
    Ok(Faces {
        regular,
        bold: face(Slant::Normal, Weight::Bold),
        italic: italic.unwrap_or_else(|| face(Slant::Italic, Weight::Normal)),
        bold_italic: face(Slant::Italic, Weight::Bold),
    })
}

fn load_metrics(rasterizer: &mut Rasterizer, regular: FontKey, size: Size) -> Result<Metrics, Error> {
    // A face reports metrics only once one of its glyphs is loaded.
    rasterizer.get_glyph(GlyphKey { font_key: regular, character: 'm', size })?;
    rasterizer.metrics(regular, size)
}
