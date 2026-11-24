//! Convert Sketch (`.sketch`, v43+ JSON format) documents to PDF.
//!
//! Every artboard becomes one PDF page, in document order. A document without artboards
//! exports its symbol masters instead. Drawing goes through Skia's PDF backend, so shapes and
//! text stay vector; blurs and shadows are rasterised by Skia. Background blurs need what is
//! behind the layer, so boards that use them get an extra raster pass first.

use serde_json::Value;
use skia_safe::{
    canvas::SaveLayerRec,
    gradient_shader::GradientShaderColors,
    color_filters, image_filters, images, pdf, surfaces,
    textlayout::{
        FontCollection, ParagraphBuilder, ParagraphStyle, TextAlign, TextDecoration, TextStyle,
        TypefaceFontProvider,
    },
    BlendMode, BlurStyle, Canvas, ClipOp, Color4f, ColorMatrix, Data, IRect, FontMgr, FontStyle, Image, MaskFilter,
    Matrix, Paint, PaintCap, PaintJoin, PaintStyle, Path, PathEffect, PathFillType, PathOp,
    Point, Rect, SamplingOptions, Shader, TileMode, Typeface,
};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashMap},
    fmt,
    io::{Cursor, Read},
};

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err(e: impl fmt::Display) -> Error {
    Error(e.to_string())
}

pub struct Output {
    pub pdf: Vec<u8>,
    /// Things that could not be rendered faithfully (missing fonts, unsupported images).
    pub warnings: Vec<String>,
}

/// Convert the bytes of a `.sketch` file into PDF bytes.
pub fn convert(sketch: &[u8]) -> Result<Output, Error> {
    convert_with_fonts(sketch, &[])
}

/// Like [`convert`], with extra font files (TTF/OTF/TTC bytes) to use before system fonts.
/// Fonts embedded in the document still win.
pub fn convert_with_fonts(sketch: &[u8], fonts: &[Vec<u8>]) -> Result<Output, Error> {
    let doc = Doc::load(sketch, fonts)?;
    let boards = doc.boards();
    if boards.is_empty() {
        return Err(err("document has no artboards or symbols to export"));
    }
    let mut pdf = Vec::new();
    let mut d = pdf::new_document(&mut pdf, None);
    for b in boards {
        let f = board_rect(b);
        if doc.has_backdrop(b, 0) {
            doc.capture_backdrops(b);
        }
        let mut page = d.begin_page((f.width(), f.height()), None);
        doc.draw_board(page.canvas(), b);
        d = page.end_page();
        doc.backdrops.borrow_mut().clear();
    }
    d.close();
    Ok(Output { pdf, warnings: doc.warnings.into_inner().into_iter().collect() })
}

type Ov = HashMap<String, Value>;

struct Doc {
    files: HashMap<String, Vec<u8>>,
    pages: Vec<Value>,
    symbols: HashMap<String, Value>,
    shared_styles: HashMap<String, Value>,
    images: RefCell<HashMap<String, Option<Image>>>,
    fonts: Fonts,
    warnings: RefCell<BTreeSet<String>>,
    depth: Cell<u32>,
    /// What lies behind each background-blur layer of the current board, in drawing order,
    /// rendered at [`BACKDROP_SCALE`] by a raster pass (Skia's PDF backend can't read back).
    backdrops: RefCell<Vec<Image>>,
    next_backdrop: Cell<usize>,
    capturing: Cell<bool>,
}

const BACKDROP_SCALE: f32 = 2.0;

impl Doc {
    fn load(sketch: &[u8], extra_fonts: &[Vec<u8>]) -> Result<Doc, Error> {
        let mut zip = zip::ZipArchive::new(Cursor::new(sketch))
            .map_err(|e| err(format!("not a .sketch (zip) file: {e}")))?;
        let mut files = HashMap::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).map_err(err)?;
            if f.is_dir() {
                continue;
            }
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(err)?;
            files.insert(f.name().to_string(), buf);
        }
        let json = |name: &str| -> Result<Value, Error> {
            let bytes = files.get(name).ok_or_else(|| err(format!("missing {name}")))?;
            serde_json::from_slice(bytes).map_err(|e| err(format!("{name}: {e}")))
        };
        let document = json("document.json")?;
        let pages = arr(&document, "pages")
            .iter()
            .map(|p| json(&format!("{}.json", s(p, "_ref"))))
            .collect::<Result<Vec<_>, _>>()?;

        let mut symbols = HashMap::new();
        let masters = pages.iter().flat_map(|p| arr(p, "layers")).filter(|l| cls(l) == "symbolMaster");
        let foreign = arr(&document, "foreignSymbols").iter().map(|f| &f["symbolMaster"]);
        for m in foreign.chain(masters) {
            symbols.insert(s(m, "symbolID").to_string(), m.clone());
        }

        let mut shared_styles = HashMap::new();
        let local = ["layerStyles", "layerTextStyles"].into_iter().flat_map(|k| arr(&document[k], "objects"));
        let foreign = ["foreignLayerStyles", "foreignTextStyles"]
            .into_iter()
            .flat_map(|k| arr(&document, k))
            .map(|f| &f["localSharedStyle"]);
        for st in foreign.chain(local) {
            shared_styles.insert(s(st, "do_objectID").to_string(), st["value"].clone());
        }

        let fonts = Fonts::new(extra_fonts.iter().chain(files.iter().filter(|(k, _)| k.starts_with("fonts/")).map(|(_, v)| v)));
        Ok(Doc {
            files,
            pages,
            symbols,
            shared_styles,
            images: Default::default(),
            fonts,
            warnings: Default::default(),
            depth: Cell::new(0),
            backdrops: Default::default(),
            next_backdrop: Cell::new(0),
            capturing: Cell::new(false),
        })
    }

    fn warn(&self, w: String) {
        self.warnings.borrow_mut().insert(w);
    }

    fn boards(&self) -> Vec<&Value> {
        let top = || self.pages.iter().flat_map(|p| arr(p, "layers")).filter(|l| visible(l));
        // Symbol masters laid out among artboards are screens too; a page of only masters (the
        // Symbols page) is a library and stays out.
        let boards: Vec<_> = self
            .pages
            .iter()
            .filter(|p| arr(p, "layers").iter().any(|l| visible(l) && cls(l) == "artboard"))
            .flat_map(|p| arr(p, "layers"))
            .filter(|l| visible(l) && matches!(cls(l), "artboard" | "symbolMaster"))
            .collect();
        if !boards.is_empty() {
            return boards;
        }
        // A pure symbol library exports its masters; once instances or other loose layers sit
        // on the canvas, each page's whole canvas is one board, like Sketch's page preview.
        if top().next().is_some() && top().all(|l| cls(l) == "symbolMaster") {
            return top().collect();
        }
        self.pages.iter().filter(|p| arr(p, "layers").iter().any(|l| visible(l))).collect()
    }

    fn draw_board(&self, c: &Canvas, b: &Value) {
        let rect = board_rect(b);
        c.translate((-rect.left, -rect.top));
        if flag(b, "hasBackgroundColor") {
            c.draw_rect(rect, &color_paint(&b["backgroundColor"]));
        }
        c.clip_rect(rect, None, true);
        self.draw_layers(c, arr(b, "layers"), &Ov::new());
    }

    /// Whether a layer, its children or the symbols it uses have a background blur.
    // ponytail: ignores symbol-swap overrides; a swapped-in blur is then left unblurred.
    fn has_backdrop(&self, l: &Value, depth: u32) -> bool {
        backdrop_blur(&l["style"]).is_some()
            || depth < 32
                && (arr(l, "layers").iter().any(|x| self.has_backdrop(x, depth + 1))
                    || self.symbols.get(s(l, "symbolID")).is_some_and(|m| self.has_backdrop(m, depth + 1)))
    }

    /// Raster pass over a board that records what is behind each background-blur layer.
    fn capture_backdrops(&self, b: &Value) {
        let r = board_rect(b);
        let size = ((r.width() * BACKDROP_SCALE).ceil() as i32, (r.height() * BACKDROP_SCALE).ceil() as i32);
        let Some(mut surface) = surfaces::raster_n32_premul(size) else { return };
        self.capturing.set(true);
        let c = surface.canvas();
        c.scale((BACKDROP_SCALE, BACKDROP_SCALE));
        self.draw_board(c, b);
        self.capturing.set(false);
        self.next_backdrop.set(0);
    }

    /// Draw the blurred backdrop inside the layer's outline. In the raster pass the backdrop is
    /// read from the canvas itself; in the PDF pass it comes from that pass, in the same order.
    fn draw_backdrop(&self, c: &Canvas, l: &Value, ov: &Ov, radius: f32, saturation: f32) {
        let img = if self.capturing.get() {
            let info = c.image_info();
            let rb = info.min_row_bytes();
            let mut px = vec![0u8; info.compute_min_byte_size()];
            if !c.read_pixels(&info, &mut px, rb, (0, 0)) {
                return;
            }
            let Some(img) = images::raster_from_data(&info, Data::new_copy(&px), rb) else { return };
            self.backdrops.borrow_mut().push(img.clone());
            img
        } else {
            let i = self.next_backdrop.get();
            self.next_backdrop.set(i + 1);
            let Some(img) = self.backdrops.borrow().get(i).cloned() else { return };
            img
        };
        let path = self.outline(l, ov, 0).unwrap_or_else(|| Path::rect(frame_rect(l), None));
        let n = c.save();
        c.clip_path(&path, None, true);
        (|| {
            let clip = c.device_clip_bounds()?;
            // Image pixels are raster-pass device pixels; the PDF page is BACKDROP_SCALE smaller.
            let k = if self.capturing.get() { 1.0 } else { BACKDROP_SCALE };
            // Sketch's background blur is visibly stronger than its layer blur: sigma = radius
            // matched its previews better than radius / 2.
            let sigma = radius * BACKDROP_SCALE;
            let margin = (sigma * 3.0).ceil() as i32;
            let scaled = |v: i32| (v as f32 * k) as i32;
            let src = IRect::new(scaled(clip.left), scaled(clip.top), scaled(clip.right), scaled(clip.bottom))
                .with_outset((margin, margin));
            let src = IRect::intersect(&src, &img.bounds())?;
            let sub = img.make_subset(None, src)?;
            c.reset_matrix();
            c.scale((1.0 / k, 1.0 / k));
            let mut cm = ColorMatrix::default();
            cm.set_saturation(saturation);
            let blur = image_filters::blur((sigma, sigma), TileMode::Clamp, None, None);
            let mut p = Paint::default();
            p.set_image_filter(image_filters::color_filter(color_filters::matrix(&cm, None), blur, None));
            c.draw_image(&sub, (src.left as f32, src.top as f32), Some(&p));
            Some(())
        })();
        c.restore_to_count(n);
    }

    /// Draw sibling layers, applying clipping masks: a mask layer clips the siblings above it
    /// until one breaks the mask chain.
    fn draw_layers(&self, c: &Canvas, layers: &[Value], ov: &Ov) {
        let mut mask: Option<usize> = None;
        for l in layers {
            if flag(l, "shouldBreakMaskChain") {
                if let Some(n) = mask.take() {
                    c.restore_to_count(n);
                }
            }
            self.draw_layer(c, l, ov);
            if flag(l, "hasClippingMask") && visible(l) {
                if let Some(n) = mask.take() {
                    c.restore_to_count(n);
                }
                let path = self.outline(l, ov, 0).unwrap_or_else(|| Path::rect(frame_rect(l), None));
                mask = Some(c.save());
                c.clip_path(&path.with_transform(&layer_matrix(l)), None, true);
            }
        }
        if let Some(n) = mask {
            c.restore_to_count(n);
        }
    }

    fn draw_layer(&self, c: &Canvas, l: &Value, ov: &Ov) {
        if !visible(l) || matches!(cls(l), "slice" | "MSImmutableHotspotLayer") {
            return;
        }
        let style = self.style_of(l, ov);
        let n = c.save();
        c.concat(&layer_matrix(l));

        if let Some((radius, saturation)) = backdrop_blur(&style) {
            self.draw_backdrop(c, l, ov, radius, saturation);
        }

        let cs = &style["contextSettings"];
        let opacity = cs["opacity"].as_f64().unwrap_or(1.0) as f32;
        let blend = blend_mode(cs);
        let blur = &style["blur"];
        let blur = if flag(blur, "isEnabled") && blur["type"].as_i64() == Some(0) { num(blur, "radius") } else { 0.0 };
        if opacity < 1.0 || blend != BlendMode::SrcOver || blur > 0.0 {
            let mut p = Paint::default();
            p.set_alpha_f(opacity);
            p.set_blend_mode(blend);
            if blur > 0.0 {
                p.set_image_filter(image_filters::blur((blur / 2.0, blur / 2.0), None, None, None));
            }
            // Bounded, or the PDF backend rasterises the filter over the whole page.
            let bounds = filter_bounds(l, blur * 1.5);
            let rec = SaveLayerRec::default().paint(&p);
            c.save_layer(&if blur > 0.0 { rec.bounds(&bounds) } else { rec });
        }

        if let Some(path) = self.shape_path(l) {
            self.draw_shape(c, l, &path, &style, ov);
        } else {
            // Shapes draw their shadows as vectors; everything else gets a rasterised shadow
            // drawn underneath the content.
            for sh in enabled(&style, "shadows") {
                let mut p = Paint::default();
                p.set_image_filter(image_filters::drop_shadow_only(
                    (num(sh, "offsetX"), num(sh, "offsetY")),
                    (num(sh, "blurRadius") / 2.0, num(sh, "blurRadius") / 2.0),
                    shadow_color(sh),
                    None,
                    None,
                    None,
                ));
                let reach = num(sh, "blurRadius") * 1.5 + num(sh, "offsetX").abs().max(num(sh, "offsetY").abs());
                c.save_layer(&SaveLayerRec::default().paint(&p).bounds(&filter_bounds(l, reach)));
                self.draw_content(c, l, &style, ov);
                c.restore();
            }
            self.draw_content(c, l, &style, ov);
        }
        c.restore_to_count(n);
    }

    fn draw_content(&self, c: &Canvas, l: &Value, style: &Value, ov: &Ov) {
        // A colour fill on a group or symbol instance tints everything inside it.
        let tint = match cls(l) {
            "group" | "symbolInstance" => enabled(style, "fills").filter(|f| f["fillType"].as_i64().unwrap_or(0) == 0).last(),
            _ => None,
        };
        if let Some(fill) = tint {
            c.save_layer(&SaveLayerRec::default());
            self.draw_plain_content(c, l, style, ov);
            let mut p = Paint::default();
            p.set_color4f(to_color4f(&fill["color"]), None);
            p.set_alpha_f(p.alpha_f() * fill["contextSettings"]["opacity"].as_f64().unwrap_or(1.0) as f32);
            p.set_blend_mode(BlendMode::SrcIn);
            c.draw_paint(&p);
            c.restore();
        } else {
            self.draw_plain_content(c, l, style, ov);
        }
    }

    fn draw_plain_content(&self, c: &Canvas, l: &Value, style: &Value, ov: &Ov) {
        match cls(l) {
            "group" => self.draw_layers(c, arr(l, "layers"), ov),
            "artboard" | "symbolMaster" => {
                let n = c.save();
                self.draw_board(c, l);
                c.restore_to_count(n);
            }
            "symbolInstance" => self.draw_instance(c, l, ov),
            "text" => self.draw_text(c, l, style, ov),
            "bitmap" => {
                let img = ov.get(&format!("{}_image", s(l, "do_objectID"))).unwrap_or(&l["image"]);
                if let Some(img) = self.image(img) {
                    c.draw_image_rect(&img, None, frame_rect(l), &Paint::default());
                }
            }
            _ => {}
        }
    }

    fn style_of<'a>(&self, l: &'a Value, ov: &Ov) -> Cow<'a, Value> {
        let id = s(l, "do_objectID");
        let shared = ov
            .get(&format!("{id}_layerStyle"))
            .and_then(|v| self.shared_styles.get(v.as_str()?));
        let mut style = match shared {
            Some(st) => Cow::Owned(st.clone()),
            None => Cow::Borrowed(&l["style"]),
        };
        if let Some(color) = ov.get(&format!("{id}_fillColor")) {
            if let Some(fill) = style.to_mut()["fills"].get_mut(0) {
                fill["color"] = color.clone();
            }
        }
        style
    }

    /// An instance's master (after a symbol swap override) and the overrides that apply inside it.
    fn master_of(&self, l: &Value, ov: &Ov) -> Option<(&Value, Ov)> {
        let id = s(l, "do_objectID");
        let sym_id = ov.get(&format!("{id}_symbolID")).and_then(Value::as_str).unwrap_or(s(l, "symbolID"));
        let master = self.symbols.get(sym_id)?;
        // Overrides addressed through this instance ("<id>/...") win over its own.
        let prefix = format!("{id}/");
        let mut inner: Ov =
            ov.iter().filter_map(|(k, v)| Some((k.strip_prefix(&prefix)?.to_string(), v.clone()))).collect();
        for o in arr(l, "overrideValues") {
            inner.entry(s(o, "overrideName").to_string()).or_insert_with(|| o["value"].clone());
        }
        Some((master, inner))
    }

    fn draw_instance(&self, c: &Canvas, l: &Value, ov: &Ov) {
        let Some((master, inner)) = self.master_of(l, ov) else { return };
        if self.depth.get() > 32 {
            return; // a symbol that contains itself
        }

        let mf = frame(master);
        // A scaled instance (Sketch's Scale tool) draws its content enlarged; its unscaled
        // size is what the master's resizing constraints see.
        let scale = l["scale"].as_f64().filter(|k| *k > 0.0).unwrap_or(1.0) as f32;
        let f = frame(l);
        let f = Rect::from_wh(f.width() / scale, f.height() / scale);
        c.scale((scale, scale));
        let rect = Rect::from_wh(f.width(), f.height());
        if flag(master, "hasBackgroundColor") && flag(master, "includeBackgroundColorInInstance") {
            c.draw_rect(rect, &color_paint(&master["backgroundColor"]));
        }
        let layers = arr(master, "layers");
        let layers = if (mf.width(), mf.height()) == (f.width(), f.height()) {
            Cow::Borrowed(layers)
        } else {
            Cow::Owned(layers.iter().map(|x| resized(x, (mf.width(), mf.height()), (f.width(), f.height()))).collect())
        };
        self.depth.set(self.depth.get() + 1);
        self.draw_layers(c, &layers, &inner);
        self.depth.set(self.depth.get() - 1);
    }

    /// The area a mask layer covers: its shape, or the union of the shapes inside a group or
    /// symbol instance.
    fn outline(&self, l: &Value, ov: &Ov, depth: u32) -> Option<Path> {
        if let Some(p) = self.shape_path(l) {
            return Some(p);
        }
        let union = |layers: &[Value], ov: &Ov| {
            let mut acc: Option<Path> = None;
            for x in layers.iter().filter(|x| visible(x)) {
                let Some(p) = self.outline(x, ov, depth + 1) else { continue };
                let p = p.with_transform(&layer_matrix(x));
                acc = Some(match acc {
                    None => p,
                    Some(a) => a.op(&p, PathOp::Union).unwrap_or(a),
                });
            }
            acc
        };
        match cls(l) {
            "group" if depth < 32 => union(arr(l, "layers"), ov),
            "symbolInstance" if depth < 32 => {
                let (master, inner) = self.master_of(l, ov)?;
                let mf = frame(master);
                let k = l["scale"].as_f64().filter(|k| *k > 0.0).unwrap_or(1.0) as f32;
                let f = frame(l);
                let size = (f.width() / k, f.height() / k);
                let layers: Vec<Value> =
                    arr(master, "layers").iter().map(|x| resized(x, (mf.width(), mf.height()), size)).collect();
                Some(union(&layers, &inner)?.with_transform(&Matrix::scale((k, k))))
            }
            _ => None,
        }
    }

    fn shape_path(&self, l: &Value) -> Option<Path> {
        let mut path = match cls(l) {
            "rectangle" | "oval" | "triangle" | "star" | "polygon" | "shapePath" => points_path(l),
            "shapeGroup" => {
                let mut acc: Option<Path> = None;
                for child in arr(l, "layers").iter().filter(|x| visible(x)) {
                    let Some(p) = self.shape_path(child) else { continue };
                    let p = p.with_transform(&layer_matrix(child));
                    acc = Some(match (acc, child["booleanOperation"].as_i64().unwrap_or(-1)) {
                        (None, _) => p,
                        (Some(mut a), -1) => {
                            a.add_path(&p, (0, 0), None);
                            a
                        }
                        (Some(a), op) => {
                            let op = [PathOp::Union, PathOp::Difference, PathOp::Intersect, PathOp::XOR]
                                [op.clamp(0, 3) as usize];
                            a.op(&p, op).unwrap_or(a)
                        }
                    });
                }
                acc.unwrap_or_default()
            }
            _ => return None,
        };
        let rule = l.get("windingRule").or_else(|| l["style"].get("windingRule"));
        let even_odd = rule.and_then(Value::as_i64) == Some(1);
        path.set_fill_type(if even_odd { PathFillType::EvenOdd } else { PathFillType::Winding });
        Some(path)
    }

    fn draw_shape(&self, c: &Canvas, l: &Value, path: &Path, style: &Value, ov: &Ov) {
        let f = frame(l);
        let (w, h) = (f.width(), f.height());

        for sh in enabled(style, "shadows") {
            let mut p = Paint::default();
            p.set_anti_alias(true);
            p.set_color4f(shadow_color(sh), None);
            let blur = num(sh, "blurRadius");
            if blur > 0.0 {
                p.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, blur / 2.0, false));
            }
            let spread = num(sh, "spread");
            if spread > 0.0 {
                p.set_style(PaintStyle::StrokeAndFill).set_stroke_width(spread * 2.0);
            }
            c.draw_path(&path.with_offset((num(sh, "offsetX"), num(sh, "offsetY"))), &p);
        }

        for fill in enabled(style, "fills") {
            if fill["fillType"].as_i64() == Some(4) {
                let img = ov.get(&format!("{}_image", s(l, "do_objectID"))).unwrap_or(&fill["image"]);
                if let Some(img) = self.image(img) {
                    let n = c.save();
                    c.clip_path(path, None, true);
                    draw_pattern(c, &img, fill, w, h);
                    c.restore_to_count(n);
                }
            } else if let Some(p) = fill_paint(fill, w, h) {
                c.draw_path(path, &p);
            }
        }

        for sh in enabled(style, "innerShadows") {
            let mut p = Paint::default();
            p.set_anti_alias(true);
            p.set_color4f(shadow_color(sh), None);
            let blur = num(sh, "blurRadius");
            if blur > 0.0 {
                p.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, blur / 2.0, false));
            }
            let mut outside = path.with_offset((num(sh, "offsetX"), num(sh, "offsetY")));
            outside.toggle_inverse_fill_type();
            let n = c.save();
            c.clip_path(path, None, true);
            c.draw_path(&outside, &p);
            c.restore_to_count(n);
        }

        let opts = &style["borderOptions"];
        for b in enabled(style, "borders") {
            let Some(mut p) = fill_paint(b, w, h) else { continue };
            let width = num(b, "thickness");
            p.set_style(PaintStyle::Stroke);
            p.set_stroke_cap([PaintCap::Butt, PaintCap::Round, PaintCap::Square][(num(opts, "lineCapStyle") as usize).min(2)]);
            p.set_stroke_join([PaintJoin::Miter, PaintJoin::Round, PaintJoin::Bevel][(num(opts, "lineJoinStyle") as usize).min(2)]);
            let dash: Vec<f32> = arr(opts, "dashPattern").iter().filter_map(|v| v.as_f64()).map(|v| v as f32).collect();
            if !dash.is_empty() {
                let dash = if dash.len() % 2 == 1 { [dash.clone(), dash].concat() } else { dash };
                p.set_path_effect(PathEffect::dash(&dash, 0.0));
            }
            // Inside/outside borders: stroke twice as wide and clip away the other half.
            let n = c.save();
            match (b["position"].as_i64(), flag(l, "isClosed") || cls(l) == "shapeGroup") {
                (Some(1), true) => {
                    c.clip_path(path, ClipOp::Intersect, true);
                    p.set_stroke_width(width * 2.0);
                }
                (Some(2), true) => {
                    c.clip_path(path, ClipOp::Difference, true);
                    p.set_stroke_width(width * 2.0);
                }
                _ => {
                    p.set_stroke_width(width);
                }
            }
            c.draw_path(path, &p);
            c.restore_to_count(n);
        }
    }

    fn draw_text(&self, c: &Canvas, l: &Value, style: &Value, ov: &Ov) {
        let id = s(l, "do_objectID");
        let attributed = &l["attributedString"];
        let mut text = s(attributed, "string").to_string();
        let mut runs: Vec<(usize, usize, Cow<Value>)> = arr(attributed, "attributes")
            .iter()
            .map(|a| (num(a, "location") as usize, num(a, "length") as usize, Cow::Borrowed(&a["attributes"])))
            .collect();
        if let Some(v) = ov.get(&format!("{id}_stringValue")).and_then(Value::as_str) {
            text = v.to_string();
            runs.truncate(1);
        }
        let shared_text = ov
            .get(&format!("{id}_textStyle"))
            .and_then(|v| self.shared_styles.get(v.as_str()?))
            .map(|st| &st["textStyle"]["encodedAttributes"]);
        if let Some(attrs) = shared_text {
            runs = vec![(0, 0, Cow::Owned(attrs.clone()))];
        }
        if runs.is_empty() {
            runs.push((0, 0, Cow::Owned(Value::Null)));
        }
        // A single run covers the whole text (after an override the old ranges are stale).
        let utf16: Vec<u16> = text.encode_utf16().collect();
        if runs.len() == 1 {
            runs[0].0 = 0;
            runs[0].1 = utf16.len();
        }

        let first = &runs[0].2;
        let para_attrs = &first["paragraphStyle"];
        let align = para_attrs["alignment"].as_i64().unwrap_or(0);
        let (text_align, k) = match align {
            1 => (TextAlign::Right, 1.0),
            2 => (TextAlign::Center, 0.5),
            3 => (TextAlign::Justify, 0.0),
            _ => (TextAlign::Left, 0.0),
        };
        let mut ps = ParagraphStyle::new();
        ps.set_text_align(text_align);
        ps.set_replace_tab_characters(true);
        let mut b = ParagraphBuilder::new(&ps, self.fonts.collection.clone());

        // Fills on a text layer paint over the run colours.
        let f = frame(l);
        let fill = enabled(style, "fills").next().and_then(|fl| fill_paint(fl, f.width(), f.height()));
        for (loc, len, attrs) in &runs {
            let end = (loc + len).min(utf16.len());
            let mut chunk = String::from_utf16_lossy(&utf16[(*loc).min(end)..end]);
            match attrs["MSAttributedStringTextTransformAttribute"].as_i64() {
                Some(1) => chunk = chunk.to_uppercase(),
                Some(2) => chunk = chunk.to_lowercase(),
                _ => {}
            }
            let font = &attrs["MSAttributedStringFontAttribute"]["attributes"];
            let size = font["size"].as_f64().unwrap_or(12.0) as f32;
            let (family, font_style) = self.fonts.resolve(s(font, "name"), self);
            let mut ts = TextStyle::new();
            ts.set_font_families(&[family]).set_font_style(font_style).set_font_size(size);
            match &fill {
                Some(p) => {
                    ts.set_foreground_paint(p);
                }
                None => {
                    let col = &attrs["MSAttributedStringColorAttribute"];
                    ts.set_color(if col.is_object() { to_color4f(col).to_color() } else { skia_safe::Color::BLACK });
                }
            }
            ts.set_letter_spacing(num(attrs, "kerning"));
            let line_height = num(&attrs["paragraphStyle"], "maximumLineHeight");
            if line_height > 0.0 {
                ts.set_height(line_height / size).set_height_override(true);
            }
            let mut deco = TextDecoration::NO_DECORATION;
            if num(attrs, "underlineStyle") != 0.0 {
                deco |= TextDecoration::UNDERLINE;
            }
            if num(attrs, "strikethroughStyle") != 0.0 {
                deco |= TextDecoration::LINE_THROUGH;
            }
            ts.set_decoration_type(deco);
            b.push_style(&ts);
            b.add_text(&chunk);
            b.pop();
        }
        let mut para = b.build();
        // Auto-width text grows to fit; fixed-width text gets a little slack so a substitute
        // font that is a hair wider does not wrap differently.
        let behaviour = l["textBehaviour"].as_i64().unwrap_or(0);
        let width = if behaviour == 0 {
            para.layout(1e6);
            para.max_intrinsic_width().ceil().max(f.width())
        } else {
            f.width() + 1.0
        };
        para.layout(width);
        let x = (f.width() - width) * k;
        let va = match style["textStyle"]["verticalAlignment"].as_i64() {
            Some(1) => 0.5,
            Some(2) => 1.0,
            _ => 0.0,
        };
        // Auto-height text: Sketch sized the frame with the real font and puts extra line
        // spacing above each line, so the last line ends at the frame's bottom.
        let y = if behaviour == 2 { (f.height() - para.height()) * va } else { f.height() - para.height() };
        para.paint(c, (x, y + self.leading_shift(&runs[0].2)));
    }

    /// Skia splits a fixed line height's extra space between ascent and descent; Sketch puts
    /// it all above the text (baseline at line height − descent). How far to move down.
    fn leading_shift(&self, attrs: &Value) -> f32 {
        let font = &attrs["MSAttributedStringFontAttribute"]["attributes"];
        let size = font["size"].as_f64().unwrap_or(12.0) as f32;
        let lh = num(&attrs["paragraphStyle"], "maximumLineHeight");
        if lh <= 0.0 {
            return 0.0;
        }
        let (family, style) = self.fonts.resolve(s(font, "name"), self);
        let Some(tf) = self.fonts.collection.clone().find_typefaces(&[family], style).into_iter().next() else {
            return 0.0;
        };
        let (_, m) = skia_safe::Font::from_typeface(tf, size).metrics();
        let (a, d) = (-m.ascent, m.descent);
        if a + d <= 0.0 {
            return 0.0;
        }
        lh * d / (a + d) - d
    }

    fn image(&self, reference: &Value) -> Option<Image> {
        let r = s(reference, "_ref");
        if r.is_empty() {
            return None;
        }
        if let Some(img) = self.images.borrow().get(r) {
            return img.clone();
        }
        let bytes = ["", ".png", ".jpg", ".jpeg", ".tiff", ".gif", ".webp", ".pdf"]
            .iter()
            .find_map(|ext| self.files.get(&format!("{r}{ext}")));
        let img = match bytes {
            Some(b) if b.starts_with(b"%PDF") => {
                // ponytail: vector (PDF) images are skipped; rasterise them (e.g. with hayro) if needed.
                self.warn(format!("PDF image not supported: {r}"));
                None
            }
            Some(b) => Image::from_encoded(Data::new_copy(b)),
            None => {
                self.warn(format!("missing image: {r}"));
                None
            }
        };
        self.images.borrow_mut().insert(r.to_string(), img.clone());
        img
    }
}

/// Fonts embedded in the document plus the system's, addressed by PostScript name.
struct Fonts {
    collection: FontCollection,
    provider: RefCell<TypefaceFontProvider>,
    system: FontMgr,
    embedded: HashMap<String, Typeface>,
    families: std::collections::HashSet<String>,
    system_index: RefCell<Option<HashMap<String, Typeface>>>,
    resolved: RefCell<HashMap<String, (String, FontStyle)>>,
}

impl Fonts {
    fn new<'a>(data: impl Iterator<Item = &'a Vec<u8>>) -> Fonts {
        let system = FontMgr::new();
        let embedded: HashMap<String, Typeface> = data
            .filter_map(|d| system.new_from_data(d, None))
            .filter_map(|tf| Some((tf.post_script_name()?, tf)))
            .collect();
        let mut provider = TypefaceFontProvider::new();
        // Also addressable by family, so a missing style falls back to the nearest one.
        let mut families = std::collections::HashSet::new();
        for tf in embedded.values() {
            families.insert(tf.family_name());
            provider.register_typeface(tf.clone(), None);
        }
        let mut collection = FontCollection::new();
        collection.set_asset_font_manager(Some(provider.clone().into()));
        collection.set_default_font_manager(system.clone(), "Helvetica");
        Fonts {
            collection,
            provider: RefCell::new(provider),
            system,
            embedded,
            families,
            system_index: Default::default(),
            resolved: Default::default(),
        }
    }

    /// Map a PostScript name to a family the font collection knows and a style.
    fn resolve(&self, ps: &str, doc: &Doc) -> (String, FontStyle) {
        if let Some(r) = self.resolved.borrow().get(ps) {
            return r.clone();
        }
        let tf = self.embedded.get(ps).cloned().or_else(|| self.system_lookup(ps));
        let r = match tf {
            Some(tf) => {
                let style = tf.font_style();
                self.provider.borrow_mut().register_typeface(tf, ps);
                (ps.to_string(), style)
            }
            None => {
                let (family, style) = guess_font(ps);
                // Apple's SF fonts ship with macOS under private names.
                let family = match family.as_str() {
                    f if f.starts_with("SF") && f.contains("Rounded") => ".SF NS Rounded".to_string(),
                    f if f.starts_with("SFPro") || f.starts_with("SFCompact") || f.starts_with("SFUI") => ".SF NS".to_string(),
                    _ => family,
                };
                if !ps.is_empty() && !family.starts_with('.') {
                    doc.warn(format!("missing font: {ps}"));
                }
                // PostScript names drop spaces: try "RobotoCondensed" as "Roboto Condensed" too.
                let spaced = spaced(&family);
                let known = |f: &str| self.families.contains(f) || self.system.match_family(f).count() > 0;
                (if !known(&family) && known(&spaced) { spaced } else { family }, style)
            }
        };
        self.resolved.borrow_mut().insert(ps.to_string(), r.clone());
        r
    }

    fn system_lookup(&self, ps: &str) -> Option<Typeface> {
        let mut index = self.system_index.borrow_mut();
        let index = index.get_or_insert_with(|| {
            let mut m = HashMap::new();
            for family in self.system.family_names() {
                let mut set = self.system.match_family(&family);
                for i in 0..set.count() {
                    if let Some(tf) = set.new_typeface(i) {
                        if let Some(name) = tf.post_script_name() {
                            m.insert(name, tf);
                        }
                    }
                }
            }
            m
        });
        index.get(ps).cloned()
    }
}

/// "OpenSans-SemiBoldItalic" -> ("OpenSans", semibold italic), for fonts we could not find.
/// "RobotoCondensed" -> "Roboto Condensed", "DMSans" -> "DM Sans".
fn spaced(family: &str) -> String {
    let c: Vec<char> = family.chars().collect();
    let mut out = String::new();
    for (i, &ch) in c.iter().enumerate() {
        let next_lower = c.get(i + 1).is_some_and(|n| n.is_lowercase());
        if i > 0 && ch.is_uppercase() && (c[i - 1].is_lowercase() || next_lower) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

fn guess_font(ps: &str) -> (String, FontStyle) {
    use skia_safe::font_style::{Slant, Weight, Width};
    let (family, rest) = ps.split_once('-').unwrap_or((ps, ""));
    let r = rest.to_lowercase();
    let weight = [
        ("thin", 100),
        ("extralight", 200),
        ("ultralight", 200),
        ("light", 300),
        ("medium", 500),
        ("semibold", 600),
        ("demibold", 600),
        ("extrabold", 800),
        ("heavy", 800),
        ("bold", 700),
        ("black", 900),
    ]
    .iter()
    .find(|(k, _)| r.contains(k))
    .map_or(400, |(_, w)| *w);
    let slant = if r.contains("italic") || r.contains("oblique") { Slant::Italic } else { Slant::Upright };
    (family.to_string(), FontStyle::new(Weight::from(weight), Width::NORMAL, slant))
}

fn draw_pattern(c: &Canvas, img: &Image, fill: &Value, w: f32, h: f32) {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let mut p = Paint::default();
    p.set_alpha_f(fill["contextSettings"]["opacity"].as_f64().unwrap_or(1.0) as f32);
    let dst = match fill["patternFillType"].as_i64() {
        Some(0) => {
            let scale = fill["patternTileScale"].as_f64().unwrap_or(1.0) as f32;
            let m = Matrix::scale((scale, scale));
            p.set_shader(img.to_shader((TileMode::Repeat, TileMode::Repeat), SamplingOptions::default(), &m));
            c.draw_rect(Rect::from_wh(w, h), &p);
            return;
        }
        Some(2) => Rect::from_wh(w, h),
        t => {
            let scale = if t == Some(3) { (w / iw).min(h / ih) } else { (w / iw).max(h / ih) };
            Rect::from_xywh((w - iw * scale) / 2.0, (h - ih * scale) / 2.0, iw * scale, ih * scale)
        }
    };
    c.draw_image_rect(img, None, dst, &p);
}

/// Paint for a fill or border: flat colour or gradient (image fills are drawn separately).
fn fill_paint(fill: &Value, w: f32, h: f32) -> Option<Paint> {
    let mut p = Paint::default();
    p.set_anti_alias(true);
    match fill["fillType"].as_i64().unwrap_or(0) {
        0 => {
            p.set_color4f(to_color4f(&fill["color"]), None);
        }
        1 => {
            p.set_shader(gradient(&fill["gradient"], w, h)?);
        }
        _ => return None,
    }
    let cs = &fill["contextSettings"];
    p.set_alpha_f(p.alpha_f() * cs["opacity"].as_f64().unwrap_or(1.0) as f32);
    p.set_blend_mode(blend_mode(cs));
    Some(p)
}

fn gradient(g: &Value, w: f32, h: f32) -> Option<Shader> {
    let mut stops: Vec<(f32, Color4f)> =
        arr(g, "stops").iter().map(|st| (num(st, "position"), to_color4f(&st["color"]))).collect();
    stops.sort_by(|a, b| a.0.total_cmp(&b.0));
    let pos: Vec<f32> = stops.iter().map(|s| s.0).collect();
    let colors: Vec<Color4f> = stops.iter().map(|s| s.1).collect();
    let colors = || GradientShaderColors::ColorsInSpace(&colors, None);
    let (from, to) = (pt(s(g, "from")), pt(s(g, "to")));
    match g["gradientType"].as_i64() {
        Some(1) => {
            // Radial gradients live in the unit square, so they stretch with the layer.
            let (dx, dy) = (to.x - from.x, to.y - from.y);
            let ellipse = match num(g, "elipseLength") {
                e if e > 0.0 => e,
                _ => 1.0,
            };
            let mut m = Matrix::scale((w, h));
            m.pre_translate(from).pre_rotate(dy.atan2(dx).to_degrees(), None).pre_scale((1.0, ellipse), None);
            Shader::radial_gradient((0.0, 0.0), dx.hypot(dy), colors(), &pos[..], TileMode::Clamp, None, &m)
        }
        Some(2) => {
            Shader::sweep_gradient((w / 2.0, h / 2.0), colors(), &pos[..], TileMode::Clamp, None, None, None)
        }
        _ => Shader::linear_gradient(
            ((from.x * w, from.y * h), (to.x * w, to.y * h)),
            colors(),
            &pos[..],
            TileMode::Clamp,
            None,
            None,
        ),
    }
}

/// Outline from a layer's curve points, which are stored relative to its frame size.
fn points_path(l: &Value) -> Path {
    let f = frame(l);
    let (w, h) = (f.width(), f.height());
    let pts = arr(l, "points");
    let mut path = Path::new();
    let n = pts.len();
    if n == 0 {
        return path;
    }
    let closed = flag(l, "isClosed");
    let at = |p: &Value, k: &str| {
        let q = pt(s(p, k));
        Point::new(q.x * w, q.y * h)
    };
    let fixed = num(l, "fixedRadius");
    let radius = |i: usize| match num(&pts[i], "cornerRadius") {
        0.0 if cls(l) == "rectangle" => fixed,
        r => r,
    };
    // Straight segment i -> j has no control points.
    let straight = |i: usize, j: usize| !flag(&pts[i], "hasCurveFrom") && !flag(&pts[j], "hasCurveTo");
    let rounded = |i: usize| {
        radius(i) > 0.0 && (closed || (i > 0 && i < n - 1)) && straight((i + n - 1) % n, i) && straight(i, (i + 1) % n)
    };
    let segment = |path: &mut Path, i: usize, j: usize| {
        if straight(i, j) {
            path.line_to(at(&pts[j], "point"));
        } else {
            let a = if flag(&pts[i], "hasCurveFrom") { at(&pts[i], "curveFrom") } else { at(&pts[i], "point") };
            let b = if flag(&pts[j], "hasCurveTo") { at(&pts[j], "curveTo") } else { at(&pts[j], "point") };
            path.cubic_to(a, b, at(&pts[j], "point"));
        }
    };
    let corner = |path: &mut Path, i: usize| {
        let (prev, p, next) = (at(&pts[(i + n - 1) % n], "point"), at(&pts[i], "point"), at(&pts[(i + 1) % n], "point"));
        // Clamp so the arc's tangent points stay within half of each adjacent edge.
        let (a, b) = (prev - p, next - p);
        let (la, lb) = (a.length(), b.length());
        let angle = (a.dot(b) / (la * lb)).clamp(-1.0, 1.0).acos();
        let max_tangent = la.min(lb) / 2.0;
        let r = radius(i).min(max_tangent * (angle / 2.0).tan());
        path.arc_to_tangent(p, next, r);
    };

    let p0 = at(&pts[0], "point");
    let start = if closed && rounded(0) { Point::new((p0.x + at(&pts[1 % n], "point").x) / 2.0, (p0.y + at(&pts[1 % n], "point").y) / 2.0) } else { p0 };
    path.move_to(start);
    let last = if closed { n } else { n - 1 };
    for k in 1..=last {
        let i = k % n;
        if rounded(i) {
            corner(&mut path, i);
        } else {
            segment(&mut path, k - 1, i);
        }
    }
    if closed {
        path.close();
    }
    path
}

/// A copy of a symbol master's layer, refitted to a new parent size by its resizing constraints.
fn resized(l: &Value, old: (f32, f32), new: (f32, f32)) -> Value {
    let mut l = l.clone();
    let rc = l["resizingConstraint"].as_i64().unwrap_or(63);
    let fixed = |bit: i64| rc & bit == 0;
    let f = frame(&l);
    let (x, w) = axis(f.x(), f.width(), old.0, new.0, fixed(4), fixed(2), fixed(1));
    let (y, h) = axis(f.y(), f.height(), old.1, new.1, fixed(32), fixed(16), fixed(8));
    if matches!(cls(&l), "group" | "shapeGroup") && (w, h) != (f.width(), f.height()) {
        let kids: Vec<Value> = arr(&l, "layers").iter().map(|k| resized(k, (f.width(), f.height()), (w, h))).collect();
        l["layers"] = Value::Array(kids);
    }
    let fr = &mut l["frame"];
    fr["x"] = x.into();
    fr["y"] = y.into();
    fr["width"] = w.into();
    fr["height"] = h.into();
    l
}

/// Position and size along one axis; `lo`/`size`/`hi` say which of left, width, right are pinned.
fn axis(p: f32, s: f32, old: f32, new: f32, lo: bool, size: bool, hi: bool) -> (f32, f32) {
    let k = if old > 0.0 { new / old } else { 1.0 };
    let far = old - p - s;
    match (lo, size, hi) {
        (true, _, true) => (p, new - p - far),
        (true, true, false) => (p, s),
        (false, true, true) => (new - far - s, s),
        (true, false, false) => (p, s * k),
        (false, false, true) => (new - far - s * k, s * k),
        (false, true, false) => ((p + s / 2.0) * k - s / 2.0, s),
        (false, false, false) => (p * k, s * k),
    }
}

fn layer_matrix(l: &Value) -> Matrix {
    let f = frame(l);
    let mut m = Matrix::translate((f.x(), f.y()));
    let rot = num(l, "rotation");
    let (fh, fv) = (flag(l, "isFlippedHorizontal"), flag(l, "isFlippedVertical"));
    if rot != 0.0 || fh || fv {
        let c = (f.width() / 2.0, f.height() / 2.0);
        m.pre_translate(c);
        // Sketch flips in the parent's frame, after rotating.
        m.pre_scale((if fh { -1.0 } else { 1.0 }, if fv { -1.0 } else { 1.0 }), None);
        m.pre_rotate(-rot, None);
        m.pre_translate((-c.0, -c.1));
    }
    m
}

/// The area a board covers: its own size, or for a page the bounds of its layers.
fn board_rect(b: &Value) -> Rect {
    if cls(b) != "page" {
        return frame_rect(b);
    }
    let mut r = Rect::new_empty();
    for l in arr(b, "layers").iter().filter(|l| visible(l)) {
        r.join(layer_matrix(l).map_rect(frame_rect(l)).0);
    }
    r
}

/// A layer's own area plus how far a filter reaches, in its local coordinates; with slack for
/// content (text, unfitted groups) that spills a little past the frame.
fn filter_bounds(l: &Value, reach: f32) -> Rect {
    let f = frame(l);
    Rect::from_wh(f.width(), f.height()).with_outset((reach + 8.0, reach + 8.0))
}

fn frame(l: &Value) -> Rect {
    let f = &l["frame"];
    Rect::from_xywh(num(f, "x"), num(f, "y"), num(f, "width"), num(f, "height"))
}

fn frame_rect(l: &Value) -> Rect {
    let f = frame(l);
    Rect::from_wh(f.width(), f.height())
}

fn blend_mode(context: &Value) -> BlendMode {
    use BlendMode::*;
    // Sketch's order; "plus darker" has no Skia equivalent, multiply is close.
    const MODES: [BlendMode; 18] = [
        SrcOver, Darken, Multiply, ColorBurn, Lighten, Screen, ColorDodge, Overlay, SoftLight, HardLight,
        Difference, Exclusion, Hue, Saturation, Color, Luminosity, Multiply, Plus,
    ];
    MODES.get(num(context, "blendMode") as usize).copied().unwrap_or(SrcOver)
}

fn to_color4f(c: &Value) -> Color4f {
    let a = c["alpha"].as_f64().unwrap_or(1.0) as f32;
    Color4f::new(num(c, "red"), num(c, "green"), num(c, "blue"), a)
}

fn shadow_color(sh: &Value) -> Color4f {
    let mut col = to_color4f(&sh["color"]);
    col.a *= sh["contextSettings"]["opacity"].as_f64().unwrap_or(1.0) as f32;
    col
}

fn color_paint(c: &Value) -> Paint {
    let mut p = Paint::default();
    p.set_color4f(to_color4f(c), None);
    p
}

/// Parse Sketch's "{x, y}" point strings.
fn pt(v: &str) -> Point {
    let mut it = v.trim_matches(|c| c == '{' || c == '}').split(',').map(|n| n.trim().parse::<f32>().unwrap_or(0.0));
    Point::new(it.next().unwrap_or(0.0), it.next().unwrap_or(0.0))
}

fn enabled<'a>(style: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    arr(style, key).iter().filter(|x| flag(x, "isEnabled"))
}

/// Radius and saturation of an enabled background blur.
fn backdrop_blur(style: &Value) -> Option<(f32, f32)> {
    let b = &style["blur"];
    (flag(b, "isEnabled") && b["type"].as_i64() == Some(3))
        .then(|| (num(b, "radius"), b["saturation"].as_f64().unwrap_or(1.0) as f32))
}

fn cls(v: &Value) -> &str {
    s(v, "_class")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

fn num(v: &Value, k: &str) -> f32 {
    v[k].as_f64().unwrap_or(0.0) as f32
}

fn flag(v: &Value, k: &str) -> bool {
    v[k].as_bool().unwrap_or_else(|| v[k].as_i64().is_some_and(|n| n != 0))
}

fn visible(l: &Value) -> bool {
    l["isVisible"].as_bool().unwrap_or(true)
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v[k].as_array().map_or(&[], |a| a.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(pt("{0.5, 1}"), Point::new(0.5, 1.0));
        // Pinned left and right: stretches. Nothing pinned: scales.
        assert_eq!(axis(10.0, 20.0, 100.0, 200.0, true, false, true), (10.0, 120.0));
        assert_eq!(axis(10.0, 20.0, 100.0, 200.0, false, false, false), (20.0, 40.0));
        assert_eq!(guess_font("OpenSans-SemiBoldItalic").0, "OpenSans");
        assert_eq!(spaced("DMSans"), "DM Sans");
        assert_eq!(spaced("RobotoCondensed"), "Roboto Condensed");
    }

    /// Converts every sample in `../samples/sketch` (when present) and checks the result is a PDF.
    #[test]
    fn samples() {
        let Ok(dir) = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../samples/sketch")) else { return };
        for e in dir.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "sketch")) {
            let out = convert(&std::fs::read(e.path()).unwrap()).unwrap_or_else(|er| panic!("{:?}: {er}", e.path()));
            assert!(out.pdf.starts_with(b"%PDF"), "{:?}", e.path());
        }
    }
}
