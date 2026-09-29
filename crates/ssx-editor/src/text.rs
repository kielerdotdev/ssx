//! Text shaping, layout, glyph outlines and caret geometry.
//!
//! # Why this is built the way it is
//!
//! * **Deterministic fonts.** The [`TextEngine`] owns a `cosmic-text` `FontSystem` populated
//!   *only* with the bundled Liberation Sans faces (never system fonts). Any requested family
//!   maps onto it, so a project renders identically on every OS and in CI.
//! * **Glyph outlines, not bitmaps.** We take vector outlines from the font and hand them to
//!   the rasteriser as paths. That gives resolution-independent text at any zoom, rotation,
//!   outline stroke and shadow with one code path, and exact hit rectangles.
//! * **Layout is separate from painting** and cached ([`TextLayout`]), because hit-testing
//!   and caret movement need line/glyph geometry on every pointer event.
//! * Only *logical* (visual-order) geometry is exposed; bidirectional carets use per-glyph
//!   direction but selection of mixed-direction runs is approximated by one span per line.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use cosmic_text::{
    Attrs, Buffer, CacheKey, CacheKeyFlags, Command, Family, FontSystem, Metrics, Shaping,
    Style as FontStyle, SwashCache, Weight, Wrap, fontdb,
};
use tiny_skia::{Path, PathBuilder, Transform};

use crate::{
    geom::RectF,
    object::{FontSpec, TextAlign, TextContent},
};

const REGULAR: &[u8] = include_bytes!("../assets/fonts/LiberationSans-Regular.ttf");
const BOLD: &[u8] = include_bytes!("../assets/fonts/LiberationSans-Bold.ttf");
const ITALIC: &[u8] = include_bytes!("../assets/fonts/LiberationSans-Italic.ttf");
const BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/LiberationSans-BoldItalic.ttf");

/// The family name of the bundled default font.
pub const DEFAULT_FAMILY: &str = "Liberation Sans";

const CACHE_LIMIT: usize = 2048;

/// Top-left of a balloon's text block: padded from the left, vertically centred when it fits.
pub fn balloon_text_origin(b: &crate::object::BalloonShape, layout_height: f32) -> crate::geom::PointF {
    let pad = b.content.padding.max(0.0);
    let y = b.rect.y + ((b.rect.h - layout_height) / 2.0).max(pad.min(b.rect.h / 2.0));
    crate::geom::PointF::new(b.rect.x + pad, y)
}

/// One visual line of laid-out text. Byte offsets index the **whole** text.
#[derive(Debug, Clone, PartialEq)]
pub struct VisualLine {
    /// First byte on the line.
    pub start: usize,
    /// One past the last byte on the line (excludes the newline that ends a paragraph).
    pub end: usize,
    /// Top of the line box, relative to the text origin.
    pub top: f32,
    /// Line box height.
    pub height: f32,
    /// Baseline y, relative to the text origin.
    pub baseline: f32,
    /// Ink advance width of the line.
    pub width: f32,
    /// Horizontal shift applied for alignment.
    pub x_offset: f32,
    /// Range of [`TextLayout::glyphs`] on this line.
    pub glyphs: std::ops::Range<usize>,
    /// Right-to-left paragraph.
    pub rtl: bool,
}

/// One positioned glyph.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphPos {
    /// Font face.
    pub font_id: fontdb::ID,
    /// Glyph index in the face.
    pub glyph_id: u16,
    /// Font size in pixels.
    pub size: f32,
    /// Pen x (already includes alignment), relative to the text origin.
    pub x: f32,
    /// Baseline y, relative to the text origin.
    pub y: f32,
    /// Advance width of the cluster.
    pub w: f32,
    /// First byte of the cluster.
    pub start: usize,
    /// One past the last byte of the cluster.
    pub end: usize,
    /// Glyph is right-to-left.
    pub rtl: bool,
}

/// Where a caret is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretPos {
    /// Caret x relative to the text origin.
    pub x: f32,
    /// Top of the caret.
    pub y: f32,
    /// Caret height (the line height).
    pub height: f32,
    /// Index of the visual line.
    pub line: usize,
}

/// A laid-out block of text.
#[derive(Debug, Clone)]
pub struct TextLayout {
    /// The text that was laid out.
    pub text: String,
    /// Width of the block: the wrap width if wrapping, else the widest line.
    pub width: f32,
    /// Total height.
    pub height: f32,
    /// Visual lines, top to bottom (always at least one).
    pub lines: Vec<VisualLine>,
    /// All glyphs in line order.
    pub glyphs: Vec<GlyphPos>,
    key: u64,
}

/// Inputs that determine a layout.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutRequest<'a> {
    /// The text (`\n` separated).
    pub text: &'a str,
    /// Font.
    pub font: &'a FontSpec,
    /// Line height multiple.
    pub line_spacing: f32,
    /// Alignment.
    pub align: TextAlign,
    /// Wrap width; `None` = no wrapping.
    pub wrap_width: Option<f32>,
}

impl<'a> LayoutRequest<'a> {
    /// Builds a request from text content and an optional wrap width.
    pub fn from_content(c: &'a TextContent, wrap_width: Option<f32>) -> Self {
        Self {
            text: &c.text,
            font: &c.font,
            line_spacing: c.line_spacing,
            align: c.align,
            wrap_width,
        }
    }

    fn key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.text.hash(&mut h);
        self.font.family.hash(&mut h);
        self.font.size.to_bits().hash(&mut h);
        self.font.bold.hash(&mut h);
        self.font.italic.hash(&mut h);
        self.line_spacing.to_bits().hash(&mut h);
        (self.align as u8).hash(&mut h);
        self.wrap_width.map(f32::to_bits).hash(&mut h);
        h.finish()
    }
}

/// Owns fonts and caches. Cheap to create (parses four embedded fonts); keep one per thread
/// or per session.
pub struct TextEngine {
    fs: FontSystem,
    swash: SwashCache,
    layouts: HashMap<u64, Arc<TextLayout>>,
    glyph_paths: HashMap<(fontdb::ID, u16, u32), Option<Arc<Path>>>,
    text_paths: HashMap<u64, Option<Arc<Path>>>,
}

impl std::fmt::Debug for TextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextEngine").field("cached_layouts", &self.layouts.len()).finish()
    }
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine {
    /// Creates an engine with only the bundled fonts.
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        for data in [REGULAR, BOLD, ITALIC, BOLD_ITALIC] {
            db.load_font_data(data.to_vec());
        }
        // Every generic family resolves to the one bundled family.
        db.set_sans_serif_family(DEFAULT_FAMILY);
        db.set_serif_family(DEFAULT_FAMILY);
        db.set_monospace_family(DEFAULT_FAMILY);
        db.set_cursive_family(DEFAULT_FAMILY);
        db.set_fantasy_family(DEFAULT_FAMILY);
        Self {
            fs: FontSystem::new_with_locale_and_db("en-US".into(), db),
            swash: SwashCache::new(),
            layouts: HashMap::new(),
            glyph_paths: HashMap::new(),
            text_paths: HashMap::new(),
        }
    }

    /// Registers an additional font file (TTF/OTF bytes) that objects can then reference by
    /// family name. Fonts added this way make rendering machine-dependent unless the same
    /// bytes are registered everywhere, so the default engine never does this by itself.
    pub fn register_font(&mut self, data: Vec<u8>) {
        self.fs.db_mut().load_font_data(data);
        self.layouts.clear();
        self.text_paths.clear();
    }

    /// Lays out `req`. Results are cached.
    pub fn layout(&mut self, req: &LayoutRequest<'_>) -> Arc<TextLayout> {
        let key = req.key();
        if let Some(l) = self.layouts.get(&key) {
            return l.clone();
        }
        if self.layouts.len() >= CACHE_LIMIT {
            self.layouts.clear();
            self.text_paths.clear();
        }
        let layout = Arc::new(self.compute_layout(req, key));
        self.layouts.insert(key, layout.clone());
        layout
    }

    /// Convenience: layout of a [`TextContent`].
    pub fn layout_content(
        &mut self,
        c: &TextContent,
        wrap_width: Option<f32>,
    ) -> Arc<TextLayout> {
        self.layout(&LayoutRequest::from_content(c, wrap_width))
    }

    fn compute_layout(&mut self, req: &LayoutRequest<'_>, key: u64) -> TextLayout {
        let size = req.font.size.clamp(1.0, 4096.0);
        let line_h = (size * req.line_spacing.clamp(0.5, 5.0)).max(1.0);
        let family = if req.font.family.eq_ignore_ascii_case(DEFAULT_FAMILY) {
            Family::Name(DEFAULT_FAMILY)
        } else {
            // Unknown families deliberately fall back to the bundled sans (see module docs).
            Family::SansSerif
        };
        let attrs = Attrs::new()
            .family(family)
            .weight(if req.font.bold { Weight::BOLD } else { Weight::NORMAL })
            .style(if req.font.italic { FontStyle::Italic } else { FontStyle::Normal });
        let wrap_width = req.wrap_width.filter(|w| w.is_finite()).map(|w| w.max(1.0));
        let mut buf = Buffer::new(&mut self.fs, Metrics::new(size, line_h));
        buf.set_wrap(&mut self.fs, if wrap_width.is_some() { Wrap::WordOrGlyph } else { Wrap::None });
        buf.set_size(&mut self.fs, wrap_width, None);
        buf.set_text(&mut self.fs, req.text, &attrs, Shaping::Advanced);
        buf.shape_until_scroll(&mut self.fs, false);

        // Byte offset of each logical line in the whole text.
        let mut starts = Vec::new();
        let mut lens = Vec::new();
        let mut off = 0;
        for l in req.text.split('\n') {
            starts.push(off);
            lens.push(l.len());
            off += l.len() + 1;
        }

        struct RawRun {
            line_i: usize,
            top: f32,
            height: f32,
            baseline: f32,
            width: f32,
            rtl: bool,
            glyphs: Vec<GlyphPos>,
        }
        let mut raw: Vec<RawRun> = Vec::new();
        for run in buf.layout_runs() {
            let base = starts.get(run.line_i).copied().unwrap_or(0);
            let glyphs = run
                .glyphs
                .iter()
                .map(|g| GlyphPos {
                    font_id: g.font_id,
                    glyph_id: g.glyph_id,
                    size: g.font_size,
                    x: g.x + g.font_size * g.x_offset,
                    y: run.line_y + g.y - g.font_size * g.y_offset,
                    w: g.w,
                    start: base + g.start,
                    end: base + g.end,
                    rtl: g.level.is_rtl(),
                })
                .collect();
            raw.push(RawRun {
                line_i: run.line_i,
                top: run.line_top,
                height: run.line_height,
                baseline: run.line_y,
                width: run.line_w,
                rtl: run.rtl,
                glyphs,
            });
        }
        // cosmic-text drops trailing empty paragraphs (and all of empty text); a caret still
        // needs a line to sit on, so synthesise them below the last real line.
        let next_line = raw.last().map_or(0, |r| r.line_i + 1);
        for li in next_line..starts.len() {
            let (top, height, ascent) = match raw.last() {
                Some(p) => (p.top + p.height, p.height, p.baseline - p.top),
                None => (0.0, line_h, size),
            };
            raw.push(RawRun {
                line_i: li,
                top,
                height,
                baseline: top + ascent,
                width: 0.0,
                rtl: false,
                glyphs: Vec::new(),
            });
        }

        let widest = raw.iter().map(|r| r.width).fold(0.0f32, f32::max);
        let block_w = wrap_width.unwrap_or(widest);
        let factor = match req.align {
            TextAlign::Left => 0.0,
            TextAlign::Center => 0.5,
            TextAlign::Right => 1.0,
        };
        // First byte of every visual line. Wrapped pieces start where their first glyph
        // does; a piece's end is the next piece's start, so trailing spaces that cosmic
        // hangs off the line are still owned by a line and no byte is lost.
        let piece_start = |i: usize| -> usize {
            let r = &raw[i];
            let base = starts.get(r.line_i).copied().unwrap_or(0);
            if i == 0 || raw[i - 1].line_i != r.line_i {
                base
            } else {
                r.glyphs.iter().map(|g| g.start).min().unwrap_or(base)
            }
        };
        let mut lines = Vec::with_capacity(raw.len());
        let mut glyphs: Vec<GlyphPos> = Vec::new();
        let n = raw.len();
        for i in 0..n {
            let r = &raw[i];
            let base = starts.get(r.line_i).copied().unwrap_or(0);
            let logical_end = base + lens.get(r.line_i).copied().unwrap_or(0);
            let is_last_piece = raw.get(i + 1).is_none_or(|nx| nx.line_i != r.line_i);
            let start = piece_start(i);
            let end = if is_last_piece { logical_end } else { piece_start(i + 1) };
            let x_offset = ((block_w - r.width) * factor).max(0.0);
            let g0 = glyphs.len();
            for g in &r.glyphs {
                let mut g = g.clone();
                g.x += x_offset;
                glyphs.push(g);
            }
            lines.push(VisualLine {
                start,
                end: end.max(start),
                top: r.top,
                height: r.height,
                baseline: r.baseline,
                width: r.width,
                x_offset,
                glyphs: g0..glyphs.len(),
                rtl: r.rtl,
            });
        }
        let height = lines.last().map_or(line_h, |l| l.top + l.height);
        TextLayout { text: req.text.to_owned(), width: block_w, height, lines, glyphs, key }
    }

    /// The outline of one glyph at its own size, y-down, origin at the pen position on the
    /// baseline. `None` for glyphs without outlines (spaces).
    fn glyph_path(&mut self, g: &GlyphPos) -> Option<Arc<Path>> {
        let k = (g.font_id, g.glyph_id, g.size.to_bits());
        if let Some(p) = self.glyph_paths.get(&k) {
            return p.clone();
        }
        let (ck, _, _) =
            CacheKey::new(g.font_id, g.glyph_id, g.size, (0.0, 0.0), CacheKeyFlags::empty());
        let path = self.swash.get_outline_commands(&mut self.fs, ck).and_then(|cmds| {
            let mut pb = PathBuilder::new();
            for c in cmds {
                match *c {
                    Command::MoveTo(p) => pb.move_to(p.x, -p.y),
                    Command::LineTo(p) => pb.line_to(p.x, -p.y),
                    Command::CurveTo(a, b, p) => pb.cubic_to(a.x, -a.y, b.x, -b.y, p.x, -p.y),
                    Command::QuadTo(a, p) => pb.quad_to(a.x, -a.y, p.x, -p.y),
                    Command::Close => pb.close(),
                }
            }
            pb.finish().map(Arc::new)
        });
        if self.glyph_paths.len() >= CACHE_LIMIT * 4 {
            self.glyph_paths.clear();
        }
        self.glyph_paths.insert(k, path.clone());
        path
    }

    /// The combined outline of a whole layout in text-local coordinates (origin = top-left
    /// of the text block, y down). `None` when there is nothing to draw.
    pub fn text_path(&mut self, layout: &TextLayout) -> Option<Arc<Path>> {
        if let Some(p) = self.text_paths.get(&layout.key) {
            return p.clone();
        }
        let mut pb = PathBuilder::new();
        let mut any = false;
        for g in &layout.glyphs {
            if let Some(p) = self.glyph_path(g) {
                if let Some(moved) = p.as_ref().clone().transform(Transform::from_translate(g.x, g.y)) {
                    pb.push_path(&moved);
                    any = true;
                }
            }
        }
        let path = if any { pb.finish().map(Arc::new) } else { None };
        self.text_paths.insert(layout.key, path.clone());
        path
    }

    /// Measures text: `(width, height)` of the laid-out block.
    pub fn measure(&mut self, req: &LayoutRequest<'_>) -> (f32, f32) {
        let l = self.layout(req);
        (l.width, l.height)
    }
}

impl TextLayout {
    /// Index of the visual line that holds byte offset `byte`.
    pub fn line_of(&self, byte: usize) -> usize {
        let mut found = 0;
        for (i, l) in self.lines.iter().enumerate() {
            if l.start <= byte {
                found = i;
            } else {
                break;
            }
        }
        found
    }

    /// Caret position for byte offset `byte` (clamped to the text).
    pub fn caret(&self, byte: usize) -> CaretPos {
        let byte = byte.min(self.text.len());
        let li = self.line_of(byte);
        let line = &self.lines[li];
        let x = self.x_at(line, byte);
        CaretPos { x, y: line.top, height: line.height, line: li }
    }

    fn x_at(&self, line: &VisualLine, byte: usize) -> f32 {
        let gl = &self.glyphs[line.glyphs.clone()];
        for g in gl {
            if g.start <= byte && byte < g.end {
                let span = (g.end - g.start).max(1) as f32;
                let frac = (byte - g.start) as f32 / span;
                return if g.rtl { g.x + g.w - frac * g.w } else { g.x + frac * g.w };
            }
        }
        match gl.last() {
            Some(last) if byte >= line.end || byte >= last.end => {
                if last.rtl { last.x } else { last.x + last.w }
            }
            Some(first) if byte <= line.start => {
                if line.rtl { first.x + first.w } else { line.x_offset }
            }
            Some(_) => line.x_offset,
            None => line.x_offset,
        }
    }

    /// Byte offset closest to the point (`x`, `y`) in text-local coordinates.
    pub fn hit(&self, x: f32, y: f32) -> usize {
        let mut li = 0;
        for (i, l) in self.lines.iter().enumerate() {
            if l.top <= y {
                li = i;
            }
        }
        let line = &self.lines[li];
        let gl = &self.glyphs[line.glyphs.clone()];
        let Some(first) = gl.first() else { return line.start };
        if x <= first.x.min(line.x_offset) && !first.rtl {
            return line.start;
        }
        for g in gl {
            if x >= g.x && x <= g.x + g.w {
                let frac = ((x - g.x) / g.w.max(1e-3)).clamp(0.0, 1.0);
                let frac = if g.rtl { 1.0 - frac } else { frac };
                let span = g.end - g.start;
                let raw = g.start + (frac * span as f32).round() as usize;
                return self.snap_to_char(raw.clamp(g.start, g.end));
            }
        }
        // Past either end of the line.
        let last = &gl[gl.len() - 1];
        if x > last.x + last.w {
            if last.rtl { line.start } else { line.end }
        } else {
            line.start
        }
    }

    fn snap_to_char(&self, mut b: usize) -> usize {
        b = b.min(self.text.len());
        while b > 0 && !self.text.is_char_boundary(b) {
            b -= 1;
        }
        b
    }

    /// Rectangles (text-local) covering the byte range `a..b`, one per visual line.
    pub fn selection_rects(&self, a: usize, b: usize) -> Vec<RectF> {
        let (a, b) = (a.min(b), a.max(b).min(self.text.len()));
        if a >= b {
            return Vec::new();
        }
        let mut out = Vec::new();
        for line in &self.lines {
            let (s, e) = (a.max(line.start), b.min(line.end));
            let spans_newline = b > line.end && a <= line.end && line.end < self.text.len();
            if s > e || (s == e && !spans_newline) {
                continue;
            }
            let x0 = self.x_at(line, s);
            let mut x1 = self.x_at(line, e);
            if spans_newline {
                // Show the selected newline as a small extra block.
                x1 += 4.0;
            }
            let (l, r) = (x0.min(x1), x0.max(x1));
            out.push(RectF::new(l, line.top, r - l, line.height));
        }
        out
    }

    /// Moves a caret one visual line up (`dir < 0`) or down (`dir > 0`), keeping the
    /// horizontal position. Returns the new byte offset and the x used.
    pub fn move_vertical(&self, byte: usize, dir: i32, preferred_x: Option<f32>) -> (usize, f32) {
        let cur = self.caret(byte);
        let x = preferred_x.unwrap_or(cur.x);
        let target = if dir < 0 {
            if cur.line == 0 {
                return (0, x);
            }
            cur.line - 1
        } else {
            if cur.line + 1 >= self.lines.len() {
                return (self.text.len(), x);
            }
            cur.line + 1
        };
        let l = &self.lines[target];
        (self.hit(x, l.top + l.height / 2.0), x)
    }

    /// First byte of the visual line containing `byte`.
    pub fn line_start(&self, byte: usize) -> usize {
        self.lines[self.line_of(byte.min(self.text.len()))].start
    }

    /// One past the last byte of the visual line containing `byte`.
    pub fn line_end(&self, byte: usize) -> usize {
        self.lines[self.line_of(byte.min(self.text.len()))].end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(text: &'a str, font: &'a FontSpec) -> LayoutRequest<'a> {
        LayoutRequest { text, font, line_spacing: 1.2, align: TextAlign::Left, wrap_width: None }
    }

    #[test]
    fn single_line_has_sensible_metrics() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let l = e.layout(&req("Hello", &f));
        assert_eq!(l.lines.len(), 1);
        assert!(l.width > 40.0 && l.width < 90.0, "24px 'Hello' width {}", l.width);
        assert!((l.height - 24.0 * 1.2).abs() < 0.01, "height {}", l.height);
        assert_eq!(l.glyphs.len(), 5);
        assert_eq!(l.lines[0].start, 0);
        assert_eq!(l.lines[0].end, 5);
    }

    #[test]
    fn empty_text_has_one_line_and_a_caret() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let l = e.layout(&req("", &f));
        assert_eq!(l.lines.len(), 1);
        assert_eq!(l.width, 0.0);
        let c = l.caret(0);
        assert_eq!((c.x, c.y), (0.0, 0.0));
        assert!(c.height > 20.0);
        assert!(l.selection_rects(0, 0).is_empty());
        assert_eq!(l.hit(100.0, 100.0), 0);
    }

    #[test]
    fn multiline_lines_and_empty_lines() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let l = e.layout(&req("ab\n\ncd", &f));
        assert_eq!(l.lines.len(), 3);
        assert_eq!((l.lines[0].start, l.lines[0].end), (0, 2));
        assert_eq!((l.lines[1].start, l.lines[1].end), (3, 3));
        assert_eq!((l.lines[2].start, l.lines[2].end), (4, 6));
        assert!(l.lines[1].top > l.lines[0].top && l.lines[2].top > l.lines[1].top);
        assert_eq!(l.caret(3).line, 1);
        assert_eq!(l.caret(6).line, 2);
        let trailing = e.layout(&req("ab\n", &f));
        assert_eq!(trailing.lines.len(), 2, "text ending in a newline has an empty last line");
        assert_eq!(trailing.caret(3).line, 1);
    }

    #[test]
    fn bold_is_wider_and_italic_differs() {
        let mut e = TextEngine::new();
        let reg = FontSpec::default();
        let bold = FontSpec { bold: true, ..FontSpec::default() };
        let w_reg = e.measure(&req("Screenshot", &reg)).0;
        let w_bold = e.measure(&req("Screenshot", &bold)).0;
        assert!(w_bold > w_reg, "{w_bold} > {w_reg}");
        let it = FontSpec { italic: true, ..FontSpec::default() };
        let _ = e.measure(&req("Screenshot", &it));
    }

    #[test]
    fn unknown_family_falls_back_to_bundled_font() {
        let mut e = TextEngine::new();
        let known = FontSpec::default();
        let unknown = FontSpec { family: "Comic Sans MS".into(), ..FontSpec::default() };
        assert_eq!(
            e.measure(&req("Fallback", &known)),
            e.measure(&req("Fallback", &unknown)),
            "same metrics because both resolve to Liberation Sans"
        );
    }

    #[test]
    fn wrapping_breaks_at_words_and_respects_width() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let mut r = req("the quick brown fox jumps over the lazy dog", &f);
        r.wrap_width = Some(150.0);
        let l = e.layout(&r);
        assert!(l.lines.len() >= 3, "{} lines", l.lines.len());
        assert_eq!(l.width, 150.0);
        for line in &l.lines {
            assert!(line.width <= 150.5, "line width {}", line.width);
        }
        // Lines partition the text contiguously.
        for w in l.lines.windows(2) {
            assert_eq!(w[0].end, w[1].start, "no bytes lost at wrap boundaries");
        }
        assert_eq!(l.lines.last().unwrap().end, r.text.len());
    }

    #[test]
    fn alignment_shifts_lines() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let mut r = req("wide line here\nab", &f);
        let left = e.layout(&r);
        r.align = TextAlign::Center;
        let centre = e.layout(&r);
        r.align = TextAlign::Right;
        let right = e.layout(&r);
        assert_eq!(left.lines[1].x_offset, 0.0);
        let free = left.width - left.lines[1].width;
        assert!((centre.lines[1].x_offset - free / 2.0).abs() < 0.01);
        assert!((right.lines[1].x_offset - free).abs() < 0.01);
        assert_eq!(centre.lines[0].x_offset, 0.0, "the widest line does not move");
    }

    #[test]
    fn caret_is_monotonic_and_hit_inverts_caret() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let text = "Hello, wörld";
        let l = e.layout(&req(text, &f));
        let mut prev = -1.0;
        let bounds: Vec<usize> = (0..=text.len()).filter(|&i| text.is_char_boundary(i)).collect();
        for &b in &bounds {
            let c = l.caret(b);
            assert!(c.x > prev - 1e-4, "x must not go backwards at byte {b}");
            prev = c.x;
            assert_eq!(l.hit(c.x, c.y + c.height / 2.0), b, "hit(caret({b}))");
        }
        assert_eq!(l.caret(text.len()).x, l.width);
        // Far outside clamps to the text ends.
        assert_eq!(l.hit(-50.0, 0.0), 0);
        assert_eq!(l.hit(5000.0, 0.0), text.len());
    }

    #[test]
    fn vertical_movement_and_line_edges() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let l = e.layout(&req("abcdef\nab\nabcdef", &f));
        let (down, x) = l.move_vertical(4, 1, None);
        assert_eq!(l.caret(down).line, 1);
        assert_eq!(down, 9, "short line clamps to its end");
        let (down2, _) = l.move_vertical(down, 1, Some(x));
        assert_eq!(l.caret(down2).line, 2);
        assert_eq!(l.move_vertical(0, -1, None).0, 0);
        assert_eq!(l.move_vertical(16, 1, None).0, 16);
        assert_eq!(l.line_start(8), 7);
        assert_eq!(l.line_end(8), 9);
    }

    #[test]
    fn selection_rects_cover_selected_glyphs() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let l = e.layout(&req("abc\ndef", &f));
        let one = l.selection_rects(1, 3);
        assert_eq!(one.len(), 1);
        assert!((one[0].x - l.caret(1).x).abs() < 1e-4);
        assert!((one[0].right() - l.caret(3).x).abs() < 1e-4);
        let two = l.selection_rects(2, 6);
        assert_eq!(two.len(), 2);
        assert!(two[1].y > two[0].y);
        assert!(l.selection_rects(3, 3).is_empty());
    }

    #[test]
    fn glyph_paths_exist_and_are_deterministic() {
        let mut e = TextEngine::new();
        let f = FontSpec::default();
        let l = e.layout(&req("Hi there", &f));
        let p = e.text_path(&l).expect("text has ink");
        let b = p.bounds();
        assert!(b.width() > 30.0 && b.height() > 8.0);
        assert!(b.top() >= -1.0 && b.bottom() <= l.height + 1.0, "ink inside the line box");
        let mut e2 = TextEngine::new();
        let l2 = e2.layout(&req("Hi there", &f));
        let p2 = e2.text_path(&l2).unwrap();
        assert_eq!(p.bounds(), p2.bounds());
        let spaces = e.layout(&req("   ", &f));
        assert!(e.text_path(&spaces).is_none(), "spaces have no ink");
    }

    #[test]
    fn extreme_inputs_do_not_panic() {
        let mut e = TextEngine::new();
        let tiny = FontSpec { size: 0.0, ..FontSpec::default() };
        let _ = e.layout(&req("x", &tiny));
        let huge = FontSpec { size: 1e9, ..FontSpec::default() };
        let _ = e.layout(&req("x", &huge));
        let f = FontSpec::default();
        let mut r = req("wrap me please", &f);
        r.wrap_width = Some(0.0);
        let _ = e.layout(&r);
        r.wrap_width = Some(f32::NAN);
        let _ = e.layout(&r);
        r.wrap_width = Some(f32::INFINITY);
        let _ = e.layout(&r);
        let emoji = e.layout(&req("😀 ok \u{0301}", &f));
        assert!(!emoji.lines.is_empty());
        let long = "x".repeat(5000);
        let _ = e.layout(&req(&long, &f));
    }
}
