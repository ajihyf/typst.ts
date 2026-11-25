// todo
#![allow(clippy::arc_with_non_send_sync)]

mod bounds;
mod device;
#[cfg(feature = "incremental")]
mod incr;
mod ops;
mod paint;
#[cfg(feature = "rasterize_glyph")]
mod pixglyph_canvas;
mod utils;

pub use bounds::{hit_canvas_bound_at, BBoxAt, CanvasBound};
pub use device::CanvasDevice;
#[cfg(feature = "incremental")]
pub use incr::*;
use js_sys::Promise;
pub use ops::*;
pub use paint::*;
use web_sys::{Blob, HtmlImageElement, OffscreenCanvas, OffscreenCanvasRenderingContext2d};

use std::{
    cell::OnceCell,
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use ecow::EcoVec;
use reflexo::{
    hash::Fingerprint,
    vector::{
        ir::{
            self, Abs, Axes, FontIndice, FontItem, FontRef, Image, ImmutStr, Module, Point, Ratio,
            Rect, Scalar, Size,
        },
        vm::{GroupContext, RenderVm, TransformContext},
    },
};
use tiny_skia as sk;
use wasm_bindgen::{prelude::Closure, JsCast, JsValue};

use bounds::*;

/// All the features that can be enabled or disabled.
pub trait ExportFeature {
    /// Whether to enable tracing.
    const ENABLE_TRACING: bool;
}

/// The default feature set which is used for exporting full-fledged canvas.
pub struct DefaultExportFeature;
/// The default feature set which is used for exporting canvas for printing.
pub type DefaultCanvasTask = CanvasTask<DefaultExportFeature>;

impl ExportFeature for DefaultExportFeature {
    const ENABLE_TRACING: bool = false;
}

#[derive(Clone, Copy)]
pub struct BrowserFontMetric {
    pub semi_char_width: f32,
    pub full_char_width: f32,
    pub emoji_width: f32,
    // height: f32,
}

impl BrowserFontMetric {
    pub fn from_env() -> Self {
        let v = OffscreenCanvas::new(0, 0).expect("offscreen canvas is not supported");
        let ctx = v
            .get_context("2d")
            .unwrap()
            .unwrap()
            .dyn_into::<OffscreenCanvasRenderingContext2d>()
            .unwrap();

        let _g = CanvasStateGuard::new(&ctx);
        ctx.set_font("128px monospace");
        let metrics = ctx.measure_text("A").unwrap();
        let semi_char_width = metrics.width();
        let metrics = ctx.measure_text("喵").unwrap();
        let full_char_width = metrics.width();
        let metrics = ctx.measure_text("🦄").unwrap();
        let emoji_width = metrics.width();
        // let a_height =
        //     (metrics.font_bounding_box_descent() +
        // metrics.font_bounding_box_ascent()).abs();

        Self {
            semi_char_width: (semi_char_width / 128.) as f32,
            full_char_width: (full_char_width / 128.) as f32,
            emoji_width: (emoji_width / 128.) as f32,
            // height: (a_height / 128.) as f32,
        }
    }

    /// Create a new instance for testing.
    /// The width are prime numbers for helping test.
    pub fn new_test() -> Self {
        Self {
            semi_char_width: 2.0,
            full_char_width: 3.0,
            emoji_width: 5.0,
            // height: 7.0,
        }
    }
}

/// A rendered page of canvas.
#[derive(Clone)]
pub struct CanvasPage {
    /// A rendered canvas element.
    pub elem: CanvasNode,
    /// The fingerprint of the content for identifying page difference.
    pub content: Fingerprint,
    /// The size of the page.
    pub size: Size,
}

/// The task context for exporting canvas.
/// It is also as a namespace for all the functions used in the task.
pub struct CanvasTask<Feat: ExportFeature> {
    _feat_phantom: std::marker::PhantomData<Feat>,
}

/// Unfortunately, `Default` derive does not work for generic structs.
impl<Feat: ExportFeature> Default for CanvasTask<Feat> {
    fn default() -> Self {
        Self {
            _feat_phantom: std::marker::PhantomData,
        }
    }
}

impl<Feat: ExportFeature> CanvasTask<Feat> {
    /// fork a render task with module.
    pub fn fork_canvas_render_task<'m, 't>(
        &'t mut self,
        module: &'m ir::Module,
    ) -> CanvasRenderTask<'m, 't, Feat> {
        CanvasRenderTask::<Feat> {
            module,

            use_stable_glyph_id: true,
            glyph_cache: HashMap::new(),

            _feat_phantom: Default::default(),
        }
    }
}

trait GlyphFactory {
    fn get_glyph(
        &mut self,
        font: &FontItem,
        glyph: u32,
        fill: Option<CanvasPaint>,
        stroke: Option<CanvasStroke>,
    ) -> Option<CanvasNode>;
    fn get_paint(&self, fill: &ImmutStr) -> CanvasPaint;
}

/// Holds the data for rendering canvas.
///
/// The 'm lifetime is the lifetime of the module which stores the frame data.
/// The 't lifetime is the lifetime of SVG task.
pub struct CanvasRenderTask<'m, 't, Feat: ExportFeature> {
    /// The module which stores the frame data.
    pub module: &'m Module,

    /// See [`ExportFeature`].
    pub use_stable_glyph_id: bool,

    glyph_cache: HashMap<CanvasGlyphCacheKey, CanvasNode>,

    _feat_phantom: std::marker::PhantomData<&'t Feat>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct CanvasGlyphCacheKey {
    font: Fingerprint,
    glyph: u32,
    fill: Option<ImmutStr>,
    stroke: Option<(ImmutStr, Arc<CanvasStrokeAttrs>)>,
}

impl<'m, Feat: ExportFeature> FontIndice<'m> for CanvasRenderTask<'m, '_, Feat> {
    fn get_font(&self, value: &FontRef) -> Option<&'m ir::FontItem> {
        self.module.fonts.get(value.idx as usize)
    }
}

impl<Feat: ExportFeature> GlyphFactory for CanvasRenderTask<'_, '_, Feat> {
    fn get_glyph(
        &mut self,
        font: &FontItem,
        glyph: u32,
        fill: Option<CanvasPaint>,
        stroke: Option<CanvasStroke>,
    ) -> Option<CanvasNode> {
        let cacheable_fill = match &fill {
            None => Some(None),
            Some(CanvasPaint::Solid(color)) => Some(Some(color.clone())),
            Some(CanvasPaint::Gradient(_) | CanvasPaint::Unsupported) => None,
        };
        let cacheable_stroke = match &stroke {
            None => Some(None),
            Some(CanvasStroke {
                paint: CanvasPaint::Solid(color),
                attrs,
            }) => Some(Some((color.clone(), attrs.clone()))),
            Some(_) => None,
        };

        if let (Some(fill_key), Some(stroke_key)) = (cacheable_fill, cacheable_stroke) {
            let key = CanvasGlyphCacheKey {
                font: font.fingerprint,
                glyph,
                fill: fill_key,
                stroke: stroke_key,
            };
            if let Some(cached) = self.glyph_cache.get(&key) {
                return Some(cached.clone());
            }

            let glyph_data = font.get_glyph(glyph)?;
            let node = Arc::new(CanvasElem::Glyph(CanvasGlyphElem {
                fill,
                stroke,
                upem: font.units_per_em,
                glyph_data: glyph_data.clone(),
                path: Default::default(),
                bbox: Default::default(),
            }));
            self.glyph_cache.insert(key, node.clone());
            return Some(node);
        }

        let glyph_data = font.get_glyph(glyph)?;
        Some(Arc::new(CanvasElem::Glyph(CanvasGlyphElem {
            fill,
            stroke,
            upem: font.units_per_em,
            glyph_data: glyph_data.clone(),
            path: Default::default(),
            bbox: Default::default(),
        })))
    }

    fn get_paint(&self, fill: &ImmutStr) -> CanvasPaint {
        CanvasPaint::from_ref(self.module, fill)
    }
}

impl<'m, Feat: ExportFeature> RenderVm<'m> for CanvasRenderTask<'m, '_, Feat> {
    // type Resultant = String;
    type Resultant = CanvasNode;
    type Group = CanvasStack;

    fn get_item(&self, value: &Fingerprint) -> Option<&'m ir::VecItem> {
        self.module.get_item(value)
    }

    fn start_group(&mut self, _v: &Fingerprint) -> Self::Group {
        Self::Group {
            kind: GroupKind::General,
            ts: sk::Transform::identity(),
            clipper: None,
            fill: None,
            stroke: None,
            glyph_scale: None,
            inner: EcoVec::new(),
            rect: CanvasBBox::Dynamic(Box::new(OnceCell::new())),
        }
    }

    fn start_text(&mut self, value: &Fingerprint, text: &ir::TextItem) -> Self::Group {
        let mut g = self.start_group(value);
        g.kind = GroupKind::Text;
        let font = self.get_font(&text.shape.font).unwrap();
        let upem = Scalar(font.units_per_em.0);
        let mut stroke_paint = None;
        let mut stroke_width_pt = 0.0_f32;
        let mut stroke_line_cap = None;
        let mut stroke_line_join = None;
        let mut stroke_miter_limit = None;
        let mut stroke_dash_array: Option<Arc<[Scalar]>> = None;
        let mut stroke_dash_offset: Option<Scalar> = None;

        for style in &text.shape.styles {
            match style {
                ir::PathStyle::Fill(fill) => {
                    g.fill = Some(fill.clone());
                }
                ir::PathStyle::Stroke(stroke) => {
                    stroke_paint = Some(stroke.clone());
                }
                ir::PathStyle::StrokeWidth(width) => {
                    stroke_width_pt = width.0;
                }
                ir::PathStyle::StrokeLineCap(cap) => {
                    stroke_line_cap = Some(cap.clone());
                }
                ir::PathStyle::StrokeLineJoin(join) => {
                    stroke_line_join = Some(join.clone());
                }
                ir::PathStyle::StrokeMitterLimit(limit) => {
                    stroke_miter_limit = Some(*limit);
                }
                ir::PathStyle::StrokeDashArray(array) => {
                    stroke_dash_array = Some(array.clone());
                }
                ir::PathStyle::StrokeDashOffset(offset) => {
                    stroke_dash_offset = Some(*offset);
                }
                ir::PathStyle::FillRule(_) => {}
            }
        }

        let inv_ppem = if text.shape.size.0 > 0.0 && upem.0 > 0.0 {
            let scale = upem.0 / text.shape.size.0;
            scale.is_finite().then_some(scale)
        } else {
            None
        };
        if let (Some(paint), Some(inv_ppem)) = (stroke_paint, inv_ppem) {
            if stroke_width_pt.is_finite() && stroke_width_pt > 1e-5 {
                let scaled_width = stroke_width_pt * inv_ppem;
                if scaled_width.is_finite() && scaled_width > 0.0 {
                    let dash_array = stroke_dash_array.and_then(|arr| {
                        let scaled: Option<Vec<Scalar>> = arr
                            .iter()
                            .map(|d| {
                                let v = d.0 * inv_ppem;
                                (v.is_finite() && v >= 0.0).then_some(Scalar(v))
                            })
                            .collect();
                        scaled.map(Arc::from)
                    });
                    let dash_offset = stroke_dash_offset.and_then(|offset| {
                        let v = offset.0 * inv_ppem;
                        v.is_finite().then_some(Scalar(v))
                    });
                    let miter_limit =
                        stroke_miter_limit.filter(|limit| limit.0.is_finite() && limit.0 > 0.0);
                    g.stroke = Some((
                        paint,
                        Arc::new(CanvasStrokeAttrs {
                            width: Scalar(scaled_width),
                            line_cap: stroke_line_cap,
                            line_join: stroke_line_join,
                            miter_limit,
                            dash_array,
                            dash_offset,
                        }),
                    ));
                }
            }
        }

        g.rect = if g.stroke.is_some() {
            // Stroked glyphs (e.g. italic overhangs such as `f` with thick strokes) can extend
            // outside the advance-based font metric box. Use a dynamic group bbox so
            // `CanvasGroupElem::bbox_at` unions every inner glyph's stroke-padded outline bounds
            // before windowed culling runs.
            CanvasBBox::Dynamic(Box::new(OnceCell::new()))
        } else {
            // upem is the unit per em defined in the font.
            let accender = Scalar(font.ascender.0) * upem;

            // todo: glyphs like macron has zero width... why?
            let w = text.width();

            let rect = if text.shape.size.0 == 0. {
                Rect {
                    lo: Point::new(Scalar(0.), accender - upem),
                    hi: Point::new(Scalar(0.), accender),
                }
            } else {
                Rect {
                    lo: Point::new(Scalar(0.), accender - upem),
                    hi: Point::new(w * upem / text.shape.size, accender),
                }
            }
            .cano();
            CanvasBBox::Static(Box::new(rect))
        };
        g.glyph_scale = Some(Scalar(text.shape.size.0 / upem.0));
        g
    }
}

/// A stacked builder for [`CanvasNode`].
///
/// It holds state of the building process.
pub struct CanvasStack {
    /// The kind of the group.
    pub kind: GroupKind,
    /// The transform matrix.
    pub ts: sk::Transform,
    /// A unique clip path on stack
    pub clipper: Option<ir::PathItem>,
    /// The fill color.
    pub fill: Option<ImmutStr>,
    /// The text stroke paint and glyph-space attributes.
    pub stroke: Option<(ImmutStr, Arc<CanvasStrokeAttrs>)>,
    /// The font-size to font-unit scale for text glyphs.
    pub glyph_scale: Option<Scalar>,
    /// The inner elements.
    pub inner: EcoVec<(ir::Point, CanvasNode)>,
    /// The bounding box of the group.
    pub rect: CanvasBBox,
}

impl From<CanvasStack> for CanvasNode {
    fn from(s: CanvasStack) -> Self {
        let inner: CanvasNode = Arc::new(CanvasElem::Group(CanvasGroupElem {
            ts: Box::new(s.ts),
            inner: s.inner,
            kind: s.kind,
            rect: s.rect,
            #[cfg(feature = "bitmap_cache_word")]
            bitmap_cache: Default::default(),
        }));
        if let Some(clipper) = s.clipper {
            Arc::new(CanvasElem::Clip(CanvasClipElem {
                d: clipper.d,
                inner,
                clip_bbox: CanvasBBox::Dynamic(Box::new(OnceCell::new())),
                path: Default::default(),
            }))
        } else {
            inner
        }
    }
}

/// See [`TransformContext`].
impl<C> TransformContext<C> for CanvasStack {
    fn transform_matrix(mut self, _ctx: &mut C, m: &ir::Transform) -> Self {
        let sub_ts: sk::Transform = (*m).into();
        self.ts = self.ts.post_concat(sub_ts);
        self
    }

    fn transform_translate(mut self, _ctx: &mut C, matrix: Axes<Abs>) -> Self {
        self.ts = self.ts.post_translate(matrix.x.0, matrix.y.0);
        self
    }

    fn transform_scale(mut self, _ctx: &mut C, x: Ratio, y: Ratio) -> Self {
        self.ts = self.ts.post_scale(x.0, y.0);
        self
    }

    fn transform_rotate(self, _ctx: &mut C, _matrix: Scalar) -> Self {
        todo!()
    }

    fn transform_skew(mut self, _ctx: &mut C, matrix: (Ratio, Ratio)) -> Self {
        self.ts = self.ts.post_concat(sk::Transform {
            sx: 1.,
            sy: 1.,
            kx: matrix.0 .0,
            ky: matrix.1 .0,
            tx: 0.,
            ty: 0.,
        });
        self
    }

    fn transform_clip(mut self, _ctx: &mut C, matrix: &ir::PathItem) -> Self {
        self.clipper = Some(matrix.clone());
        self
    }
}

/// See [`GroupContext`].
impl<'m, C: RenderVm<'m, Resultant = CanvasNode> + GlyphFactory> GroupContext<C> for CanvasStack {
    fn render_path(&mut self, ctx: &mut C, path: &ir::PathItem, _abs_ref: &Fingerprint) {
        self.inner.push((
            ir::Point::default(),
            Arc::new(CanvasElem::Path(CanvasPathElem {
                path_data: Box::new(path.clone()),
                fill: path.styles.iter().find_map(|style| match style {
                    ir::PathStyle::Fill(fill) => Some(ctx.get_paint(fill)),
                    _ => None,
                }),
                stroke: path.styles.iter().find_map(|style| match style {
                    ir::PathStyle::Stroke(stroke) => Some(ctx.get_paint(stroke)),
                    _ => None,
                }),
                rect: CanvasBBox::Dynamic(Box::new(OnceCell::new())),
                path: Default::default(),
            })),
        ))
    }

    fn render_image(&mut self, _ctx: &mut C, image_item: &ir::ImageItem) {
        self.inner.push((
            ir::Point::default(),
            Arc::new(CanvasElem::Image(CanvasImageElem {
                image_data: image_item.clone(),
            })),
        ))
    }

    fn render_item_at(&mut self, ctx: &mut C, pos: crate::ir::Point, item: &Fingerprint) {
        self.inner.push((pos, ctx.render_item(item)));
    }

    fn render_glyph(&mut self, ctx: &mut C, pos: Axes<Scalar>, font: &FontItem, glyph: u32) {
        let mut fill = self.fill.as_ref().map(|fill| ctx.get_paint(fill));
        if let (Some(fill), Some(glyph_scale)) = (fill.as_mut(), self.glyph_scale) {
            *fill = fill.for_glyph(pos, glyph_scale, font, glyph);
        }

        let stroke = self.stroke.as_ref().map(|(paint, attrs)| {
            let mut stroke_paint = ctx.get_paint(paint);
            if let Some(glyph_scale) = self.glyph_scale {
                stroke_paint = stroke_paint.for_glyph(pos, glyph_scale, font, glyph);
            }
            CanvasStroke {
                paint: stroke_paint,
                attrs: attrs.clone(),
            }
        });

        if let Some(glyph) = ctx.get_glyph(font, glyph, fill, stroke) {
            self.inner.push((ir::Point::new(pos.x, pos.y), glyph));
        }
    }
}

#[inline]
#[must_use]
fn set_transform(canvas: &dyn CanvasDevice, transform: sk::Transform) -> bool {
    if transform.sx == 0. || transform.sy == 0. {
        return false;
    }

    // see sync_transform
    let a = transform.sx as f64;
    let b = transform.ky as f64;
    let c = transform.kx as f64;
    let d = transform.sy as f64;
    let e = transform.tx as f64;
    let f = transform.ty as f64;

    canvas.set_transform(a, b, c, d, e, f);
    true
}

/// A guard for saving and restoring the canvas state.
///
/// When the guard is created, a cheap checkpoint of the canvas state is saved.
/// When the guard is dropped, the canvas state is restored.
pub struct CanvasStateGuard<'a>(&'a dyn CanvasDevice);

impl<'a> CanvasStateGuard<'a> {
    pub fn new(context: &'a dyn CanvasDevice) -> Self {
        context.save();
        Self(context)
    }
}

impl Drop for CanvasStateGuard<'_> {
    fn drop(&mut self) {
        self.0.restore();
    }
}

#[derive(Debug, Clone)]
struct UnsafeMemorize<T>(T);

// Safety: `UnsafeMemorize` is only used in wasm targets
unsafe impl<T> Send for UnsafeMemorize<T> {}
// Safety: `UnsafeMemorize` is only used in wasm targets
unsafe impl<T> Sync for UnsafeMemorize<T> {}

#[derive(Debug, Clone)]
struct LazyImage {
    elem: Promise,
    loaded: Arc<Mutex<Option<JsValue>>>,
}

fn create_image(image: Arc<Image>) -> Option<LazyImage> {
    let is_svg = image.format.contains("svg");

    web_sys::console::log_1(&format!("image format: {:?}", image.format).into());

    let u = js_sys::Uint8Array::new_with_length(image.data.len() as u32);
    u.copy_from(&image.data);

    let f = format!("image/{}", image.format);

    let blob = || {
        let parts = js_sys::Array::new();
        parts.push(&u);

        let tag = web_sys::BlobPropertyBag::new();
        tag.set_type(&f);
        web_sys::Blob::new_with_u8_array_sequence_and_options(
            &parts,
            // todo: security check
            // https://security.stackexchange.com/questions/148507/how-to-prevent-xss-in-svg-file-upload
            // todo: use our custom font
            &tag,
        )
        .unwrap()
    };

    let res = match web_sys::window() {
        Some(e) => {
            if is_svg {
                let blob = blob();
                Some(wasm_bindgen_futures::future_to_promise(async move {
                    // todo: image-rendering is not respected
                    let img = HtmlImageElement::new().unwrap();
                    let p = exception_create_image_blob(&blob, &img);
                    p.await;
                    Ok(html_image_to_bitmap(&img).into())
                }))
            } else {
                e.create_image_bitmap_with_blob(&blob()).ok()
            }
        }
        None => {
            let this = js_sys::global()
                .dyn_into::<web_sys::WorkerGlobalScope>()
                .unwrap();
            if is_svg {
                js_sys::Reflect::get(&this, &JsValue::from_str("loadSvg"))
                    .unwrap()
                    .dyn_into::<js_sys::Function>()
                    .unwrap()
                    .call2(&JsValue::NULL, &u, &f.into())
                    .unwrap()
                    .dyn_into::<Promise>()
                    .ok()
            } else {
                this.create_image_bitmap_with_blob(&blob()).ok()
            }
        }
    };

    let loaded = Arc::new(Mutex::new(None));

    let elem = res.map(|elem| {
        let loaded_that = loaded.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let elem = wasm_bindgen_futures::JsFuture::from(elem).await?;
            *loaded_that.lock().unwrap() = Some(elem.clone());
            Ok(elem)
        })
    });

    elem.map(|elem| LazyImage { elem, loaded })
}

pub fn html_image_to_bitmap(img: &HtmlImageElement) -> web_sys::ImageBitmap {
    let canvas = web_sys::OffscreenCanvas::new(img.width(), img.height()).unwrap();

    let ctx = canvas
        .get_context("2d")
        .expect("get context 2d")
        .expect("get context 2d");
    let ctx = ctx
        .dyn_into::<web_sys::OffscreenCanvasRenderingContext2d>()
        .expect("must be OffscreenCanvasRenderingContext2d");
    ctx.draw_image_with_html_image_element(img, 0., 0.)
        .expect("must draw_image_with_html_image_element");

    canvas
        .transfer_to_image_bitmap()
        .expect("transfer_to_image_bitmap")
}

pub async fn exception_create_image_blob(blob: &Blob, image_elem: &HtmlImageElement) {
    let data_url = web_sys::Url::create_object_url_with_blob(blob).unwrap();

    let img_load_promise = Promise::new(
        &mut move |complete: js_sys::Function, _reject: js_sys::Function| {
            let data_url = data_url.clone();
            let data_url2 = data_url.clone();
            let complete2 = complete.clone();

            image_elem.set_src(&data_url);

            // simulate async callback from another thread
            let a = Closure::<dyn Fn()>::new(move || {
                web_sys::Url::revoke_object_url(&data_url).unwrap();
                complete.call0(&complete).unwrap();
            });

            image_elem.set_onload(Some(a.as_ref().unchecked_ref()));
            a.forget();

            let a = Closure::<dyn Fn(JsValue)>::new(move |e: JsValue| {
                web_sys::Url::revoke_object_url(&data_url2).unwrap();
                complete2.call0(&complete2).unwrap();
                // let end = std::time::Instant::now();
                web_sys::console::log_1(
                    &format!(
                        "err image loading in {:?} {:?} {:?} {}",
                        // end - begin,
                        0,
                        js_sys::Reflect::get(&e, &"type".into()).unwrap(),
                        js_sys::JSON::stringify(&e).unwrap(),
                        data_url2,
                    )
                    .into(),
                );
            });

            image_elem.set_onerror(Some(a.as_ref().unchecked_ref()));
            a.forget();
        },
    );

    wasm_bindgen_futures::JsFuture::from(img_load_promise)
        .await
        .unwrap();
}

#[comemo::memoize]
fn rasterize_image(e: Arc<Image>) -> Option<UnsafeMemorize<LazyImage>> {
    create_image(e).map(UnsafeMemorize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflexo::vector::ir::{
        FlatGlyphItem, GradientItem, GradientKind, ItemMap, OutlineGlyphItem, PathStyle, Rgba8Item,
        TextItem, TextItemContent, TextShape, VecItem,
    };

    fn test_font() -> FontItem {
        let outline = Arc::new(FlatGlyphItem::Outline(Arc::new(OutlineGlyphItem {
            ts: None,
            d: "M 0 0 L 500 0 L 500 700 L 0 700 Z".into(),
            ligature_len: 1,
        })));
        FontItem {
            fingerprint: Fingerprint::from_u128(0x1234),
            family: "TestSong".into(),
            hash: 1,
            cap_height: Scalar(0.7),
            ascender: Scalar(0.88),
            descender: Scalar(-0.12),
            units_per_em: Scalar(1000.0),
            vertical: false,
            glyphs: vec![outline],
            glyph_cov: Default::default(),
        }
    }

    fn make_text_item(size: f32, styles: Vec<PathStyle>) -> VecItem {
        VecItem::Text(TextItem {
            content: Arc::new(TextItemContent {
                content: "A".into(),
                glyphs: Arc::from([(
                    Axes::new(Scalar(0.0), Scalar(0.0)),
                    Axes::new(Scalar(size), Scalar(0.0)),
                    0u32,
                )]),
            }),
            shape: Arc::new(TextShape {
                font: FontRef { hash: 1, idx: 0 },
                dir: "ltr".into(),
                size: Scalar(size),
                styles,
            }),
        })
    }

    fn extract_single_glyph(node: &CanvasNode) -> (&CanvasGroupElem, &CanvasGlyphElem) {
        let CanvasElem::Group(group) = node.as_ref() else {
            panic!("expected CanvasElem::Group");
        };
        let CanvasElem::Glyph(glyph) = group.inner[0].1.as_ref() else {
            panic!("expected CanvasElem::Glyph");
        };
        (group, glyph)
    }

    #[test]
    fn text_stroke_scales_lengths_keeps_miter_limit_and_uses_solid_fast_path() {
        let fg = Fingerprint::from_u128(1);
        let mut items = ItemMap::default();
        items.insert(
            fg,
            make_text_item(
                10.0,
                vec![
                    PathStyle::Fill("#111111".into()),
                    PathStyle::Stroke("#e60012".into()),
                    PathStyle::StrokeWidth(Scalar(0.3)),
                    PathStyle::StrokeLineCap("round".into()),
                    PathStyle::StrokeLineJoin("bevel".into()),
                    PathStyle::StrokeMitterLimit(Scalar(4.0)),
                    PathStyle::StrokeDashArray(Arc::from([Scalar(1.5), Scalar(0.5)])),
                    PathStyle::StrokeDashOffset(Scalar(0.25)),
                ],
            ),
        );
        let module = Module {
            fonts: vec![test_font()],
            glyphs: vec![],
            items,
        };

        let mut task = CanvasTask::<DefaultExportFeature>::default();
        let mut render = task.fork_canvas_render_task(&module);
        let node = render.render_item(&fg);
        let (group, glyph) = extract_single_glyph(&node);

        assert_eq!(
            glyph.fill.as_ref().and_then(CanvasPaint::as_solid_str),
            Some("#111111")
        );
        let stroke = glyph.stroke.as_ref().expect("glyph stroke must be present");
        assert_eq!(stroke.paint.as_solid_str(), Some("#e60012"));
        // inv_ppem = 1000 / 10 = 100
        assert!((stroke.attrs.width.0 - 30.0).abs() < 1e-4);
        assert_eq!(stroke.attrs.line_cap.as_deref(), Some("round"));
        assert_eq!(stroke.attrs.line_join.as_deref(), Some("bevel"));
        // miter_limit is a dimensionless ratio and must NOT be scaled by inv_ppem.
        assert_eq!(stroke.attrs.miter_limit, Some(Scalar(4.0)));
        assert_eq!(
            stroke.attrs.dash_array.as_deref(),
            Some(&[Scalar(150.0), Scalar(50.0)][..])
        );
        assert_eq!(stroke.attrs.dash_offset, Some(Scalar(25.0)));

        #[cfg(not(feature = "rasterize_glyph"))]
        {
            let run_style = group
                .solid_text_run_style()
                .expect("solid fill + solid stroke must use batched text run fast path");
            assert_eq!(run_style.fill, Some("#111111"));
            assert_eq!(run_style.stroke, Some(("#e60012", stroke.attrs.as_ref())));
        }

        // BBox expands by half-width (bevel join => factor 1.0 => 15 upem => 0.15pt).
        let glyph_bbox = glyph
            .bbox_at(sk::Transform::identity())
            .expect("glyph bbox");
        assert!((glyph_bbox.lo.x.0 - (-15.0)).abs() < 1e-3);
        assert!((glyph_bbox.hi.x.0 - 515.0).abs() < 1e-3);
        assert!((glyph_bbox.lo.y.0 - (-15.0)).abs() < 1e-3);
        assert!((glyph_bbox.hi.y.0 - 715.0).abs() < 1e-3);

        // Stroked text group uses dynamic outline bbox scaled by (10/1000, -10/1000).
        let group_bbox = group
            .bbox_at(sk::Transform::identity())
            .expect("group bbox");
        assert!((group_bbox.lo.x.0 - (-0.15)).abs() < 1e-3);
        assert!((group_bbox.hi.x.0 - 5.15).abs() < 1e-3);
        assert!((group_bbox.lo.y.0 - (-7.15)).abs() < 1e-3);
        assert!((group_bbox.hi.y.0 - 0.15).abs() < 1e-3);
    }

    #[test]
    fn glyph_cache_isolates_unstroked_and_differently_stroked_glyphs() {
        let fg_plain = Fingerprint::from_u128(10);
        let fg_bold = Fingerprint::from_u128(11);
        let fg_thicker = Fingerprint::from_u128(12);
        let fg_bold_same = Fingerprint::from_u128(13);

        let mut items = ItemMap::default();
        items.insert(
            fg_plain,
            make_text_item(12.0, vec![PathStyle::Fill("#000000".into())]),
        );
        items.insert(
            fg_bold,
            make_text_item(
                12.0,
                vec![
                    PathStyle::Fill("#000000".into()),
                    PathStyle::Stroke("#000000".into()),
                    PathStyle::StrokeWidth(Scalar(0.34284)),
                    PathStyle::StrokeMitterLimit(Scalar(4.0)),
                ],
            ),
        );
        items.insert(
            fg_thicker,
            make_text_item(
                12.0,
                vec![
                    PathStyle::Fill("#000000".into()),
                    PathStyle::Stroke("#000000".into()),
                    PathStyle::StrokeWidth(Scalar(0.6)),
                    PathStyle::StrokeMitterLimit(Scalar(4.0)),
                ],
            ),
        );
        items.insert(
            fg_bold_same,
            make_text_item(
                12.0,
                vec![
                    PathStyle::Fill("#000000".into()),
                    PathStyle::Stroke("#000000".into()),
                    PathStyle::StrokeWidth(Scalar(0.34284)),
                    PathStyle::StrokeMitterLimit(Scalar(4.0)),
                ],
            ),
        );
        let module = Module {
            fonts: vec![test_font()],
            glyphs: vec![],
            items,
        };

        let mut task = CanvasTask::<DefaultExportFeature>::default();
        let mut render = task.fork_canvas_render_task(&module);

        let plain_node = render.render_item(&fg_plain);
        let bold_node = render.render_item(&fg_bold);
        let thicker_node = render.render_item(&fg_thicker);
        let bold_same_node = render.render_item(&fg_bold_same);

        let (_, plain_glyph) = extract_single_glyph(&plain_node);
        let (bold_group, bold_glyph) = extract_single_glyph(&bold_node);
        let (_, thicker_glyph) = extract_single_glyph(&thicker_node);
        let (bold_same_group, _) = extract_single_glyph(&bold_same_node);

        assert!(plain_glyph.stroke.is_none());
        assert!(bold_glyph.stroke.is_some());
        assert!(thicker_glyph.stroke.is_some());
        assert_ne!(
            bold_glyph.stroke.as_ref().unwrap().attrs.width,
            thicker_glyph.stroke.as_ref().unwrap().attrs.width
        );
        // Identical font + glyph + fill + stroke hits glyph_cache and shares Arc.
        assert!(Arc::ptr_eq(
            &bold_group.inner[0].1,
            &bold_same_group.inner[0].1
        ));
    }

    #[test]
    fn handles_zero_width_zero_size_stroke_only_and_gradient_stroke() {
        let fg_zero_width = Fingerprint::from_u128(20);
        let fg_zero_size = Fingerprint::from_u128(21);
        let fg_stroke_only = Fingerprint::from_u128(22);
        let fg_grad = Fingerprint::from_u128(23);
        let fg_grad_text = Fingerprint::from_u128(24);

        let mut items = ItemMap::default();
        items.insert(
            fg_zero_width,
            make_text_item(
                12.0,
                vec![
                    PathStyle::Fill("#000000".into()),
                    PathStyle::Stroke("#ff0000".into()),
                    PathStyle::StrokeWidth(Scalar(0.0)),
                ],
            ),
        );
        items.insert(
            fg_zero_size,
            make_text_item(
                0.0,
                vec![
                    PathStyle::Fill("#000000".into()),
                    PathStyle::Stroke("#ff0000".into()),
                    PathStyle::StrokeWidth(Scalar(0.5)),
                ],
            ),
        );
        items.insert(
            fg_stroke_only,
            make_text_item(
                10.0,
                vec![
                    PathStyle::Stroke("#0044ff".into()),
                    PathStyle::StrokeWidth(Scalar(0.2)),
                ],
            ),
        );
        items.insert(
            fg_grad,
            VecItem::Gradient(Arc::new(GradientItem {
                stops: vec![
                    (
                        Rgba8Item {
                            r: 255,
                            g: 0,
                            b: 0,
                            a: 255,
                        },
                        Scalar(0.0),
                    ),
                    (
                        Rgba8Item {
                            r: 0,
                            g: 0,
                            b: 255,
                            a: 255,
                        },
                        Scalar(1.0),
                    ),
                ],
                space: reflexo::vector::ir::ColorSpace::Srgb,
                anti_alias: true,
                kind: GradientKind::Linear(Scalar(0.0)),
                styles: vec![],
            })),
        );
        let grad_ref: ImmutStr = format!("@{}", fg_grad.as_svg_id("g")).into();
        items.insert(
            fg_grad_text,
            make_text_item(
                10.0,
                vec![
                    PathStyle::Fill("#000000".into()),
                    PathStyle::Stroke(grad_ref),
                    PathStyle::StrokeWidth(Scalar(0.2)),
                ],
            ),
        );

        let module = Module {
            fonts: vec![test_font()],
            glyphs: vec![],
            items,
        };
        let mut task = CanvasTask::<DefaultExportFeature>::default();
        let mut render = task.fork_canvas_render_task(&module);

        let zero_width_node = render.render_item(&fg_zero_width);
        let (_, g_zero_width) = extract_single_glyph(&zero_width_node);
        assert!(g_zero_width.stroke.is_none());

        let zero_size_node = render.render_item(&fg_zero_size);
        let (_, g_zero_size) = extract_single_glyph(&zero_size_node);
        assert!(g_zero_size.stroke.is_none());

        let stroke_only_node = render.render_item(&fg_stroke_only);
        let (_stroke_only_group, g_stroke_only) = extract_single_glyph(&stroke_only_node);
        assert!(g_stroke_only.fill.is_none());
        assert_eq!(
            g_stroke_only
                .stroke
                .as_ref()
                .and_then(|s| s.paint.as_solid_str()),
            Some("#0044ff")
        );
        #[cfg(not(feature = "rasterize_glyph"))]
        {
            let style = _stroke_only_group
                .solid_text_run_style()
                .expect("stroke-only solid text should use batched path");
            assert!(style.fill.is_none());
            assert_eq!(style.stroke.map(|(c, _)| c), Some("#0044ff"));
        }

        let grad_node = render.render_item(&fg_grad_text);
        let (_grad_group, g_grad) = extract_single_glyph(&grad_node);
        assert!(matches!(
            g_grad.stroke.as_ref().map(|s| &s.paint),
            Some(CanvasPaint::Gradient(_))
        ));
        assert!(
            !g_grad
                .stroke
                .as_ref()
                .unwrap()
                .paint
                .needs_stroked_outline_fill(),
            "linear gradient stroke keeps native createLinearGradient fast path"
        );
        #[cfg(not(feature = "rasterize_glyph"))]
        assert!(
            _grad_group.solid_text_run_style().is_none(),
            "gradient stroke must fall back to per-glyph CanvasGlyphElem::realize"
        );
    }

    #[test]
    fn radial_and_conic_glyph_strokes_require_expanded_outline_fill_and_expand_stroke_svg() {
        let fg_radial = Fingerprint::from_u128(30);
        let fg_radial_ts = Fingerprint::from_u128(31);
        let fg_conic = Fingerprint::from_u128(32);
        let fg_conic_ts = Fingerprint::from_u128(33);
        let fg_radial_text = Fingerprint::from_u128(34);
        let fg_conic_text = Fingerprint::from_u128(35);

        let stops = vec![
            (
                Rgba8Item {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                },
                Scalar(0.0),
            ),
            (
                Rgba8Item {
                    r: 0,
                    g: 0,
                    b: 255,
                    a: 255,
                },
                Scalar(1.0),
            ),
        ];

        let mut items = ItemMap::default();
        items.insert(
            fg_radial,
            VecItem::Gradient(Arc::new(GradientItem {
                stops: stops.clone(),
                space: reflexo::vector::ir::ColorSpace::Oklab,
                anti_alias: true,
                kind: GradientKind::Radial(Scalar(0.5)),
                styles: vec![],
            })),
        );
        items.insert(
            fg_radial_ts,
            VecItem::ColorTransform(Arc::new(ir::ColorTransform {
                transform: ir::Transform::from_scale(Scalar(240.0), Scalar(120.0)),
                item: fg_radial,
            })),
        );
        items.insert(
            fg_conic,
            VecItem::Gradient(Arc::new(GradientItem {
                stops,
                space: reflexo::vector::ir::ColorSpace::Oklab,
                anti_alias: true,
                kind: GradientKind::Conic(Scalar(0.0)),
                styles: vec![],
            })),
        );
        items.insert(
            fg_conic_ts,
            VecItem::ColorTransform(Arc::new(ir::ColorTransform {
                transform: ir::Transform::from_scale(Scalar(240.0), Scalar(120.0)),
                item: fg_conic,
            })),
        );

        let radial_ref: ImmutStr = format!("@{}", fg_radial_ts.as_svg_id("g")).into();
        let conic_ref: ImmutStr = format!("@{}", fg_conic_ts.as_svg_id("g")).into();
        items.insert(
            fg_radial_text,
            make_text_item(
                72.0,
                vec![
                    PathStyle::Stroke(radial_ref),
                    PathStyle::StrokeWidth(Scalar(8.0)),
                ],
            ),
        );
        items.insert(
            fg_conic_text,
            make_text_item(
                72.0,
                vec![
                    PathStyle::Stroke(conic_ref),
                    PathStyle::StrokeWidth(Scalar(5.0)),
                    PathStyle::StrokeLineCap("round".into()),
                    PathStyle::StrokeLineJoin("round".into()),
                    PathStyle::StrokeDashArray(Arc::from([Scalar(6.0)])),
                    PathStyle::StrokeDashOffset(Scalar(1.5)),
                ],
            ),
        );

        let module = Module {
            fonts: vec![test_font()],
            glyphs: vec![],
            items,
        };
        let mut task = CanvasTask::<DefaultExportFeature>::default();
        let mut render = task.fork_canvas_render_task(&module);

        let radial_node = render.render_item(&fg_radial_text);
        let (_, radial_glyph) = extract_single_glyph(&radial_node);
        let radial_stroke = radial_glyph.stroke.as_ref().expect("radial stroke");
        assert!(radial_stroke.paint.needs_stroked_outline_fill());

        let conic_node = render.render_item(&fg_conic_text);
        let (_, conic_glyph) = extract_single_glyph(&conic_node);
        let conic_stroke = conic_glyph.stroke.as_ref().expect("conic stroke");
        assert!(conic_stroke.paint.needs_stroked_outline_fill());

        let outline = "M 0 0 L 500 0 L 500 700 L 0 700 Z";
        let solid_svg = crate::expand_stroked_outline_svg(outline, &radial_stroke.attrs, 0.2)
            .expect("solid stroke expansion must produce non-empty SVG path");
        assert!(solid_svg.starts_with("M "));
        assert!(solid_svg.contains("Z "));

        // Odd-length dash array must duplicate cleanly and produce stroked dashes.
        let dashed_svg = crate::expand_stroked_outline_svg(outline, &conic_stroke.attrs, 0.2)
            .expect("odd-length dashed stroke expansion must produce non-empty SVG path");
        assert!(dashed_svg.starts_with("M "));
        assert_ne!(solid_svg, dashed_svg);

        // Empty or zero-width paths return None cleanly.
        assert!(crate::expand_stroked_outline_svg("", &radial_stroke.attrs, 0.2).is_none());
        let zero_attrs = CanvasStrokeAttrs {
            width: Scalar(0.0),
            line_cap: None,
            line_join: None,
            miter_limit: None,
            dash_array: None,
            dash_offset: None,
        };
        assert!(crate::expand_stroked_outline_svg(outline, &zero_attrs, 0.2).is_none());
    }

    #[test]
    fn stroked_text_group_bbox_covers_italic_overhang_past_advance() {
        // Simulate an italic `f` whose advance is narrow (300 upem) while its outline overhangs
        // well to the left (-180 upem) and right (+680 upem) and vertically (-220..+920 upem).
        let italic_f_outline = Arc::new(FlatGlyphItem::Outline(Arc::new(OutlineGlyphItem {
            ts: None,
            d: "M -180 -220 L 680 -220 L 680 920 L -180 920 Z".into(),
            ligature_len: 1,
        })));
        let font = FontItem {
            fingerprint: Fingerprint::from_u128(0x5678),
            family: "TestItalic".into(),
            hash: 2,
            cap_height: Scalar(0.7),
            ascender: Scalar(0.88),
            descender: Scalar(-0.12),
            units_per_em: Scalar(1000.0),
            vertical: false,
            glyphs: vec![italic_f_outline],
            glyph_cov: Default::default(),
        };

        let fg_text = Fingerprint::from_u128(40);
        let size = 76.0_f32;
        let advance_pt = 0.3 * size; // 22.8 pt, while right overhang reaches 0.68 * 76 = 51.68 pt!
        let mut items = ItemMap::default();
        items.insert(
            fg_text,
            VecItem::Text(TextItem {
                content: Arc::new(TextItemContent {
                    content: "f".into(),
                    glyphs: Arc::from([(
                        Axes::new(Scalar(0.0), Scalar(0.0)),
                        Axes::new(Scalar(advance_pt), Scalar(0.0)),
                        0u32,
                    )]),
                }),
                shape: Arc::new(TextShape {
                    font: FontRef { hash: 2, idx: 0 },
                    dir: "ltr".into(),
                    size: Scalar(size),
                    styles: vec![
                        PathStyle::Stroke("#e60012".into()),
                        PathStyle::StrokeWidth(Scalar(5.0)),
                        PathStyle::StrokeLineCap("round".into()),
                        PathStyle::StrokeLineJoin("round".into()),
                    ],
                }),
            }),
        );

        let module = Module {
            fonts: vec![font],
            glyphs: vec![],
            items,
        };
        let mut task = CanvasTask::<DefaultExportFeature>::default();
        let mut render = task.fork_canvas_render_task(&module);
        let node = render.render_item(&fg_text);

        // At 3 px/pt, right overhang + 2.5 pt half-stroke reaches (51.68 + 2.5) * 3 = 162.54 px,
        // far beyond the advance-only static bound ((22.8 + 2.5) * 3 = 75.9 px).
        let page_ts = sk::Transform::from_scale(3.0, 3.0);
        let bbox = node.bbox_at(page_ts).expect("stroked text group bbox");
        assert!(
            bbox.lo.x.0 <= (-0.18 * size - 2.5) * 3.0 + 1e-2,
            "left overhang + stroke must be covered: {bbox:?}"
        );
        assert!(
            bbox.hi.x.0 >= (0.68 * size + 2.5) * 3.0 - 1e-2,
            "right italic overhang + stroke must be covered: {bbox:?}"
        );
        assert!(
            bbox.lo.y.0 <= (-0.92 * size - 2.5) * 3.0 + 1e-2,
            "top overhang + stroke must be covered: {bbox:?}"
        );
        assert!(
            bbox.hi.y.0 >= (0.22 * size + 2.5) * 3.0 - 1e-2,
            "bottom descender overhang + stroke must be covered: {bbox:?}"
        );
    }
}
