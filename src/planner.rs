//! SVG -> laser job planning: rendering, dithering, scanline extraction
//! (raster engraving), path flattening (vector cut/engrave) and color-based
//! assignment of SVG colors to operations.

use crate::egv::{strip_redundant_codes, ECoord, Egv};
use crate::laser_speed::Board;
use resvg::tiny_skia;
use resvg::usvg;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

const MM_PER_INCH: f64 = 25.4;

/// Usable bed size (K40). The head cannot travel outside this area, so any
/// design content placed beyond it (e.g. a negative placement offset) is
/// clipped away during planning instead of being sent to the controller.
pub const BED_W_MM: f64 = 300.0;
pub const BED_H_MM: f64 = 200.0;

/// usvg options with system fonts loaded (once), so <text> elements work.
fn svg_options() -> usvg::Options<'static> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let db = DB
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone();
    usvg::Options {
        fontdb: db,
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobMode {
    Raster,
    VectorEngrave,
    VectorCut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Raster,
    VectorEngrave,
    VectorCut,
    Ignore,
}

/// Assignment of one SVG color to an operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorRule {
    /// "#rrggbb"
    pub color: String,
    pub op: OpKind,
}

/// Clip window in mm relative to the (scaled) design's top-left corner.
/// Anything outside is dropped from the output entirely — used to keep the
/// engraving within the physical workpiece.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClipRect {
    pub x_mm: f64,
    pub y_mm: f64,
    pub w_mm: f64,
    pub h_mm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareSettings {
    /// Legacy single-mode field; used only when `color_rules` is empty.
    #[serde(default)]
    pub mode: Option<JobMode>,
    /// Color -> operation assignments. Colors without a rule default to raster.
    #[serde(default)]
    pub color_rules: Vec<ColorRule>,
    /// Raster feed rate in mm/s
    pub feed_mmps: f64,
    /// Vector engrave feed rate in mm/s
    #[serde(default = "default_engrave_feed")]
    pub engrave_feed_mmps: f64,
    /// Vector cut feed rate in mm/s
    #[serde(default = "default_cut_feed")]
    pub cut_feed_mmps: f64,
    /// Raster passes (baked into the job, run without re-homing)
    pub passes: u32,
    #[serde(default = "one")]
    pub engrave_passes: u32,
    #[serde(default = "one")]
    pub cut_passes: u32,
    /// Raster line spacing in mils (1/1000 inch); 2 = 500 lines per inch
    #[serde(default = "default_raster_step")]
    pub raster_step_mils: i64,
    /// Horizontal render resolution in DPI for raster mode
    #[serde(default = "default_render_dpi")]
    pub render_dpi: f64,
    /// Floyd-Steinberg dithering (true) or 50% threshold (false)
    #[serde(default)]
    pub dither: bool,
    /// Invert dark/light before engraving
    #[serde(default)]
    pub invert: bool,
    /// Engrave bottom-up (positive raster step / NLB): the Y axis only steps
    /// upward while engraving — useful when a machine has trouble stepping
    /// reliably in one Y direction. K40 Whisperer's "Engrave Bottom Up".
    #[serde(default)]
    pub bottom_up: bool,
    /// Placement offset from home position, mm to the right
    #[serde(default)]
    pub offset_x_mm: f64,
    /// Placement offset from home position, mm downward
    #[serde(default)]
    pub offset_y_mm: f64,
    /// Uniform scale factor applied to the design
    #[serde(default = "default_scale")]
    pub scale: f64,
    /// Optional clip window (workpiece boundary); parts of the design
    /// outside this rectangle are not lasered and not emitted at all.
    #[serde(default)]
    pub clip: Option<ClipRect>,
}

fn default_raster_step() -> i64 {
    2
}
fn default_render_dpi() -> f64 {
    500.0
}
fn default_scale() -> f64 {
    1.0
}
fn default_engrave_feed() -> f64 {
    30.0
}
fn default_cut_feed() -> f64 {
    10.0
}
fn one() -> u32 {
    1
}

pub struct PlannedJob {
    /// Ready-to-send EGV byte stream (starts with 'I', ends with 'FNSE').
    /// Contains all operations and their per-op passes, joined with '@'.
    pub egv: Vec<u8>,
    /// Design size in mm
    pub width_mm: f64,
    pub height_mm: f64,
    /// Preview of what will be engraved/cut, PNG encoded
    pub preview_png: Vec<u8>,
    /// Rough total duration estimate, seconds
    pub estimate_s: f64,
}

#[derive(Debug, Serialize)]
pub struct PaletteColor {
    pub color: String,
    /// number of SVG paths using this color (fill or stroke)
    pub count: u32,
}

//////////////////////////////////////////////////////////////////////////////
// Palette extraction
//////////////////////////////////////////////////////////////////////////////

fn hex(c: (u8, u8, u8)) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

fn paint_color(paint: &usvg::Paint) -> Option<(u8, u8, u8)> {
    match paint {
        usvg::Paint::Color(c) => Some((c.red, c.green, c.blue)),
        _ => None,
    }
}

fn walk_colors(group: &usvg::Group, counts: &mut HashMap<(u8, u8, u8), u32>) {
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => walk_colors(g, counts),
            usvg::Node::Path(p) => {
                if !p.is_visible() {
                    continue;
                }
                if let Some(f) = p.fill() {
                    if let Some(c) = paint_color(f.paint()) {
                        *counts.entry(c).or_insert(0) += 1;
                    }
                }
                if let Some(s) = p.stroke() {
                    if let Some(c) = paint_color(s.paint()) {
                        *counts.entry(c).or_insert(0) += 1;
                    }
                }
            }
            // text is kept as a Text node; its path representation lives in
            // the flattened group
            usvg::Node::Text(t) => walk_colors(t.flattened(), counts),
            usvg::Node::Image(_) => {}
        }
    }
}

/// Extract the distinct path colors of an SVG, most used first.
pub fn extract_palette(svg_text: &str) -> Result<(Vec<PaletteColor>, f64, f64), String> {
    let opt = svg_options();
    let tree = usvg::Tree::from_str(svg_text, &opt).map_err(|e| format!("SVG-Fehler: {e}"))?;
    let mut counts: HashMap<(u8, u8, u8), u32> = HashMap::new();
    walk_colors(tree.root(), &mut counts);
    let mut colors: Vec<PaletteColor> = counts
        .into_iter()
        .map(|(c, count)| PaletteColor { color: hex(c), count })
        .collect();
    colors.sort_by(|a, b| b.count.cmp(&a.count).then(a.color.cmp(&b.color)));
    colors.truncate(16);
    let w_mm = tree.size().width() as f64 / 96.0 * MM_PER_INCH;
    let h_mm = tree.size().height() as f64 / 96.0 * MM_PER_INCH;
    Ok((colors, w_mm, h_mm))
}

//////////////////////////////////////////////////////////////////////////////
// Planning
//////////////////////////////////////////////////////////////////////////////

struct RuleSet {
    /// exact color -> op
    map: HashMap<(u8, u8, u8), OpKind>,
    /// op for colors without an explicit rule
    default_op: OpKind,
    /// legacy luminance mode: no color classification at all, raster from
    /// grayscale of the full rendering
    luminance_raster: bool,
    /// legacy vector mode: all paths regardless of color
    all_vector: Option<OpKind>,
}

impl RuleSet {
    fn from_settings(settings: &PrepareSettings) -> RuleSet {
        if settings.color_rules.is_empty() {
            match settings.mode.unwrap_or(JobMode::Raster) {
                JobMode::Raster => RuleSet {
                    map: HashMap::new(),
                    default_op: OpKind::Raster,
                    luminance_raster: true,
                    all_vector: None,
                },
                JobMode::VectorEngrave => RuleSet {
                    map: HashMap::new(),
                    default_op: OpKind::VectorEngrave,
                    luminance_raster: false,
                    all_vector: Some(OpKind::VectorEngrave),
                },
                JobMode::VectorCut => RuleSet {
                    map: HashMap::new(),
                    default_op: OpKind::VectorCut,
                    luminance_raster: false,
                    all_vector: Some(OpKind::VectorCut),
                },
            }
        } else {
            let mut map = HashMap::new();
            for rule in &settings.color_rules {
                if let Some(c) = parse_hex(&rule.color) {
                    map.insert(c, rule.op);
                }
            }
            RuleSet {
                map,
                default_op: OpKind::Raster,
                luminance_raster: false,
                all_vector: None,
            }
        }
    }

    fn op_for(&self, color: (u8, u8, u8)) -> OpKind {
        *self.map.get(&color).unwrap_or(&self.default_op)
    }

    fn has_ignores(&self) -> bool {
        self.default_op == OpKind::Ignore || self.map.values().any(|o| *o == OpKind::Ignore)
    }

    fn any_raster(&self) -> bool {
        if self.luminance_raster {
            return true;
        }
        if self.all_vector.is_some() {
            return false;
        }
        // colors not covered by rules default to raster; conservatively true
        // unless every rule is non-raster AND callers find no unruled colors —
        // the cheap answer: raster pass runs and may yield nothing.
        true
    }
}

fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() == 3 {
        let d = |i: usize| u8::from_str_radix(&s[i..i + 1], 16).ok().map(|v| v * 17);
        return Some((d(0)?, d(1)?, d(2)?));
    }
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Remove `<path>` elements whose paints are all "ignored" colors from a
/// canonical SVG produced by `usvg::Tree::to_string` (there, every paint is
/// an explicit `fill="…"`/`stroke="…"` attribute). This keeps ignored shapes
/// from occluding engravable artwork underneath — e.g. a white cover
/// rectangle exported on top of the design.
fn strip_ignored_paths(svg: &str, rules: &RuleSet) -> String {
    fn get_attr<'a>(elem: &'a str, name: &str) -> Option<&'a str> {
        let pat = format!(" {name}=\"");
        let s = elem.find(&pat)? + pat.len();
        let e = elem[s..].find('"')? + s;
        Some(&elem[s..e])
    }
    fn all_paints_ignored(elem: &str, rules: &RuleSet) -> bool {
        let mut any_ignored = false;
        for name in ["fill", "stroke"] {
            match get_attr(elem, name) {
                None => {}
                Some("none") => {}
                Some(v) => match parse_hex(v) {
                    Some(c) if rules.op_for(c) == OpKind::Ignore => any_ignored = true,
                    _ => return false, // visible paint or gradient/pattern: keep
                },
            }
        }
        any_ignored
    }

    let mut out = String::with_capacity(svg.len());
    let mut i = 0usize;
    let mut defs_depth = 0usize; // inside clipPath/mask/pattern: paints are irrelevant
    while i < svg.len() {
        let Some(off) = svg[i..].find('<') else {
            out.push_str(&svg[i..]);
            break;
        };
        out.push_str(&svg[i..i + off]);
        i += off;
        let rest = &svg[i..];
        if rest.starts_with("<clipPath") || rest.starts_with("<mask") || rest.starts_with("<pattern")
        {
            defs_depth += 1;
        } else if rest.starts_with("</clipPath")
            || rest.starts_with("</mask")
            || rest.starts_with("</pattern")
        {
            defs_depth = defs_depth.saturating_sub(1);
        } else if defs_depth == 0 && rest.starts_with("<path") {
            if let Some(end) = rest.find("/>") {
                let elem = &rest[..end + 2];
                if all_paints_ignored(elem, rules) {
                    i += end + 2;
                    continue; // drop the element
                }
            }
        }
        let gt = rest.find('>').map(|g| i + g + 1).unwrap_or(svg.len());
        out.push_str(&svg[i..gt]);
        i = gt;
    }
    out
}

pub fn plan_svg(svg_text: &str, settings: &PrepareSettings) -> Result<PlannedJob, String> {
    let opt = svg_options();
    let tree = usvg::Tree::from_str(svg_text, &opt).map_err(|e| format!("SVG-Fehler: {e}"))?;

    let size = tree.size();
    let scale = settings.scale.clamp(0.01, 50.0);
    let width_mm = size.width() as f64 / 96.0 * MM_PER_INCH * scale;
    let height_mm = size.height() as f64 / 96.0 * MM_PER_INCH * scale;
    if width_mm <= 0.0 || height_mm <= 0.0 {
        return Err("SVG hat keine Größe".to_string());
    }

    // The design may be placed partly outside the bed (negative offset or
    // overhang past the far edge). The head cannot travel there, so whatever
    // lies outside the machine area must never reach the EGV: fold the bed
    // into the clip window.
    let bed = ClipRect {
        x_mm: -settings.offset_x_mm,
        y_mm: -settings.offset_y_mm,
        w_mm: BED_W_MM,
        h_mm: BED_H_MM,
    };
    let overhang = settings.offset_x_mm < 0.0
        || settings.offset_y_mm < 0.0
        || settings.offset_x_mm + width_mm > BED_W_MM
        || settings.offset_y_mm + height_mm > BED_H_MM;
    let effective_clip = match (settings.clip, overhang) {
        (Some(c), true) => Some(intersect_clip(c, bed)),
        (Some(c), false) => Some(c),
        (None, true) => Some(bed),
        (None, false) => None,
    };
    if overhang
        && effective_clip.is_some_and(|c| {
            c.w_mm <= 0.0
                || c.h_mm <= 0.0
                || c.x_mm >= width_mm
                || c.y_mm >= height_mm
                || c.x_mm + c.w_mm <= 0.0
                || c.y_mm + c.h_mm <= 0.0
        })
    {
        return Err("Design liegt vollständig außerhalb der Arbeitsfläche".to_string());
    }
    let settings = &PrepareSettings {
        clip: effective_clip,
        ..settings.clone()
    };

    let rules = RuleSet::from_settings(settings);
    let height_in = height_mm / MM_PER_INCH;
    // Head start: top-left of the design; placement offset baked in.
    let start_x = -(settings.offset_x_mm / MM_PER_INCH);
    let start_y = height_in + settings.offset_y_mm / MM_PER_INCH;

    ////////////////////////////////////////////////////////////
    // Collect vector polylines (in svg px, y down) per op
    ////////////////////////////////////////////////////////////
    let mut cut_lines: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut eng_lines: Vec<Vec<(f64, f64)>> = Vec::new();
    collect_vector(tree.root(), &rules, &mut eng_lines, &mut cut_lines);

    // Workpiece clipping for vector paths (raster is masked in rasterize()).
    // Clip rect is in design mm; polylines are in svg px.
    if let Some(clip) = settings.clip {
        let px_per_mm = 96.0 / (MM_PER_INCH * scale);
        let rect = (
            clip.x_mm * px_per_mm,
            clip.y_mm * px_per_mm,
            (clip.x_mm + clip.w_mm) * px_per_mm,
            (clip.y_mm + clip.h_mm) * px_per_mm,
        );
        cut_lines = clip_polylines(&cut_lines, rect);
        eng_lines = clip_polylines(&eng_lines, rect);
    }

    ////////////////////////////////////////////////////////////
    // Raster scanlines
    ////////////////////////////////////////////////////////////
    let raster = if rules.any_raster() {
        rasterize(&tree, width_mm, height_mm, settings, &rules)?
    } else {
        None
    };

    ////////////////////////////////////////////////////////////
    // Generate EGV blocks and join them (K40W operation order:
    // raster, vector engrave, vector cut)
    ////////////////////////////////////////////////////////////
    let mut data: Vec<u8> = vec![b'I'];
    let mut estimate_s = 0.0f64;
    let mut any = false;

    if let Some(ref r) = raster {
        let mut egv = Egv::new();
        let step = settings.raster_step_mils.max(1);
        egv.make_egv_data(
            &r.ecoords,
            start_x,
            start_y,
            settings.feed_mmps,
            Board::M2,
            if settings.bottom_up { step } else { -step },
        )?;
        let block = strip_redundant_codes(&egv.data);
        let per_pass = r.scan_len_mm / settings.feed_mmps.max(0.1) + r.n_scanlines as f64 * 0.08;
        estimate_s += per_pass * settings.passes.max(1) as f64;
        append_block(&mut data, &block, settings.passes);
        any = true;
    }

    for (lines, feed, passes) in [
        (&eng_lines, settings.engrave_feed_mmps, settings.engrave_passes),
        (&cut_lines, settings.cut_feed_mmps, settings.cut_passes),
    ] {
        if lines.is_empty() {
            continue;
        }
        let (ecoords, len_mm) = polylines_to_ecoords(lines, scale, height_in);
        if ecoords.is_empty() {
            continue;
        }
        let mut egv = Egv::new();
        egv.make_egv_data(&ecoords, start_x, start_y, feed, Board::M2, 0)?;
        estimate_s += (len_mm / feed.max(0.1)) * 1.15 * passes.max(1) as f64;
        append_block(&mut data, &egv.data, passes);
        any = true;
    }

    if !any {
        return Err("Nichts zu tun: keine dunklen Pixel und keine zugeordneten Pfade".to_string());
    }

    ////////////////////////////////////////////////////////////
    // Preview: raster bitmap + vector overlays
    ////////////////////////////////////////////////////////////
    let preview_png = compose_preview(&tree, raster.as_ref(), &eng_lines, &cut_lines)?;

    Ok(PlannedJob {
        egv: data,
        width_mm,
        height_mm,
        preview_png,
        estimate_s,
    })
}

/// Positioning markers: four small L-shaped corner marks on the outer
/// bounding box (x/y offset from home, w×h in mm). Lasered onto a template
/// at low power they make it easy to place the real workpiece precisely.
pub fn plan_markers(
    x_mm: f64,
    y_mm: f64,
    w_mm: f64,
    h_mm: f64,
    feed_mmps: f64,
) -> Result<Vec<u8>, String> {
    if w_mm <= 0.0 || h_mm <= 0.0 {
        return Err("Ungültige Markergröße".to_string());
    }
    // A box may extend beyond the bed (negative placement); the head cannot
    // laser there, so clamp the marker rectangle to the machine area.
    let x1 = (x_mm + w_mm).min(BED_W_MM);
    let y1 = (y_mm + h_mm).min(BED_H_MM);
    let x_mm = x_mm.max(0.0);
    let y_mm = y_mm.max(0.0);
    let (w_mm, h_mm) = (x1 - x_mm, y1 - y_mm);
    if w_mm <= 0.0 || h_mm <= 0.0 {
        return Err("Markerbereich liegt außerhalb der Arbeitsfläche".to_string());
    }
    // arm length: 4 mm, capped for tiny designs
    let arm = 4.0f64.min(w_mm / 3.0).min(h_mm / 3.0).max(0.5);

    let w = w_mm / MM_PER_INCH;
    let h = h_mm / MM_PER_INCH;
    let a = arm / MM_PER_INCH;

    // design space: x right, y up, box occupies [0,w]×[0,h]
    // each corner is one polyline: arm end -> corner -> other arm end
    let corners: [[(f64, f64); 3]; 4] = [
        [(0.0, h - a), (0.0, h), (a, h)],         // top-left
        [(w - a, h), (w, h), (w, h - a)],         // top-right
        [(w, a), (w, 0.0), (w - a, 0.0)],         // bottom-right
        [(a, 0.0), (0.0, 0.0), (0.0, a)],         // bottom-left
    ];
    let mut ecoords: Vec<ECoord> = Vec::new();
    for (i, corner) in corners.iter().enumerate() {
        for &(x, y) in corner {
            ecoords.push(ECoord { x, y, loop_id: (i + 1) as i64 });
        }
    }

    let start_x = -(x_mm / MM_PER_INCH);
    let start_y = h + y_mm / MM_PER_INCH;
    let mut egv = Egv::new();
    egv.make_egv_data(&ecoords, start_x, start_y, feed_mmps.clamp(0.5, 240.0), Board::M2, 0)?;
    let mut data = vec![b'I'];
    data.extend(egv.data);
    Ok(data)
}

/// Intersection of two clip rects; width/height can come out non-positive
/// when they do not overlap.
fn intersect_clip(a: ClipRect, b: ClipRect) -> ClipRect {
    let x0 = a.x_mm.max(b.x_mm);
    let y0 = a.y_mm.max(b.y_mm);
    let x1 = (a.x_mm + a.w_mm).min(b.x_mm + b.w_mm);
    let y1 = (a.y_mm + a.h_mm).min(b.y_mm + b.h_mm);
    ClipRect {
        x_mm: x0,
        y_mm: y0,
        w_mm: x1 - x0,
        h_mm: y1 - y0,
    }
}

/// Append an operation block `passes` times; rewrites the previous footer 'F'
/// to '@' so the controller runs everything as one continuous job without
/// re-homing (K40 Whisperer multi-op/multi-pass joining).
fn append_block(data: &mut Vec<u8>, block: &[u8], passes: u32) {
    for _ in 0..passes.max(1) {
        let n = data.len();
        if n > 4 {
            data[n - 4] = b'@';
        }
        data.extend_from_slice(block);
    }
}

//////////////////////////////////////////////////////////////////////////////
// Raster
//////////////////////////////////////////////////////////////////////////////

struct RasterResult {
    ecoords: Vec<ECoord>,
    dark: Vec<bool>,
    wpx: u32,
    hpx: u32,
    scan_len_mm: f64,
    n_scanlines: u64,
}

fn rasterize(
    tree: &usvg::Tree,
    width_mm: f64,
    height_mm: f64,
    settings: &PrepareSettings,
    rules: &RuleSet,
) -> Result<Option<RasterResult>, String> {
    let dpi = settings.render_dpi.clamp(100.0, 1000.0);
    let wpx = (width_mm / MM_PER_INCH * dpi).round().max(1.0) as u32;
    let hpx = (height_mm / MM_PER_INCH * dpi).round().max(1.0) as u32;
    if wpx.saturating_mul(hpx) > 120_000_000 {
        return Err("Design zu groß für die gewählte Auflösung".to_string());
    }

    // Ignored colors must not occlude engravable artwork underneath them
    // (e.g. a white cover rectangle on top of the design): rebuild the tree
    // without those paths before rendering.
    let filtered_tree;
    let tree = if rules.has_ignores() {
        let canonical = tree.to_string(&usvg::WriteOptions::default());
        let cleaned = strip_ignored_paths(&canonical, rules);
        match usvg::Tree::from_str(&cleaned, &svg_options()) {
            Ok(t) => {
                filtered_tree = t;
                &filtered_tree
            }
            Err(_) => tree, // fall back to the unfiltered tree
        }
    } else {
        tree
    };

    // Render on transparent background so we can distinguish background
    // from white artwork via alpha.
    let mut pixmap = tiny_skia::Pixmap::new(wpx, hpx).ok_or("Pixmap-Fehler")?;
    let render_scale =
        (wpx as f32 / tree.size().width()).min(hpx as f32 / tree.size().height());
    let transform = tiny_skia::Transform::from_scale(render_scale, render_scale);
    resvg::render(tree, transform, &mut pixmap.as_mut());

    // Palette for classification: every path color in the document
    let mut counts: HashMap<(u8, u8, u8), u32> = HashMap::new();
    walk_colors(tree.root(), &mut counts);
    let palette: Vec<(u8, u8, u8)> = counts.keys().copied().collect();

    // Build grayscale: 0 = engrave, 255 = leave
    let mut gray: Vec<f32> = Vec::with_capacity((wpx * hpx) as usize);
    for p in pixmap.pixels() {
        let a = p.alpha();
        let v: f32 = if rules.luminance_raster {
            // classic luminance over white background
            let af = a as f32 / 255.0;
            let r = p.red() as f32 + (1.0 - af) * 255.0;
            let g = p.green() as f32 + (1.0 - af) * 255.0;
            let b = p.blue() as f32 + (1.0 - af) * 255.0;
            0.299 * r + 0.587 * g + 0.114 * b
        } else if a < 128 {
            255.0 // background
        } else {
            // un-premultiply and classify to the nearest palette color
            let r = (p.red() as u32 * 255 / a as u32).min(255) as u8;
            let g = (p.green() as u32 * 255 / a as u32).min(255) as u8;
            let b = (p.blue() as u32 * 255 / a as u32).min(255) as u8;
            let nearest = nearest_color(&palette, (r, g, b));
            match nearest.map(|c| rules.op_for(c)) {
                Some(OpKind::Raster) => {
                    if settings.dither {
                        0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
                    } else {
                        0.0 // solid engrave for assigned colors
                    }
                }
                _ => 255.0,
            }
        };
        gray.push(if settings.invert { 255.0 - v } else { v });
    }

    let mut dark: Vec<bool> = if settings.dither {
        floyd_steinberg(&mut gray, wpx as usize, hpx as usize)
    } else {
        gray.iter().map(|&y| y < 128.0).collect()
    };

    // Workpiece clipping: mask out everything outside the clip window so it
    // is neither engraved nor part of the generated EGV.
    if let Some(clip) = settings.clip {
        let px = |mm: f64| mm / MM_PER_INCH * dpi;
        let cx0 = px(clip.x_mm).floor().max(0.0) as usize;
        let cy0 = px(clip.y_mm).floor().max(0.0) as usize;
        let cx1 = (px(clip.x_mm + clip.w_mm).ceil().max(0.0) as usize).min(wpx as usize);
        let cy1 = (px(clip.y_mm + clip.h_mm).ceil().max(0.0) as usize).min(hpx as usize);
        for y in 0..hpx as usize {
            let row = &mut dark[y * wpx as usize..(y + 1) * wpx as usize];
            if y < cy0 || y >= cy1 {
                row.fill(false);
            } else {
                for (x, d) in row.iter_mut().enumerate() {
                    if x < cx0 || x >= cx1 {
                        *d = false;
                    }
                }
            }
        }
    }

    // Scanlines -> ecoords (port of K40 Whisperer make_raster_coords):
    // y in inches upward from the design bottom; runs of dark pixels become
    // point pairs sharing a loop id.
    let raster_step = settings.raster_step_mils.max(1);
    let im_height_mils = (hpx as f64 / dpi * 1000.0) as i64;
    let mut ecoords: Vec<ECoord> = Vec::new();
    let mut loop_id: i64 = 1;
    let mut scan_len_mils: f64 = 0.0;
    let mut n_scanlines: u64 = 0;

    let mut i_step: i64 = 0;
    while i_step < im_height_mils {
        let row = ((i_step as f64) * dpi / 1000.0).floor() as usize;
        if row >= hpx as usize {
            break;
        }
        let y_in = (im_height_mils - i_step) as f64 / 1000.0;
        let line = &dark[row * wpx as usize..(row + 1) * wpx as usize];

        let mut j = 0usize;
        let mut line_used = false;
        while j < wpx as usize {
            if line[j] {
                let start = j;
                while j < wpx as usize && line[j] {
                    j += 1;
                }
                loop_id += 1;
                ecoords.push(ECoord { x: start as f64 / dpi, y: y_in, loop_id });
                ecoords.push(ECoord { x: j as f64 / dpi, y: y_in, loop_id });
                scan_len_mils += (j - start) as f64 / dpi * 1000.0;
                line_used = true;
            } else {
                j += 1;
            }
        }
        if line_used {
            n_scanlines += 1;
        }
        i_step += raster_step;
    }

    if ecoords.is_empty() {
        return Ok(None);
    }
    Ok(Some(RasterResult {
        ecoords,
        dark,
        wpx,
        hpx,
        scan_len_mm: scan_len_mils / 1000.0 * MM_PER_INCH,
        n_scanlines,
    }))
}

fn nearest_color(palette: &[(u8, u8, u8)], c: (u8, u8, u8)) -> Option<(u8, u8, u8)> {
    let mut best: Option<((u8, u8, u8), i64)> = None;
    for &p in palette {
        let dr = p.0 as i64 - c.0 as i64;
        let dg = p.1 as i64 - c.1 as i64;
        let db = p.2 as i64 - c.2 as i64;
        let d = dr * dr + dg * dg + db * db;
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((p, d));
        }
    }
    best.map(|(p, _)| p)
}

/// Floyd-Steinberg dithering; consumes the grayscale buffer. Returns dark-mask.
fn floyd_steinberg(gray: &mut [f32], w: usize, h: usize) -> Vec<bool> {
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let old = gray[i];
            let new = if old < 128.0 { 0.0 } else { 255.0 };
            out[i] = new == 0.0;
            let err = old - new;
            if x + 1 < w {
                gray[i + 1] += err * 7.0 / 16.0;
            }
            if y + 1 < h {
                if x > 0 {
                    gray[i + w - 1] += err * 3.0 / 16.0;
                }
                gray[i + w] += err * 5.0 / 16.0;
                if x + 1 < w {
                    gray[i + w + 1] += err * 1.0 / 16.0;
                }
            }
        }
    }
    out
}

//////////////////////////////////////////////////////////////////////////////
// Vector collection
//////////////////////////////////////////////////////////////////////////////

fn collect_vector(
    group: &usvg::Group,
    rules: &RuleSet,
    eng: &mut Vec<Vec<(f64, f64)>>,
    cut: &mut Vec<Vec<(f64, f64)>>,
) {
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => collect_vector(g, rules, eng, cut),
            usvg::Node::Path(p) => {
                if !p.is_visible() {
                    continue;
                }
                let op = if let Some(all) = rules.all_vector {
                    all
                } else {
                    // stroke color decides first, then fill color
                    let stroke_op = p
                        .stroke()
                        .and_then(|s| paint_color(s.paint()))
                        .map(|c| rules.op_for(c));
                    let fill_op = p
                        .fill()
                        .and_then(|f| paint_color(f.paint()))
                        .map(|c| rules.op_for(c));
                    match (stroke_op, fill_op) {
                        (Some(OpKind::VectorCut), _) | (_, Some(OpKind::VectorCut)) => {
                            OpKind::VectorCut
                        }
                        (Some(OpKind::VectorEngrave), _) | (_, Some(OpKind::VectorEngrave)) => {
                            OpKind::VectorEngrave
                        }
                        _ => continue,
                    }
                };
                match op {
                    OpKind::VectorCut => flatten_path(p, cut),
                    OpKind::VectorEngrave => flatten_path(p, eng),
                    _ => {}
                }
            }
            usvg::Node::Text(t) => collect_vector(t.flattened(), rules, eng, cut),
            _ => {}
        }
    }
}

/// Convert px polylines (y down) into ecoords (inches, y up). Returns total
/// length in mm.
/// Clip a line segment against an axis-aligned rect (Liang-Barsky).
/// Rect is (x0, y0, x1, y1); returns the clipped segment or None.
fn clip_segment(
    p0: (f64, f64),
    p1: (f64, f64),
    r: (f64, f64, f64, f64),
) -> Option<((f64, f64), (f64, f64))> {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    let checks = [
        (-dx, p0.0 - r.0), // left
        (dx, r.2 - p0.0),  // right
        (-dy, p0.1 - r.1), // top
        (dy, r.3 - p0.1),  // bottom
    ];
    for (p, q) in checks {
        if p == 0.0 {
            if q < 0.0 {
                return None; // parallel and outside
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                if t > t1 {
                    return None;
                }
                if t > t0 {
                    t0 = t;
                }
            } else {
                if t < t0 {
                    return None;
                }
                if t < t1 {
                    t1 = t;
                }
            }
        }
    }
    Some((
        (p0.0 + t0 * dx, p0.1 + t0 * dy),
        (p0.0 + t1 * dx, p0.1 + t1 * dy),
    ))
}

/// Clip polylines to a rect; segments crossing the boundary are cut, parts
/// outside are dropped, and each contiguous inside-run becomes its own line.
fn clip_polylines(lines: &[Vec<(f64, f64)>], r: (f64, f64, f64, f64)) -> Vec<Vec<(f64, f64)>> {
    let eps = 1e-9;
    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    for line in lines {
        let mut cur: Vec<(f64, f64)> = Vec::new();
        for w in line.windows(2) {
            match clip_segment(w[0], w[1], r) {
                Some((a, b)) => {
                    let continues = cur
                        .last()
                        .is_some_and(|&l| (l.0 - a.0).abs() < eps && (l.1 - a.1).abs() < eps);
                    if !continues {
                        if cur.len() > 1 {
                            out.push(std::mem::take(&mut cur));
                        } else {
                            cur.clear();
                        }
                        cur.push(a);
                    }
                    if (b.0 - a.0).abs() > eps || (b.1 - a.1).abs() > eps {
                        cur.push(b);
                    }
                }
                None => {
                    if cur.len() > 1 {
                        out.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                }
            }
        }
        if cur.len() > 1 {
            out.push(cur);
        }
    }
    out
}

fn polylines_to_ecoords(
    polylines: &[Vec<(f64, f64)>],
    scale: f64,
    height_in: f64,
) -> (Vec<ECoord>, f64) {
    let px_to_in = 1.0 / 96.0 * scale;
    let mut ecoords: Vec<ECoord> = Vec::new();
    let mut loop_id: i64 = 0;
    let mut len_mm = 0.0f64;
    for line in polylines {
        if line.len() < 2 {
            continue;
        }
        loop_id += 1;
        let mut last: Option<(f64, f64)> = None;
        for &(px, py) in line {
            let x = px * px_to_in;
            let y = height_in - py * px_to_in;
            ecoords.push(ECoord { x, y, loop_id });
            if let Some((lx, ly)) = last {
                len_mm += (x - lx).hypot(y - ly) * MM_PER_INCH;
            }
            last = Some((x, y));
        }
    }
    (ecoords, len_mm)
}

//////////////////////////////////////////////////////////////////////////////
// Preview
//////////////////////////////////////////////////////////////////////////////

fn compose_preview(
    tree: &usvg::Tree,
    raster: Option<&RasterResult>,
    eng_lines: &[Vec<(f64, f64)>],
    cut_lines: &[Vec<(f64, f64)>],
) -> Result<Vec<u8>, String> {
    // Base: raster bitmap if present, otherwise white canvas
    let (mut pixmap, px_scale) = if let Some(r) = raster {
        let (pm, step) = bits_to_pixmap(&r.dark, r.wpx, r.hpx)?;
        // svg px -> preview px: preview width / svg width
        let s = pm.width() as f32 / tree.size().width();
        let _ = step;
        (pm, s)
    } else {
        let size = tree.size();
        let scale = (1000.0 / size.width().max(size.height())).min(4.0);
        let w = ((size.width() * scale).round() as u32).max(1);
        let h = ((size.height() * scale).round() as u32).max(1);
        let mut pm = tiny_skia::Pixmap::new(w, h).ok_or("Pixmap-Fehler")?;
        pm.fill(tiny_skia::Color::WHITE);
        (pm, scale)
    };

    let mut draw = |lines: &[Vec<(f64, f64)>], color: tiny_skia::Color| {
        if lines.is_empty() {
            return;
        }
        let mut paint = tiny_skia::Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        let stroke = tiny_skia::Stroke {
            width: (px_scale * 1.5).max(1.2),
            ..Default::default()
        };
        for line in lines {
            if line.len() < 2 {
                continue;
            }
            let mut pb = tiny_skia::PathBuilder::new();
            pb.move_to(line[0].0 as f32 * px_scale, line[0].1 as f32 * px_scale);
            for &(x, y) in line.iter().skip(1) {
                pb.line_to(x as f32 * px_scale, y as f32 * px_scale);
            }
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, tiny_skia::Transform::identity(), None);
            }
        }
    };
    draw(eng_lines, tiny_skia::Color::from_rgba8(37, 99, 235, 255)); // blue
    draw(cut_lines, tiny_skia::Color::from_rgba8(220, 38, 38, 255)); // red

    pixmap.encode_png().map_err(|e| format!("PNG-Fehler: {e}"))
}

fn bits_to_pixmap(dark: &[bool], w: u32, h: u32) -> Result<(tiny_skia::Pixmap, u32), String> {
    let max_w = 1200u32;
    let (pw, ph, step) = if w > max_w {
        let step = (w as f64 / max_w as f64).ceil() as u32;
        (w / step, h / step, step)
    } else {
        (w, h, 1)
    };
    let mut pixmap = tiny_skia::Pixmap::new(pw.max(1), ph.max(1)).ok_or("Pixmap-Fehler")?;
    let pixels = pixmap.pixels_mut();
    for py in 0..ph {
        for px in 0..pw {
            let mut any = false;
            'outer: for sy in 0..step {
                for sx in 0..step {
                    let x = px * step + sx;
                    let y = py * step + sy;
                    if x < w && y < h && dark[(y * w + x) as usize] {
                        any = true;
                        break 'outer;
                    }
                }
            }
            let c = if any {
                tiny_skia::PremultipliedColorU8::from_rgba(20, 20, 20, 255).unwrap()
            } else {
                tiny_skia::PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap()
            };
            pixels[(py * pw + px) as usize] = c;
        }
    }
    Ok((pixmap, step))
}

//////////////////////////////////////////////////////////////////////////////
// Path flattening
//////////////////////////////////////////////////////////////////////////////

fn flatten_path(path: &usvg::Path, out: &mut Vec<Vec<(f64, f64)>>) {
    let t = path.abs_transform();
    let map = |x: f32, y: f32| -> (f64, f64) {
        let mut px = tiny_skia::Point::from_xy(x, y);
        t.map_point(&mut px);
        (px.x as f64, px.y as f64)
    };

    let mut current: Vec<(f64, f64)> = Vec::new();
    let mut start_pt: Option<(f64, f64)> = None;
    for seg in path.data().segments() {
        use tiny_skia::PathSegment::*;
        match seg {
            MoveTo(p) => {
                if current.len() > 1 {
                    out.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
                let pt = map(p.x, p.y);
                start_pt = Some(pt);
                current.push(pt);
            }
            LineTo(p) => current.push(map(p.x, p.y)),
            QuadTo(c, p) => {
                let from = *current.last().unwrap_or(&(0.0, 0.0));
                let c = map(c.x, c.y);
                let p = map(p.x, p.y);
                flatten_quad(from, c, p, 0, &mut current);
            }
            CubicTo(c1, c2, p) => {
                let from = *current.last().unwrap_or(&(0.0, 0.0));
                let c1 = map(c1.x, c1.y);
                let c2 = map(c2.x, c2.y);
                let p = map(p.x, p.y);
                flatten_cubic(from, c1, c2, p, 0, &mut current);
            }
            Close => {
                if let Some(sp) = start_pt {
                    current.push(sp);
                }
                if current.len() > 1 {
                    out.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
        }
    }
    if current.len() > 1 {
        out.push(current);
    }
}

const FLATTEN_TOL: f64 = 0.05; // px, ~13 µm
const MAX_DEPTH: u32 = 16;

fn flatten_quad(p0: (f64, f64), c: (f64, f64), p1: (f64, f64), depth: u32, out: &mut Vec<(f64, f64)>) {
    if depth >= MAX_DEPTH || quad_flat(p0, c, p1) {
        out.push(p1);
        return;
    }
    let m01 = mid(p0, c);
    let m12 = mid(c, p1);
    let m = mid(m01, m12);
    flatten_quad(p0, m01, m, depth + 1, out);
    flatten_quad(m, m12, p1, depth + 1, out);
}

fn flatten_cubic(
    p0: (f64, f64),
    c1: (f64, f64),
    c2: (f64, f64),
    p1: (f64, f64),
    depth: u32,
    out: &mut Vec<(f64, f64)>,
) {
    if depth >= MAX_DEPTH || cubic_flat(p0, c1, c2, p1) {
        out.push(p1);
        return;
    }
    let m01 = mid(p0, c1);
    let m12 = mid(c1, c2);
    let m23 = mid(c2, p1);
    let m012 = mid(m01, m12);
    let m123 = mid(m12, m23);
    let m = mid(m012, m123);
    flatten_cubic(p0, m01, m012, m, depth + 1, out);
    flatten_cubic(m, m123, m23, p1, depth + 1, out);
}

fn mid(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
}

fn dist_point_line(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-12 {
        return (p.0 - a.0).hypot(p.1 - a.1);
    }
    ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len
}

fn quad_flat(p0: (f64, f64), c: (f64, f64), p1: (f64, f64)) -> bool {
    dist_point_line(c, p0, p1) < FLATTEN_TOL
}

fn cubic_flat(p0: (f64, f64), c1: (f64, f64), c2: (f64, f64), p1: (f64, f64)) -> bool {
    dist_point_line(c1, p0, p1) < FLATTEN_TOL && dist_point_line(c2, p0, p1) < FLATTEN_TOL
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 96 96" width="96" height="96">
        <rect x="10" y="10" width="40" height="20" fill="black"/>
        <circle cx="70" cy="60" r="15" fill="none" stroke="#ff0000" stroke-width="2"/>
    </svg>"##;

    fn settings() -> PrepareSettings {
        PrepareSettings {
            mode: Some(JobMode::Raster),
            color_rules: vec![],
            feed_mmps: 100.0,
            engrave_feed_mmps: 30.0,
            cut_feed_mmps: 10.0,
            passes: 1,
            engrave_passes: 1,
            cut_passes: 1,
            raster_step_mils: 2,
            render_dpi: 500.0,
            dither: false,
            invert: false,
            bottom_up: false,
            offset_x_mm: 0.0,
            offset_y_mm: 0.0,
            scale: 1.0,
            clip: None,
        }
    }

    /// Debug helper, not a regular test: dumps the dithered bitmap (PBM),
    /// the raster ecoords and the stripped EGV block for an SVG given via
    /// env, so they can be diffed against the original K40 Whisperer Python
    /// pipeline. Run with:
    ///   RK40_DUMP_SVG=<file.svg> RK40_DUMP_DIR=<dir> \
    ///     cargo test dump_debug_artifacts -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_debug_artifacts() {
        let svg_path = std::env::var("RK40_DUMP_SVG").expect("RK40_DUMP_SVG fehlt");
        let out_dir = std::env::var("RK40_DUMP_DIR").expect("RK40_DUMP_DIR fehlt");
        let svg = std::fs::read_to_string(&svg_path).unwrap();
        let s = PrepareSettings {
            feed_mmps: 180.0,
            offset_x_mm: 10.0,
            offset_y_mm: 10.0,
            ..settings()
        };

        let opt = svg_options();
        let tree = usvg::Tree::from_str(&svg, &opt).unwrap();
        let width_mm = tree.size().width() as f64 / 96.0 * MM_PER_INCH;
        let height_mm = tree.size().height() as f64 / 96.0 * MM_PER_INCH;
        let rules = RuleSet::from_settings(&s);
        let r = rasterize(&tree, width_mm, height_mm, &s, &rules)
            .unwrap()
            .expect("keine Rasterdaten");

        // PBM (P4): 1 = dark
        let mut pbm = format!("P4\n{} {}\n", r.wpx, r.hpx).into_bytes();
        for y in 0..r.hpx as usize {
            let row = &r.dark[y * r.wpx as usize..(y + 1) * r.wpx as usize];
            for chunk in row.chunks(8) {
                let mut b = 0u8;
                for (i, &d) in chunk.iter().enumerate() {
                    if d {
                        b |= 0x80 >> i;
                    }
                }
                pbm.push(b);
            }
        }
        std::fs::write(format!("{out_dir}/dark.pbm"), pbm).unwrap();

        let mut txt = String::new();
        for p in &r.ecoords {
            txt.push_str(&format!("{:.9} {:.9} {}\n", p.x, p.y, p.loop_id));
        }
        std::fs::write(format!("{out_dir}/ecoords.txt"), txt).unwrap();

        let start_x = -(s.offset_x_mm / MM_PER_INCH);
        let start_y = height_mm / MM_PER_INCH + s.offset_y_mm / MM_PER_INCH;
        let mut egv = Egv::new();
        egv.make_egv_data(&r.ecoords, start_x, start_y, s.feed_mmps, Board::M2, -2)
            .unwrap();
        let block = strip_redundant_codes(&egv.data);
        std::fs::write(format!("{out_dir}/raster.egv"), &block).unwrap();
        println!(
            "dumped: {}x{} px, {} ecoords, {} egv bytes, start=({:.6},{:.6})",
            r.wpx, r.hpx, r.ecoords.len(), block.len(), start_x, start_y
        );
    }

    /// A cover shape in an ignored color must not hide artwork below it:
    /// this is the "white rectangle over half the design" case.
    #[test]
    fn ignored_color_does_not_occlude() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 96 96" width="96" height="96">
            <rect x="10" y="10" width="76" height="76" fill="#000000"/>
            <rect x="-5" y="-5" width="106" height="58" fill="#ffffff" stroke="#ffffff"/>
        </svg>"##;
        let opt = svg_options();
        let tree = usvg::Tree::from_str(svg, &opt).unwrap();
        let mut s = settings();
        s.mode = None;
        s.color_rules = vec![
            ColorRule { color: "#000000".into(), op: OpKind::Raster },
            ColorRule { color: "#ffffff".into(), op: OpKind::Ignore },
        ];
        let rules = RuleSet::from_settings(&s);
        let r = rasterize(&tree, 25.4, 25.4, &s, &rules).unwrap().unwrap();
        // first scanline with dark pixels must sit near the TOP of the black
        // rect (~10/96 down), not below the white cover (~53/96 down)
        let top_y = r.ecoords.first().unwrap().y; // inches from bottom
        let expected_top = (1.0 - 10.0 / 96.0) * 1.0; // design is 1in tall
        assert!(
            (top_y - expected_top).abs() < 0.03,
            "Gravur beginnt bei y={top_y} statt ~{expected_top} (Occlusion nicht entfernt?)"
        );

        // control: WITHOUT the ignore rule the white rect occludes the top
        let mut s2 = settings();
        s2.mode = None;
        s2.color_rules = vec![ColorRule { color: "#000000".into(), op: OpKind::Raster }];
        // white unruled -> default raster; force it visible-but-not-raster is
        // not possible without Ignore, so emulate legacy luminance instead:
        s2.color_rules = vec![];
        s2.mode = Some(JobMode::Raster);
        let rules2 = RuleSet::from_settings(&s2);
        let r2 = rasterize(&tree, 25.4, 25.4, &s2, &rules2).unwrap().unwrap();
        let top_y2 = r2.ecoords.first().unwrap().y;
        assert!(
            top_y2 < expected_top - 0.3,
            "Kontrollfall: Occlusion erwartet, aber top_y2={top_y2}"
        );
    }

    #[test]
    fn clip_limits_raster_and_vector() {
        // black 76×76 px rect from (10,10); red frame for vector cut
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 96 96" width="96" height="96">
            <rect x="10" y="10" width="76" height="76" fill="#000000"/>
            <rect x="5" y="5" width="86" height="86" fill="none" stroke="#ff0000" stroke-width="1"/>
        </svg>"##;
        let mut s = settings();
        s.mode = None;
        s.color_rules = vec![
            ColorRule { color: "#000000".into(), op: OpKind::Raster },
            ColorRule { color: "#ff0000".into(), op: OpKind::VectorCut },
        ];
        // clip to the top-left quadrant (design is 25.4 mm square)
        s.clip = Some(ClipRect { x_mm: 0.0, y_mm: 0.0, w_mm: 12.7, h_mm: 12.7 });

        let opt = svg_options();
        let tree = usvg::Tree::from_str(svg, &opt).unwrap();
        let rules = RuleSet::from_settings(&s);
        let r = rasterize(&tree, 25.4, 25.4, &s, &rules).unwrap().unwrap();
        // no dark pixel outside the clip window (12.7mm = half = 250px @500dpi)
        let half = (12.7 / 25.4 * 500.0) as usize;
        for y in 0..r.hpx as usize {
            for x in 0..r.wpx as usize {
                if r.dark[y * r.wpx as usize + x] {
                    assert!(x < half + 1 && y < half + 1, "Pixel außerhalb Clip: {x},{y}");
                }
            }
        }

        // vector: whole plan succeeds and the cut length shrinks vs unclipped
        let job_clipped = plan_svg(svg, &s).unwrap();
        s.clip = None;
        let job_full = plan_svg(svg, &s).unwrap();
        assert!(job_clipped.estimate_s < job_full.estimate_s);
    }

    #[test]
    fn clip_segment_cases() {
        let r = (0.0, 0.0, 10.0, 10.0);
        // fully inside
        assert_eq!(clip_segment((1.0, 1.0), (9.0, 9.0), r), Some(((1.0, 1.0), (9.0, 9.0))));
        // fully outside
        assert_eq!(clip_segment((11.0, 0.0), (20.0, 5.0), r), None);
        // crossing: clipped at both ends
        let (a, b) = clip_segment((-5.0, 5.0), (15.0, 5.0), r).unwrap();
        assert_eq!((a.0, b.0), (0.0, 10.0));
    }

    #[test]
    fn negative_offset_clips_to_bed() {
        // design placed partly left/above home: the overhang must be clipped,
        // so the job shrinks compared to full placement
        let mut s = settings();
        s.offset_x_mm = -10.0;
        s.offset_y_mm = -5.0;
        let job_neg = plan_svg(TEST_SVG, &s).unwrap();
        let job_full = plan_svg(TEST_SVG, &settings()).unwrap();
        assert!(
            job_neg.estimate_s < job_full.estimate_s,
            "clipped job ({}) not smaller than full job ({})",
            job_neg.estimate_s,
            job_full.estimate_s
        );

        // fully outside the bed -> clear error
        s.offset_x_mm = -400.0;
        assert!(plan_svg(TEST_SVG, &s).is_err());

        // overhang past the far edge is clipped too
        let mut s2 = settings();
        s2.offset_x_mm = BED_W_MM - 10.0;
        let job_right = plan_svg(TEST_SVG, &s2).unwrap();
        assert!(job_right.estimate_s < job_full.estimate_s);
    }

    #[test]
    fn negative_offset_combines_with_workpiece_clip() {
        let mut s = settings();
        s.offset_x_mm = -10.0;
        // workpiece window covering the whole design: bed clip must still apply
        s.clip = Some(ClipRect { x_mm: -5.0, y_mm: -5.0, w_mm: 40.0, h_mm: 40.0 });
        let job = plan_svg(TEST_SVG, &s).unwrap();
        let mut s_full = settings();
        s_full.clip = s.clip;
        let job_full = plan_svg(TEST_SVG, &s_full).unwrap();
        assert!(job.estimate_s < job_full.estimate_s);
    }

    #[test]
    fn markers_clamped_to_bed() {
        // box partly outside: clamped, still valid
        assert!(plan_markers(-10.0, -10.0, 30.0, 30.0, 100.0).is_ok());
        // box fully outside: error
        assert!(plan_markers(-50.0, 0.0, 30.0, 30.0, 100.0).is_err());
    }

    #[test]
    fn markers_produce_valid_egv() {
        let egv = plan_markers(25.0, 30.0, 132.0, 84.0, 100.0).unwrap();
        assert_eq!(egv[0], b'I');
        assert!(egv.ends_with(b"FNSE"));
        // vector speed code, no raster G suffix
        assert_eq!(egv[1], b'C');
        assert_eq!(egv[2], b'V');
        assert!(egv.len() > 60);
        // tiny boxes fail gracefully
        assert!(plan_markers(0.0, 0.0, 0.0, 5.0, 100.0).is_err());
    }

    #[test]
    fn legacy_raster_mode_works() {
        let job = plan_svg(TEST_SVG, &settings()).unwrap();
        assert!(job.egv.len() > 100);
        assert_eq!(job.egv[0], b'I');
        assert!(job.egv.ends_with(b"FNSE"));
        assert!((job.width_mm - 25.4).abs() < 0.01);
    }

    #[test]
    fn legacy_vector_mode_works() {
        let mut s = settings();
        s.mode = Some(JobMode::VectorCut);
        let job = plan_svg(TEST_SVG, &s).unwrap();
        assert!(job.egv.len() > 20);
        assert!(job.egv.ends_with(b"FNSE"));
    }

    #[test]
    fn color_rules_split_raster_and_cut() {
        let mut s = settings();
        s.mode = None;
        s.color_rules = vec![
            ColorRule { color: "#000000".into(), op: OpKind::Raster },
            ColorRule { color: "#ff0000".into(), op: OpKind::VectorCut },
        ];
        let job = plan_svg(TEST_SVG, &s).unwrap();
        let egv = &job.egv;
        // one '@' join between the raster block and the cut block
        let joins = egv.iter().filter(|&&b| b == b'@').count();
        assert!(joins >= 1, "expected joined multi-op stream");
        assert!(egv.ends_with(b"FNSE"));
        // raster speed code (G-suffix) and vector speed code (CV prefix) present
        let s = egv.iter().map(|&b| b as char).collect::<String>();
        assert!(s.contains('G'), "raster speed code missing");
        assert!(s.contains("CV"), "vector speed code missing");
    }

    #[test]
    fn per_op_passes_are_baked() {
        let mut s = settings();
        s.mode = None;
        s.color_rules = vec![
            ColorRule { color: "#000000".into(), op: OpKind::Ignore },
            ColorRule { color: "#ff0000".into(), op: OpKind::VectorCut },
        ];
        s.cut_passes = 3;
        let job = plan_svg(TEST_SVG, &s).unwrap();
        let joins = job.egv.iter().filter(|&&b| b == b'@').count();
        assert_eq!(joins, 2, "3 passes = 2 joins");
    }

    #[test]
    fn palette_extraction() {
        let (colors, w, h) = extract_palette(TEST_SVG).unwrap();
        let names: Vec<&str> = colors.iter().map(|c| c.color.as_str()).collect();
        assert!(names.contains(&"#000000"));
        assert!(names.contains(&"#ff0000"));
        assert!((w - 25.4).abs() < 0.01);
        assert!((h - 25.4).abs() < 0.01);
    }
}
