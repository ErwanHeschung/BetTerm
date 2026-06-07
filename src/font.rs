//! Dynamic glyph atlas backed by fontdue: glyphs are rasterized lazily into a
//! single R8 coverage texture via a shelf packer. An embedded Symbols Nerd Font
//! covers icons the primary font lacks, and powerline separators (U+E0B0..=E0B3)
//! are drawn procedurally for crisp, seamless wedges.

use std::collections::HashMap;

pub const ATLAS_SIZE: u32 = 1024;

/// Symbols-only Nerd Font, used as a fallback for glyphs the primary font lacks.
const FALLBACK_FONT: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

#[derive(Clone, Copy)]
pub struct Glyph {
    /// Pixel rect inside the atlas.
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Placement of the bitmap's top-left relative to the cell's top-left, px.
    pub left: f32,
    pub top: f32,
}

pub struct FontAtlas {
    font: fontdue::Font,
    fallback: Option<fontdue::Font>,
    px: f32,
    pub ascent: f32,
    /// Monospace cell size in pixels.
    pub cell_w: f32,
    pub cell_h: f32,

    pub data: Vec<u8>, // ATLAS_SIZE * ATLAS_SIZE, R8
    /// Bumped whenever a new glyph is rasterized so the renderer re-uploads.
    pub version: u64,

    glyphs: HashMap<char, Option<Glyph>>,
    // Shelf packer cursor.
    pen_x: u32,
    pen_y: u32,
    shelf_h: u32,
}

impl FontAtlas {
    pub fn new(font_bytes: Vec<u8>, font_size: f32, line_height_mult: f32) -> anyhow::Result<Self> {
        let font = fontdue::Font::from_bytes(font_bytes, fontdue::FontSettings::default())
            .map_err(|e| anyhow::anyhow!("font parse: {e}"))?;
        let fallback =
            fontdue::Font::from_bytes(FALLBACK_FONT, fontdue::FontSettings::default()).ok();
        if fallback.is_none() {
            eprintln!("[betterm] warning: fallback symbols font failed to load");
        }

        let lm = font
            .horizontal_line_metrics(font_size)
            .ok_or_else(|| anyhow::anyhow!("font has no horizontal metrics"))?;
        let ascent = lm.ascent;
        let cell_h = ((lm.ascent - lm.descent + lm.line_gap) * line_height_mult).ceil();

        // Use a representative glyph's advance for cell width (monospace fonts
        // give every glyph the same advance).
        let (m, _) = font.rasterize('M', font_size);
        let cell_w = m.advance_width.ceil().max(1.0);

        Ok(Self {
            font,
            fallback,
            px: font_size,
            ascent,
            cell_w,
            cell_h: cell_h.max(1.0),
            data: vec![0u8; (ATLAS_SIZE * ATLAS_SIZE) as usize],
            version: 1,
            glyphs: HashMap::new(),
            pen_x: 0,
            pen_y: 0,
            shelf_h: 0,
        })
    }

    /// Look up a glyph, rasterizing and packing it on first use. Returns `None`
    /// for whitespace / glyphs with no pixels (the caller just skips drawing).
    pub fn glyph(&mut self, ch: char) -> Option<Glyph> {
        if let Some(g) = self.glyphs.get(&ch) {
            return *g;
        }

        // Powerline separators are drawn procedurally for crisp, seamless wedges.
        if let Some((w, h, bmp)) = self.gen_powerline(ch) {
            let g = self.pack_bitmap(w, h, &bmp, 0.0, 0.0);
            self.glyphs.insert(ch, g);
            return g;
        }

        // Pick the primary font, falling back to the symbols font for glyphs the
        // primary doesn't cover (e.g. nerd-font icons).
        let (metrics, bitmap) = {
            let use_fallback = self.font.lookup_glyph_index(ch) == 0
                && self
                    .fallback
                    .as_ref()
                    .is_some_and(|f| f.lookup_glyph_index(ch) != 0);
            let font = if use_fallback {
                self.fallback.as_ref().unwrap()
            } else {
                &self.font
            };
            font.rasterize(ch, self.px)
        };

        let g = if metrics.width == 0 || metrics.height == 0 {
            None
        } else {
            let left = metrics.xmin as f32;
            let top = self.ascent - (metrics.ymin as f32 + metrics.height as f32);
            self.pack_bitmap(
                metrics.width as u32,
                metrics.height as u32,
                &bitmap,
                left,
                top,
            )
        };
        self.glyphs.insert(ch, g);
        g
    }

    fn pack_bitmap(&mut self, w: u32, h: u32, bitmap: &[u8], left: f32, top: f32) -> Option<Glyph> {
        const PAD: u32 = 1;
        if w == 0 || h == 0 || w + PAD > ATLAS_SIZE {
            return None;
        }
        if self.pen_x + w + PAD > ATLAS_SIZE {
            self.pen_x = 0; // next shelf
            self.pen_y += self.shelf_h + PAD;
            self.shelf_h = 0;
        }
        if self.pen_y + h + PAD > ATLAS_SIZE {
            eprintln!("[betterm] glyph atlas full");
            return None;
        }
        let (gx, gy) = (self.pen_x, self.pen_y);
        for row in 0..h {
            let dst = ((gy + row) * ATLAS_SIZE + gx) as usize;
            let src = (row * w) as usize;
            self.data[dst..dst + w as usize].copy_from_slice(&bitmap[src..src + w as usize]);
        }
        self.pen_x += w + PAD;
        self.shelf_h = self.shelf_h.max(h);
        self.version += 1;

        Some(Glyph {
            x: gx,
            y: gy,
            w,
            h,
            left,
            top,
        })
    }

    /// Render a powerline separator (U+E0B0..=E0B3) filling exactly one cell.
    /// kind: 0 = ▶ solid right, 1 = thin right, 2 = ◀ solid left, 3 = thin left.
    fn gen_powerline(&self, ch: char) -> Option<(u32, u32, Vec<u8>)> {
        let kind: u8 = match ch {
            '\u{E0B0}' => 0,
            '\u{E0B1}' => 1,
            '\u{E0B2}' => 2,
            '\u{E0B3}' => 3,
            _ => return None,
        };
        let w = self.cell_w.ceil() as u32;
        let h = self.cell_h.ceil() as u32;
        let (wf, hf) = (w as f32, h as f32);
        let slope = hf / (2.0 * wf); // edge slope from apex
        let thick = (wf * 0.16).max(1.3); // line width for thin variants

        let mut data = vec![0u8; (w * h) as usize];
        for py in 0..h {
            for px in 0..w {
                let mut cov = 0u32;
                // 3x3 supersampling for anti-aliasing.
                for sy in 0..3 {
                    for sx in 0..3 {
                        let x = px as f32 + (sx as f32 + 0.5) / 3.0;
                        let y = py as f32 + (sy as f32 + 0.5) / 3.0;
                        if powerline_hit(kind, x, y, wf, hf, slope, thick) {
                            cov += 1;
                        }
                    }
                }
                data[(py * w + px) as usize] = (cov * 255 / 9) as u8;
            }
        }
        Some((w, h, data))
    }
}

/// Distance from point p to segment a-b.
fn dist_seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (vx, vy) = (bx - ax, by - ay);
    let (wx, wy) = (px - ax, py - ay);
    let len2 = vx * vx + vy * vy;
    let t = if len2 > 0.0 {
        ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (ax + t * vx, ay + t * vy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

fn powerline_hit(kind: u8, x: f32, y: f32, w: f32, h: f32, slope: f32, thick: f32) -> bool {
    match kind {
        // ▶ solid right: base on left edge, apex at right-middle.
        0 => y >= slope * x && y <= h - slope * x,
        // ◀ solid left: base on right edge, apex at left-middle.
        2 => {
            let xr = w - x;
            y >= slope * xr && y <= h - slope * xr
        }
        // thin right: two edges (0,0)->(w,h/2) and (0,h)->(w,h/2).
        1 => {
            dist_seg(x, y, 0.0, 0.0, w, h / 2.0) <= thick
                || dist_seg(x, y, 0.0, h, w, h / 2.0) <= thick
        }
        // thin left: edges (w,0)->(0,h/2) and (w,h)->(0,h/2).
        3 => {
            dist_seg(x, y, w, 0.0, 0.0, h / 2.0) <= thick
                || dist_seg(x, y, w, h, 0.0, h / 2.0) <= thick
        }
        _ => false,
    }
}
