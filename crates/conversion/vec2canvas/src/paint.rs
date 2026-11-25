use std::sync::Arc;

use reflexo::{
    hash::Fingerprint,
    vector::ir::{
        self, ColorSpace, FlatGlyphItem, FontItem, GradientItem, GradientKind, GradientStyle,
        ImmutStr, Module, Rgba8Item, Scalar, Transform, VecItem,
    },
};
use tiny_skia as sk;
use typst::{
    foundations::Smart,
    visualize::{
        Color as TypstColor, ColorSpace as TypstColorSpace, ProcessColorSpace, WeightedColor,
    },
};
use web_sys::{CanvasGradient, Path2d};

use crate::{CanvasDevice, CanvasStateGuard, CanvasStrokeAttrs};

const TAU: f32 = std::f32::consts::TAU;
const CONIC_SEGMENT: usize = 360;

#[derive(Debug, Clone)]
pub enum CanvasPaint {
    Solid(ImmutStr),
    Gradient(CanvasGradientPaint),
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct CanvasGradientPaint {
    pub gradient: Arc<GradientItem>,
    pub transform: Option<Transform>,
    pub aspect_ratio: Option<f32>,
}

impl CanvasPaint {
    pub fn from_ref(module: &Module, paint: &ImmutStr) -> Self {
        if paint.starts_with("@g") {
            return resolve_gradient(module, paint).map_or(Self::Unsupported, Self::Gradient);
        }

        if paint.starts_with('@') {
            Self::Unsupported
        } else {
            Self::Solid(paint.clone())
        }
    }

    pub fn set_fill_style(&self, canvas: &dyn CanvasDevice, ts: sk::Transform) {
        match self {
            Self::Solid(color) => canvas.set_fill_style_str(color.as_ref()),
            Self::Gradient(gradient) => {
                if let Some(gradient) = gradient.to_canvas_gradient(canvas, ts) {
                    canvas.set_fill_style_canvas_gradient(&gradient);
                } else {
                    canvas.set_fill_style_str("black");
                }
            }
            Self::Unsupported => canvas.set_fill_style_str("black"),
        }
    }

    pub fn set_stroke_style(&self, canvas: &dyn CanvasDevice, ts: sk::Transform) {
        match self {
            Self::Solid(color) => canvas.set_stroke_style_str(color.as_ref()),
            Self::Gradient(gradient) => {
                if let Some(gradient) = gradient.to_canvas_gradient(canvas, ts) {
                    canvas.set_stroke_style_canvas_gradient(&gradient);
                } else {
                    canvas.set_stroke_style_str("black");
                }
            }
            Self::Unsupported => canvas.set_stroke_style_str("black"),
        }
    }

    pub fn as_solid_str(&self) -> Option<&str> {
        match self {
            Self::Solid(color) => Some(color.as_ref()),
            Self::Gradient(_) | Self::Unsupported => None,
        }
    }

    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported)
    }

    pub fn for_glyph(
        &self,
        pos: ir::Axes<Scalar>,
        glyph_scale: Scalar,
        font: &FontItem,
        glyph: u32,
    ) -> Self {
        match self {
            Self::Gradient(gradient) => {
                Self::Gradient(gradient.for_glyph(pos, glyph_scale, font, glyph))
            }
            Self::Solid(_) | Self::Unsupported => self.clone(),
        }
    }

    pub fn fill_conic_path(
        &self,
        canvas: &dyn CanvasDevice,
        ts: sk::Transform,
        clip_path: &Path2d,
        clip_bounds: Option<sk::Rect>,
        prefer_native: bool,
    ) -> bool {
        match self {
            Self::Gradient(gradient)
                if matches!(gradient.gradient.kind, GradientKind::Conic(_)) =>
            {
                gradient.fill_conic_path(canvas, ts, clip_path, clip_bounds, prefer_native);
                true
            }
            Self::Solid(_) | Self::Gradient(_) | Self::Unsupported => false,
        }
    }

    pub fn fill_radial_path(
        &self,
        canvas: &dyn CanvasDevice,
        ts: sk::Transform,
        clip_path: &Path2d,
        clip_bounds: Option<sk::Rect>,
    ) -> bool {
        match self {
            Self::Gradient(gradient)
                if matches!(gradient.gradient.kind, GradientKind::Radial(_))
                    && gradient
                        .transform
                        .is_some_and(|transform| !can_use_native_circular_gradient(transform)) =>
            {
                gradient.fill_radial_path(canvas, ts, clip_path, clip_bounds);
                true
            }
            Self::Solid(_) | Self::Gradient(_) | Self::Unsupported => false,
        }
    }

    /// Returns true when painting a path/glyph with this paint requires the
    /// clipped paint-transform path (`fill_radial_path` / `fill_conic_path`)
    /// rather than direct `CanvasRenderingContext2D` fill/stroke styles.
    pub fn needs_stroked_outline_fill(&self) -> bool {
        match self {
            Self::Gradient(gradient) => match gradient.gradient.kind {
                GradientKind::Conic(_) => true,
                GradientKind::Radial(_) => gradient
                    .transform
                    .is_some_and(|transform| !can_use_native_circular_gradient(transform)),
                GradientKind::Linear(_) => false,
            },
            Self::Solid(_) | Self::Unsupported => false,
        }
    }
}

pub(crate) fn path_bounds(path_data: &str) -> Option<sk::Rect> {
    convert_path(path_data).map(|path| path.bounds())
}

pub(crate) fn expand_stroked_outline(
    path_data: &str,
    attrs: &CanvasStrokeAttrs,
    resolution_scale: f32,
) -> Option<(String, sk::Rect)> {
    if !attrs.width.0.is_finite() || attrs.width.0 <= 0.0 {
        return None;
    }

    let mut path = convert_path(path_data)?;
    let res_scale = if resolution_scale.is_finite() && resolution_scale > 0.0 {
        resolution_scale
    } else {
        1.0
    };

    if let Some(dash_array) = attrs.dash_array.as_deref() {
        if !dash_array.is_empty() {
            let mut intervals: Vec<f32> = dash_array.iter().map(|d| d.0).collect();
            if intervals.len() % 2 == 1 {
                intervals.extend_from_within(..);
            }
            let offset = attrs.dash_offset.map_or(0.0, |o| o.0);
            if let Some(dash) = tiny_skia_path::StrokeDash::new(intervals, offset) {
                path = path.dash(&dash, res_scale)?;
            }
        }
    }

    let line_cap = match attrs.line_cap.as_deref() {
        Some("round") => tiny_skia_path::LineCap::Round,
        Some("square") => tiny_skia_path::LineCap::Square,
        _ => tiny_skia_path::LineCap::Butt,
    };
    let line_join = match attrs.line_join.as_deref() {
        Some("round") => tiny_skia_path::LineJoin::Round,
        Some("bevel") => tiny_skia_path::LineJoin::Bevel,
        _ => tiny_skia_path::LineJoin::Miter,
    };
    let miter_limit = attrs.miter_limit.map_or(10.0, |limit| limit.0).max(0.0);
    let sk_stroke = tiny_skia_path::Stroke {
        width: attrs.width.0,
        miter_limit,
        line_cap,
        line_join,
        dash: None,
    };

    let stroked = path.stroke(&sk_stroke, res_scale)?;
    let bounds = stroked.bounds();
    let svg = path_to_svg_string(&stroked)?;
    Some((svg, bounds))
}

pub(crate) fn stroked_outline_path_2d(
    path_data: &str,
    attrs: &CanvasStrokeAttrs,
    ts: sk::Transform,
) -> Option<(Path2d, sk::Rect)> {
    let res_scale = tiny_skia_path::PathStroker::compute_resolution_scale(&ts);
    let (svg, bounds) = expand_stroked_outline(path_data, attrs, res_scale)?;
    let path = Path2d::new_with_path_string(&svg).ok()?;
    Some((path, bounds))
}

fn path_to_svg_string(path: &tiny_skia_path::Path) -> Option<String> {
    use std::fmt::Write as _;

    let mut out = String::new();
    for segment in path.segments() {
        match segment {
            tiny_skia_path::PathSegment::MoveTo(p) => {
                if !p.x.is_finite() || !p.y.is_finite() {
                    return None;
                }
                let _ = write!(out, "M {} {} ", p.x, p.y);
            }
            tiny_skia_path::PathSegment::LineTo(p) => {
                if !p.x.is_finite() || !p.y.is_finite() {
                    return None;
                }
                let _ = write!(out, "L {} {} ", p.x, p.y);
            }
            tiny_skia_path::PathSegment::QuadTo(p1, p) => {
                if !p1.x.is_finite() || !p1.y.is_finite() || !p.x.is_finite() || !p.y.is_finite() {
                    return None;
                }
                let _ = write!(out, "Q {} {} {} {} ", p1.x, p1.y, p.x, p.y);
            }
            tiny_skia_path::PathSegment::CubicTo(p1, p2, p) => {
                if !p1.x.is_finite()
                    || !p1.y.is_finite()
                    || !p2.x.is_finite()
                    || !p2.y.is_finite()
                    || !p.x.is_finite()
                    || !p.y.is_finite()
                {
                    return None;
                }
                let _ = write!(
                    out,
                    "C {} {} {} {} {} {} ",
                    p1.x, p1.y, p2.x, p2.y, p.x, p.y
                );
            }
            tiny_skia_path::PathSegment::Close => {
                out.push_str("Z ");
            }
        }
    }

    (!out.is_empty()).then_some(out)
}

impl CanvasGradientPaint {
    fn to_canvas_gradient(
        &self,
        canvas: &dyn CanvasDevice,
        _ts: sk::Transform,
    ) -> Option<CanvasGradient> {
        let aspect_ratio = self
            .aspect_ratio
            .or_else(|| self.transform.map(transform_aspect_ratio));
        create_canvas_gradient(canvas, &self.gradient, self.transform, aspect_ratio)
    }

    fn for_glyph(
        &self,
        pos: ir::Axes<Scalar>,
        glyph_scale: Scalar,
        font: &FontItem,
        glyph: u32,
    ) -> Self {
        let aspect_ratio = self.text_aspect_ratio(font, glyph);

        let Some(transform) = self.transform.filter(|transform| !transform.is_identity()) else {
            return Self {
                gradient: self.gradient.clone(),
                transform: self.transform,
                aspect_ratio,
            };
        };

        if glyph_scale.0 == 0.0 {
            return Self {
                gradient: self.gradient.clone(),
                transform: Some(transform),
                aspect_ratio,
            };
        };

        let adjusted_x_offset = (pos.x.0 * 2.0).round();
        let adjusted_y_offset = (pos.y.0 * 2.0).round();
        let glyph_offset = Transform::from_translate(
            Scalar(-adjusted_x_offset / 2.0 * glyph_scale.0),
            Scalar(-adjusted_y_offset / 2.0 * glyph_scale.0),
        );
        let glyph_coord_scale =
            Transform::from_scale(Scalar(1.0 / glyph_scale.0), Scalar(1.0 / glyph_scale.0));
        let transform = transform
            .post_concat(Transform::from_scale(Scalar(1.0), Scalar(-1.0)))
            .post_concat(glyph_offset)
            .post_concat(glyph_coord_scale);

        Self {
            gradient: self.gradient.clone(),
            transform: Some(transform),
            aspect_ratio,
        }
    }

    fn text_aspect_ratio(&self, font: &FontItem, glyph: u32) -> Option<f32> {
        if !matches!(
            self.gradient.kind,
            GradientKind::Linear(_) | GradientKind::Conic(_)
        ) {
            return self.aspect_ratio;
        }

        self.transform
            .filter(|transform| !transform.is_identity())
            .and_then(valid_transform_aspect_ratio)
            .or_else(|| glyph_aspect_ratio(font, glyph))
            .or(self.aspect_ratio)
    }

    fn fill_conic_path(
        &self,
        canvas: &dyn CanvasDevice,
        ts: sk::Transform,
        clip_path: &Path2d,
        clip_bounds: Option<sk::Rect>,
        prefer_native: bool,
    ) {
        let GradientKind::Conic(angle) = self.gradient.kind else {
            return;
        };

        let transform = self.transform.unwrap_or_else(Transform::identity);
        let mut center = ir::Point::new(Scalar(0.5), Scalar(0.5));
        for style in &self.gradient.styles {
            if let GradientStyle::Center(c) = style {
                center = *c;
            }
        }

        if prefer_native {
            let paint_to_local = conic_paint_to_local(transform, self.aspect_ratio, center, ts);
            let canvas_transform = ts.pre_concat(paint_to_local);
            if is_invertible_sk_transform(canvas_transform) {
                if let Some(gradient) =
                    canvas.create_conic_gradient((std::f32::consts::PI + angle.0) as f64, 0.0, 0.0)
                {
                    append_color_stops(&gradient, &self.gradient);

                    let _guard = CanvasStateGuard::new(canvas);
                    if !set_invertible_transform(canvas, ts) {
                        return;
                    }
                    canvas.clip_with_path_2d(clip_path);

                    if !set_invertible_transform(canvas, canvas_transform) {
                        return;
                    }

                    let (x, y, w, h) = paint_space_fill_rect(clip_bounds, paint_to_local);
                    canvas.set_fill_style_canvas_gradient(&gradient);
                    canvas.fill_rect(x, y, w, h);
                    return;
                }
            }
        }

        let aspect_ratio = resolve_conic_aspect_ratio(transform, self.aspect_ratio, ts);
        let inverse_ratio = 1.0 / aspect_ratio;

        let (fill_x, fill_y, fill_w, fill_h) = paint_space_fill_rect(clip_bounds, transform.into());
        let max_dx = (fill_x - f64::from(center.x.0))
            .abs()
            .max((fill_x + fill_w - f64::from(center.x.0)).abs());
        let max_dy = (fill_y - f64::from(center.y.0))
            .abs()
            .max((fill_y + fill_h - f64::from(center.y.0)).abs());
        let radius_unit = max_dx.hypot(max_dy).max(2.0) as f32;

        let rx = radius_unit * transform.sx.0.hypot(transform.ky.0);
        let ry = radius_unit * transform.kx.0.hypot(transform.sy.0);
        if rx == 0.0 || ry == 0.0 {
            return;
        }

        let _guard = CanvasStateGuard::new(canvas);
        if !set_invertible_transform(canvas, ts) {
            return;
        }
        canvas.clip_with_path_2d(clip_path);

        let dtheta = TAU / CONIC_SEGMENT as f32;
        let sweep = if transform_det(transform) < 0.0 { 0 } else { 1 };
        for i in 0..CONIC_SEGMENT {
            let (theta1, theta2) = conic_segment_angles(angle.0, dtheta, inverse_ratio, i);
            let center_point = (center.x.0, center.y.0);
            let segment_start = conic_segment_point_with_radius(center_point, theta1, radius_unit);
            let segment_end = conic_segment_point_with_radius(center_point, theta2, radius_unit);
            let (cx, cy) = transform_point(transform, center.x.0 as f64, center.y.0 as f64);
            let (x1, y1) =
                transform_point(transform, segment_start.0 as f64, segment_start.1 as f64);
            let (x2, y2) = transform_point(transform, segment_end.0 as f64, segment_end.1 as f64);

            let segment_path = format!(
                "M {cx:.6} {cy:.6} L {x1:.6} {y1:.6} A {rx:.6} {ry:.6} 0 0 {sweep} {x2:.6} {y2:.6} Z"
            );
            let Ok(segment) = Path2d::new_with_path_string(&segment_path) else {
                continue;
            };

            let t1 = i as f32 / CONIC_SEGMENT as f32;
            let t2 = (i + 1) as f32 / CONIC_SEGMENT as f32;
            let gradient = canvas.create_linear_gradient(x1, y1, x2, y2);
            add_color_stop(
                &gradient,
                0.0,
                typst_color_to_css(sample_color_stops(&self.gradient, t1)),
            );
            add_color_stop(
                &gradient,
                1.0,
                typst_color_to_css(sample_color_stops(&self.gradient, t2)),
            );
            canvas.set_fill_style_canvas_gradient(&gradient);
            canvas.fill_with_path_2d(&segment);
        }
    }

    fn fill_radial_path(
        &self,
        canvas: &dyn CanvasDevice,
        ts: sk::Transform,
        clip_path: &Path2d,
        clip_bounds: Option<sk::Rect>,
    ) {
        let GradientKind::Radial(radius) = self.gradient.kind else {
            return;
        };

        let transform = self.transform.unwrap_or_else(Transform::identity);
        let mut center = ir::Point::new(Scalar(0.5), Scalar(0.5));
        let mut focal_center = ir::Point::new(Scalar(0.5), Scalar(0.5));
        let mut focal_radius = Scalar(0.0);

        for style in &self.gradient.styles {
            match style {
                GradientStyle::Center(c) => center = *c,
                GradientStyle::FocalCenter(c) => focal_center = *c,
                GradientStyle::FocalRadius(r) => focal_radius = *r,
            }
        }

        let Some(gradient) = canvas.create_radial_gradient(
            focal_center.x.0 as f64,
            focal_center.y.0 as f64,
            focal_radius.0 as f64,
            center.x.0 as f64,
            center.y.0 as f64,
            radius.0 as f64,
        ) else {
            return;
        };
        append_color_stops(&gradient, &self.gradient);

        let _guard = CanvasStateGuard::new(canvas);
        if !set_invertible_transform(canvas, ts) {
            return;
        }
        canvas.clip_with_path_2d(clip_path);

        let paint_transform: sk::Transform = transform.into();
        if !set_invertible_transform(canvas, ts.pre_concat(paint_transform)) {
            return;
        }

        let (x, y, w, h) = paint_space_fill_rect(clip_bounds, paint_transform);
        canvas.set_fill_style_canvas_gradient(&gradient);
        canvas.fill_rect(x, y, w, h);
    }
}

fn is_invertible_sk_transform(m: sk::Transform) -> bool {
    if !m.sx.is_finite()
        || !m.ky.is_finite()
        || !m.kx.is_finite()
        || !m.sy.is_finite()
        || !m.tx.is_finite()
        || !m.ty.is_finite()
    {
        return false;
    }
    let det = f64::from(m.sx) * f64::from(m.sy) - f64::from(m.kx) * f64::from(m.ky);
    det.is_finite() && det.abs() > 1e-12
}

fn set_invertible_transform(canvas: &dyn CanvasDevice, m: sk::Transform) -> bool {
    if !is_invertible_sk_transform(m) {
        return false;
    }
    canvas.set_transform(
        f64::from(m.sx),
        f64::from(m.ky),
        f64::from(m.kx),
        f64::from(m.sy),
        f64::from(m.tx),
        f64::from(m.ty),
    );
    true
}

fn is_orthogonal_sk_transform(m: sk::Transform) -> bool {
    let w = f64::from(m.sx).hypot(f64::from(m.ky));
    let h = f64::from(m.kx).hypot(f64::from(m.sy));
    if !w.is_finite() || !h.is_finite() || w <= 1e-6 || h <= 1e-6 {
        return false;
    }
    let dot = f64::from(m.sx) * f64::from(m.kx) + f64::from(m.ky) * f64::from(m.sy);
    dot.abs() <= 1e-3 * w * h
}

fn valid_sk_transform_aspect_ratio(m: sk::Transform) -> Option<f32> {
    let width = m.sx.hypot(m.ky);
    let height = m.kx.hypot(m.sy);
    if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 {
        let ratio = width / height;
        ratio.is_finite().then_some(ratio)
    } else {
        None
    }
}

fn resolve_conic_aspect_ratio(
    transform: Transform,
    aspect_ratio: Option<f32>,
    ts: sk::Transform,
) -> f32 {
    let paint_transform: sk::Transform = transform.into();
    if is_orthogonal_sk_transform(paint_transform) {
        return gradient_aspect_ratio(
            aspect_ratio.or_else(|| valid_transform_aspect_ratio(transform)),
        );
    }

    let screen_transform = ts.pre_concat(paint_transform);
    if is_orthogonal_sk_transform(screen_transform) {
        if let Some(ratio) = valid_sk_transform_aspect_ratio(screen_transform) {
            return gradient_aspect_ratio(Some(ratio));
        }
    }

    gradient_aspect_ratio(aspect_ratio.or_else(|| valid_transform_aspect_ratio(transform)))
}

/// Maps isotropic conic-gradient circle space `(u, v)` centered at `(0, 0)` into
/// the element's local coordinate space by first scaling `(1 / aspect_ratio, 1)`
/// around `center` in parent `[0, 1]^2` space and then applying `paint_transform`.
/// Composing `ts.pre_concat(conic_paint_to_local(...))` preserves parent aspect-ratio
/// correction while keeping any outer `#scale`, `#rotate`, or `#skew` intact.
pub(crate) fn conic_paint_to_local(
    transform: Transform,
    aspect_ratio: Option<f32>,
    center: ir::Point,
    ts: sk::Transform,
) -> sk::Transform {
    let ratio = resolve_conic_aspect_ratio(transform, aspect_ratio, ts);
    let paint_transform: sk::Transform = transform.into();
    let unit_circle_to_parent =
        sk::Transform::from_row(1.0 / ratio, 0.0, 0.0, 1.0, center.x.0, center.y.0);
    paint_transform.pre_concat(unit_circle_to_parent)
}

/// Computes a fill rectangle in paint space guaranteed to cover the clipped
/// element's local `clip_bounds` (even when the element overflows a narrow
/// parent box far past `[-4, 4]^2`).
pub(crate) fn paint_space_fill_rect(
    clip_bounds: Option<sk::Rect>,
    paint_to_local: sk::Transform,
) -> (f64, f64, f64, f64) {
    if let (Some(bounds), Some(inv)) = (clip_bounds, paint_to_local.invert()) {
        let mut corners = [
            tiny_skia_path::Point::from_xy(bounds.left(), bounds.top()),
            tiny_skia_path::Point::from_xy(bounds.right(), bounds.top()),
            tiny_skia_path::Point::from_xy(bounds.right(), bounds.bottom()),
            tiny_skia_path::Point::from_xy(bounds.left(), bounds.bottom()),
        ];
        inv.map_points(&mut corners);
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for pt in corners {
            let x = f64::from(pt.x);
            let y = f64::from(pt.y);
            if !x.is_finite() || !y.is_finite() {
                return (-4.0, -4.0, 8.0, 8.0);
            }
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        let pad_x = ((max_x - min_x) * 0.05).max(1.0);
        let pad_y = ((max_y - min_y) * 0.05).max(1.0);
        let lo_x = min_x - pad_x;
        let lo_y = min_y - pad_y;
        let w = (max_x + pad_x) - lo_x;
        let h = (max_y + pad_y) - lo_y;
        if lo_x.is_finite()
            && lo_y.is_finite()
            && w.is_finite()
            && h.is_finite()
            && w > 0.0
            && h > 0.0
        {
            return (lo_x, lo_y, w, h);
        }
    }
    (-4.0, -4.0, 8.0, 8.0)
}

fn resolve_gradient(module: &Module, paint: &str) -> Option<CanvasGradientPaint> {
    let id = paint.strip_prefix("@g")?;
    let mut id = Fingerprint::try_from_str(id).ok()?;

    let transform = match module.get_item(&id)? {
        VecItem::ColorTransform(transform) => {
            id = transform.item;
            Some(transform.transform)
        }
        _ => None,
    };

    match module.get_item(&id)? {
        VecItem::Gradient(gradient) => Some(CanvasGradientPaint {
            gradient: gradient.clone(),
            transform,
            aspect_ratio: None,
        }),
        _ => None,
    }
}

fn create_canvas_gradient(
    canvas: &dyn CanvasDevice,
    gradient: &GradientItem,
    transform: Option<Transform>,
    aspect_ratio: Option<f32>,
) -> Option<CanvasGradient> {
    let transform = transform.unwrap_or_else(Transform::identity);
    let canvas_gradient = match gradient.kind {
        GradientKind::Linear(angle) => {
            let (x1, y1, x2, y2) =
                linear_gradient_points(angle.0, gradient_aspect_ratio(aspect_ratio));
            let (x1, y1, x2, y2) = transform_linear_gradient(transform, x1, y1, x2, y2);
            canvas.create_linear_gradient(x1, y1, x2, y2)
        }
        GradientKind::Radial(radius) => {
            let mut center = ir::Point::new(Scalar(0.5), Scalar(0.5));
            let mut focal_center = ir::Point::new(Scalar(0.5), Scalar(0.5));
            let mut focal_radius = Scalar(0.0);

            for style in &gradient.styles {
                match style {
                    GradientStyle::Center(c) => center = *c,
                    GradientStyle::FocalCenter(c) => focal_center = *c,
                    GradientStyle::FocalRadius(r) => focal_radius = *r,
                }
            }

            let (fx, fy) =
                transform_point(transform, focal_center.x.0 as f64, focal_center.y.0 as f64);
            let (cx, cy) = transform_point(transform, center.x.0 as f64, center.y.0 as f64);
            let radius_scale = transform_radius_scale(transform);
            canvas.create_radial_gradient(
                fx,
                fy,
                focal_radius.0 as f64 * radius_scale,
                cx,
                cy,
                radius.0 as f64 * radius_scale,
            )?
        }
        GradientKind::Conic(angle) => {
            let mut center = ir::Point::new(Scalar(0.5), Scalar(0.5));
            for style in &gradient.styles {
                if let GradientStyle::Center(c) = style {
                    center = *c;
                }
            }

            let (cx, cy) = transform_point(transform, center.x.0 as f64, center.y.0 as f64);
            canvas.create_conic_gradient((std::f32::consts::PI + angle.0) as f64, cx, cy)?
        }
    };

    append_color_stops(&canvas_gradient, gradient);
    Some(canvas_gradient)
}

fn append_color_stops(canvas_gradient: &CanvasGradient, gradient: &GradientItem) {
    for window in gradient.stops.windows(2) {
        let (start_c, start_t) = window[0];
        let (end_c, end_t) = window[1];

        add_color_stop(canvas_gradient, start_t.0, rgba_to_css(start_c));

        let len = if gradient.anti_alias {
            (256 / gradient.stops.len() as u32).max(2)
        } else {
            2
        };

        for i in 1..(len - 1) {
            let t0 = i as f32 / (len - 1) as f32;
            let t = start_t.0 + (end_t.0 - start_t.0) * t0;
            add_color_stop(
                canvas_gradient,
                t,
                typst_color_to_css(sample_color_stops(gradient, t)),
            );
        }

        add_color_stop(canvas_gradient, end_t.0, rgba_to_css(end_c));
    }
}

fn add_color_stop(canvas_gradient: &CanvasGradient, offset: f32, color: String) {
    let _ = canvas_gradient.add_color_stop(offset.clamp(0.0, 1.0), &color);
}

fn sample_color_stops(gradient: &GradientItem, t: f32) -> TypstColor {
    let t = t.clamp(0.0, 1.0);
    let stops = &gradient.stops;
    let mut j = stops.partition_point(|(_, ratio)| ratio.0 < t);

    if j == 0 {
        while stops.get(j + 1).is_some_and(|(_, r)| r.0 == 0.0) {
            j += 1;
        }

        return rgba_to_typst(stops[j].0);
    }

    if j >= stops.len() {
        return rgba_to_typst(stops[stops.len() - 1].0);
    }

    let (col_0, pos_0) = stops[j - 1];
    let (col_1, pos_1) = stops[j];
    let t = (t - pos_0.0) / (pos_1.0 - pos_0.0);
    let col_0 = rgba_to_typst(col_0);
    let col_1 = rgba_to_typst(col_1);

    let Some(mixing_space) = color_space_to_typst(gradient.space) else {
        return col_0;
    };

    TypstColor::mix_iter(
        [
            WeightedColor::new(col_0.clone(), (1.0 - t) as f64),
            WeightedColor::new(col_1, t as f64),
        ],
        Smart::Custom(mixing_space),
    )
    .unwrap_or(col_0)
}

fn rgba_to_typst(color: Rgba8Item) -> TypstColor {
    TypstColor::from_u8(color.r, color.g, color.b, color.a)
}

fn color_space_to_typst(space: ColorSpace) -> Option<TypstColorSpace> {
    let process = match space {
        ColorSpace::Luma => return None,
        ColorSpace::Oklab => ProcessColorSpace::Oklab,
        ColorSpace::Srgb => ProcessColorSpace::Srgb,
        ColorSpace::D65Gray => ProcessColorSpace::D65Gray,
        ColorSpace::LinearRgb => ProcessColorSpace::LinearRgb,
        ColorSpace::Hsl => ProcessColorSpace::Hsl,
        ColorSpace::Hsv => ProcessColorSpace::Hsv,
        ColorSpace::Cmyk => ProcessColorSpace::Cmyk,
        ColorSpace::Oklch => ProcessColorSpace::Oklch,
    };
    Some(TypstColorSpace::Process(process))
}

fn typst_color_to_css(color: TypstColor) -> String {
    let (r, g, b, a) = color.to_rgb().into_format::<u8, u8>().into_components();
    rgba_to_css(Rgba8Item { r, g, b, a })
}

fn rgba_to_css(color: Rgba8Item) -> String {
    let Rgba8Item { r, g, b, a } = color;
    if a == 255 {
        let shorter = format!("#{r:02x}{g:02x}{b:02x}");
        if shorter.chars().nth(1) == shorter.chars().nth(2)
            && shorter.chars().nth(3) == shorter.chars().nth(4)
            && shorter.chars().nth(5) == shorter.chars().nth(6)
        {
            return format!(
                "#{}{}{}",
                shorter.chars().nth(1).unwrap(),
                shorter.chars().nth(3).unwrap(),
                shorter.chars().nth(5).unwrap()
            );
        }
        return shorter;
    }

    format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
}

fn transform_aspect_ratio(transform: Transform) -> f32 {
    valid_transform_aspect_ratio(transform).unwrap_or(1.0)
}

fn valid_transform_aspect_ratio(transform: Transform) -> Option<f32> {
    let width = transform.sx.0.hypot(transform.ky.0);
    let height = transform.kx.0.hypot(transform.sy.0);

    if width.is_finite() && height.is_finite() && height != 0.0 {
        Some(width / height)
    } else {
        None
    }
}

fn glyph_aspect_ratio(font: &FontItem, glyph: u32) -> Option<f32> {
    let (width, height) = match font.get_glyph(glyph)?.as_ref() {
        FlatGlyphItem::Outline(outline) => {
            let mut path = convert_path(&outline.d)?;
            if let Some(transform) = &outline.ts {
                let transform: tiny_skia_path::Transform = (**transform).into();
                path = path.transform(transform)?;
            }

            let bounds = path.bounds();
            (bounds.width(), bounds.height())
        }
        FlatGlyphItem::Image(image) => (image.image.size.x.0, image.image.size.y.0),
        FlatGlyphItem::None => return None,
    };

    if width.is_finite() && height.is_finite() && width != 0.0 && height != 0.0 {
        Some((width / height).abs())
    } else {
        None
    }
}

fn convert_path(path_data: &str) -> Option<tiny_skia_path::Path> {
    let mut builder = tiny_skia_path::PathBuilder::new();

    for segment in svgtypes::SimplifyingPathParser::from(path_data) {
        let segment = segment.ok()?;

        match segment {
            svgtypes::SimplePathSegment::MoveTo { x, y } => builder.move_to(x as f32, y as f32),
            svgtypes::SimplePathSegment::LineTo { x, y } => builder.line_to(x as f32, y as f32),
            svgtypes::SimplePathSegment::Quadratic { x1, y1, x, y } => {
                builder.quad_to(x1 as f32, y1 as f32, x as f32, y as f32)
            }
            svgtypes::SimplePathSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => builder.cubic_to(
                x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32,
            ),
            svgtypes::SimplePathSegment::ClosePath => builder.close(),
        }
    }

    builder.finish()
}

fn transform_point(transform: Transform, x: f64, y: f64) -> (f64, f64) {
    (
        f64::from(transform.sx.0) * x + f64::from(transform.kx.0) * y + f64::from(transform.tx.0),
        f64::from(transform.ky.0) * x + f64::from(transform.sy.0) * y + f64::from(transform.ty.0),
    )
}

fn transform_det(transform: Transform) -> f32 {
    transform.sx.0 * transform.sy.0 - transform.kx.0 * transform.ky.0
}

fn transform_linear_gradient(
    transform: Transform,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
) -> (f64, f64, f64, f64) {
    let (sx, ky, kx, sy) = (
        f64::from(transform.sx.0),
        f64::from(transform.ky.0),
        f64::from(transform.kx.0),
        f64::from(transform.sy.0),
    );
    let det = sx * sy - kx * ky;
    if det.abs() <= f64::EPSILON {
        let (x1, y1) = transform_point(transform, x1, y1);
        let (x2, y2) = transform_point(transform, x2, y2);
        return (x1, y1, x2, y2);
    }

    let vx = x2 - x1;
    let vy = y2 - y1;
    let ix = (sy * vx - ky * vy) / det;
    let iy = (-kx * vx + sx * vy) / det;
    let src_len2 = vx * vx + vy * vy;
    let dst_len2 = ix * ix + iy * iy;
    if src_len2 <= f64::EPSILON || dst_len2 <= f64::EPSILON {
        let (x1, y1) = transform_point(transform, x1, y1);
        let (x2, y2) = transform_point(transform, x2, y2);
        return (x1, y1, x2, y2);
    }

    let (x1, y1) = transform_point(transform, x1, y1);
    let scale = src_len2 / dst_len2;
    (x1, y1, x1 + ix * scale, y1 + iy * scale)
}

fn transform_radius_scale(transform: Transform) -> f64 {
    let x = transform.sx.0.hypot(transform.ky.0);
    let y = transform.kx.0.hypot(transform.sy.0);
    f64::from(x.max(y)).max(0.0)
}

fn can_use_native_circular_gradient(transform: Transform) -> bool {
    const EPS: f32 = 1e-4;

    transform.sx.0.is_finite()
        && transform.sy.0.is_finite()
        && transform.sx.0 > 0.0
        && transform.sy.0 > 0.0
        && transform.kx.0.abs() < EPS
        && transform.ky.0.abs() < EPS
        && (transform.sx.0 - transform.sy.0).abs() <= EPS * transform.sx.0.max(transform.sy.0)
}

fn gradient_aspect_ratio(aspect_ratio: Option<f32>) -> f32 {
    aspect_ratio
        .filter(|ratio| ratio.is_finite() && *ratio != 0.0)
        .unwrap_or(1.0)
}

fn correct_aspect_ratio(angle: f32, aspect_ratio: f32) -> f32 {
    (angle.sin() / aspect_ratio.abs())
        .atan2(angle.cos())
        .rem_euclid(TAU)
}

fn conic_segment_angles(angle: f32, dtheta: f32, inverse_ratio: f32, i: usize) -> (f32, f32) {
    (
        -correct_aspect_ratio(angle + dtheta * i as f32, inverse_ratio),
        -correct_aspect_ratio(angle + dtheta * (i + 1) as f32, inverse_ratio),
    )
}

fn conic_segment_point_with_radius(center: (f32, f32), theta: f32, radius: f32) -> (f32, f32) {
    (
        -radius * theta.cos() + center.0,
        radius * theta.sin() + center.1,
    )
}

fn linear_gradient_points(angle: f32, aspect_ratio: f32) -> (f64, f64, f64, f64) {
    let angle = correct_aspect_ratio(angle, aspect_ratio);
    let sin = angle.sin();
    let cos = angle.cos();
    let length = sin.abs() + cos.abs();
    let (x1, y1) = match angle {
        angle if angle < std::f32::consts::FRAC_PI_2 => (0.0, 0.0),
        angle if angle < std::f32::consts::PI => (1.0, 0.0),
        angle if angle < std::f32::consts::PI * 1.5 => (1.0, 1.0),
        _ => (0.0, 1.0),
    };

    (
        x1,
        y1,
        x1 + f64::from(cos * length),
        y1 + f64::from(sin * length),
    )
}
