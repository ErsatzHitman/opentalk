//! Small vector glyphs shared by the pill and the tray popup.
use super::raster::{Canvas, Rect};
use super::theme::Rgba;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Stroke, Transform};

/// Nucleo `Redo2Outline18` arrow at `rect`, stroke 1.5 in 18-units.
pub fn draw_retry_icon(cv: &mut Canvas, rect: Rect, scale: f32, color: Rgba) {
    let (w, h) = (cv.w as u32, cv.h as u32);
    let Some(mut pm) = tiny_skia::PixmapMut::from_bytes(&mut *cv.data, w, h) else { return };
    let k = rect.w() / 18.0;
    let tf = Transform::from_scale(k * scale, k * scale).post_translate(rect.l * scale, rect.t * scale);
    let mut pb = PathBuilder::new();
    pb.move_to(15.0002, 12.0949);
    pb.cubic_to(13.8781, 14.2659, 11.6124, 15.75, 9.0, 15.75);
    pb.cubic_to(5.272, 15.75, 2.25, 12.7279, 2.25, 9.0);
    pb.cubic_to(2.25, 5.2721, 5.272, 2.25, 9.0, 2.25);
    pb.cubic_to(10.864, 2.25, 12.5515, 3.0055, 13.773, 4.227);
    let Some(arc) = pb.finish() else { return };
    let mut head = PathBuilder::new();
    head.move_to(11.614, 5.6341);
    head.line_to(15.5736, 6.2422);
    head.line_to(15.0759, 2.4461);
    head.close();
    let Some(head) = head.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color(color.to_color());
    paint.anti_alias = true;
    let stroke = Stroke { width: 1.5, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
    pm.stroke_path(&arc, &paint, &stroke, tf, None);
    pm.fill_path(&head, &paint, FillRule::Winding, tf, None);
    pm.stroke_path(&head, &paint, &stroke, tf, None);
}
