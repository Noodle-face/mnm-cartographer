//! Marker icons: one badge per kind.
//!
//! Every icon is the same round badge in the kind's colour, with a white
//! glyph drawn inside the same inner square, so all kinds are exactly the
//! same size on the map and differ only in what they show. Glyphs are drawn
//! from primitives in a unit square ([-1, 1], y down), scaled to the badge;
//! nothing is rasterised, so they stay crisp at any size. Concave outlines
//! are built from convex pieces, which is all egui fills correctly.

use egui::{pos2, Color32, Pos2, Shape, Stroke};

/// Radius of a badge on the map, in points.
pub const RADIUS: f32 = 11.0;

const OUTLINE: Color32 = Color32::from_rgb(0x15, 0x12, 0x0e);
const GLYPH: Color32 = Color32::from_rgb(0xfb, 0xf7, 0xee);

/// Draw the badge for `kind` (a marker kind key) centred at `c`.
pub fn draw(p: &egui::Painter, kind: &str, color: Color32, c: Pos2, r: f32) {
    // A soft shadow lifts the badge off busy terrain.
    p.circle_filled(c + egui::vec2(r * 0.12, r * 0.18), r, Color32::from_black_alpha(70));
    p.circle(c, r, color, Stroke::new((r * 0.16).max(1.2), OUTLINE));
    let u = r * 0.58; // glyph half-size: the same box for every kind
    let at = |x: f32, y: f32| pos2(c.x + x * u, c.y + y * u);
    let poly = |pts: &[(f32, f32)], fill: Color32| {
        p.add(Shape::convex_polygon(pts.iter().map(|&(x, y)| at(x, y)).collect(), fill, Stroke::NONE));
    };
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32, fill: Color32| {
        poly(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1)], fill);
    };
    let line = |a: (f32, f32), b: (f32, f32), w: f32, col: Color32| {
        p.line_segment([at(a.0, a.1), at(b.0, b.1)], Stroke::new(w * u, col));
    };
    match kind {
        // Tent, with the door cut in the badge colour.
        "camp" => {
            poly(&[(-0.95, 0.75), (0.0, -0.85), (0.95, 0.75)], GLYPH);
            poly(&[(-0.22, 0.75), (0.0, 0.12), (0.22, 0.75)], color);
        }
        // Crown: a band and three points.
        "named" => {
            rect(-0.85, 0.15, 0.85, 0.6, GLYPH);
            poly(&[(-0.85, 0.2), (-0.85, -0.55), (-0.35, 0.2)], GLYPH);
            poly(&[(-0.4, 0.2), (0.0, -0.8), (0.4, 0.2)], GLYPH);
            poly(&[(0.35, 0.2), (0.85, -0.55), (0.85, 0.2)], GLYPH);
        }
        // Leaf on a diagonal, with its vein.
        "harvest" => {
            let pts: Vec<(f32, f32)> = (0..16).map(|k| {
                let a = std::f32::consts::TAU * k as f32 / 16.0;
                let (x, y) = (0.9 * a.cos(), 0.45 * a.sin());
                let s = std::f32::consts::FRAC_1_SQRT_2;
                ((x - y) * s, (-x - y) * s)
            }).collect();
            poly(&pts, GLYPH);
            line((-0.75, 0.75), (0.45, -0.45), 0.14, color);
        }
        // Coin.
        "merchant" => {
            p.circle_filled(c, 0.85 * u, GLYPH);
            p.circle_stroke(c, 0.55 * u, Stroke::new(0.13 * u, color));
            line((0.0, -0.32), (0.0, 0.32), 0.16, color);
        }
        // Arrow pointing out.
        "exit" => {
            rect(-0.85, -0.2, 0.15, 0.2, GLYPH);
            poly(&[(0.05, -0.7), (0.9, 0.0), (0.05, 0.7)], GLYPH);
        }
        // Exclamation mark.
        "quest" => {
            poly(&[(-0.2, -0.85), (0.2, -0.85), (0.13, 0.3), (-0.13, 0.3)], GLYPH);
            p.circle_filled(at(0.0, 0.65), 0.2 * u, GLYPH);
        }
        // Skull.
        "danger" => {
            p.circle_filled(at(0.0, -0.18), 0.68 * u, GLYPH);
            rect(-0.38, 0.2, 0.38, 0.78, GLYPH);
            p.circle_filled(at(-0.27, -0.15), 0.19 * u, color);
            p.circle_filled(at(0.27, -0.15), 0.19 * u, color);
            for x in [-0.13, 0.13] {
                line((x, 0.45), (x, 0.78), 0.1, color);
            }
        }
        // A page with lines of writing; also the fallback.
        _ => {
            rect(-0.62, -0.85, 0.62, 0.85, GLYPH);
            for y in [-0.42, 0.0, 0.42] {
                line((-0.38, y), (0.38, y), 0.13, color);
            }
        }
    }
}
