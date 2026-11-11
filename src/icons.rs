//! Small vector-drawn icons (no external assets), matching the edge-chevron
//! style. Every icon is painted from primitives into an arbitrary rect.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Prev,
    Next,
    PrevFile,
    NextFile,
    RotateL,
    RotateR,
    Crop,
    Undo,
    Open,
    Save,
    Check,
}

/// Map normalized coords [-1..1] (y down) into `rect`.
struct Glyph {
    c_x: f32,
    c_y: f32,
    s: f32,
}

impl Glyph {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(self.c_x + x * self.s, self.c_y + y * self.s)
    }
}

/// Paint `icon` nicely inside `rect`.
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let g = Glyph {
        c_x: rect.center().x,
        c_y: rect.center().y,
        s: rect.width().min(rect.height()) * 0.40,
    };
    let w = rect.height() * 0.085;
    let stroke = Stroke::new(w, color);

    match icon {
        Icon::Prev | Icon::Next => chevron(p, &g, stroke, if icon == Icon::Prev { -1.0 } else { 1.0 }, 0.0),
        Icon::PrevFile | Icon::NextFile => {
            let dir = if icon == Icon::PrevFile { -1.0 } else { 1.0 };
            chevron(p, &g, stroke, dir, -0.35 * dir);
            chevron(p, &g, stroke, dir, 0.75 * dir);
        }
        Icon::RotateL | Icon::RotateR => {
            let dir = if icon == Icon::RotateL { -1.0 } else { 1.0 };
            // open circular arc with an arrow tip at the end
            let a0 = if dir < 0.0 { std::f32::consts::PI * 0.28 } else { std::f32::consts::PI * 1.72 };
            let a1 = a0 - dir * std::f32::consts::PI * 1.35;
            let n = 24;
            let pt1 = g.pt(0.55 * a1.cos(), 0.55 * a1.sin());
            let pts: Vec<Pos2> = (0..=n)
                .map(|i| {
                    let a = a0 + (a1 - a0) * (i as f32 / n as f32);
                    g.pt(0.55 * a.cos(), 0.55 * a.sin())
                })
                .collect();
            p.add(Shape::line(pts, stroke));
            // Arrowhead: tip pushed forward along the tangent, base spread
            // along the *radial* direction (perpendicular to the tangent).
            // Using the tangent for the base too makes the triangle
            // degenerate (zero area), which tessellates into a huge sliver.
            let t_ang = a1 - dir * std::f32::consts::FRAC_PI_2;
            let tip = Pos2::new(pt1.x + t_ang.cos() * 0.30 * g.s, pt1.y + t_ang.sin() * 0.30 * g.s);
            let b0 = Pos2::new(pt1.x + a1.cos() * 0.17 * g.s, pt1.y + a1.sin() * 0.17 * g.s);
            let b1 = Pos2::new(pt1.x - a1.cos() * 0.17 * g.s, pt1.y - a1.sin() * 0.17 * g.s);
            p.add(Shape::convex_polygon(vec![tip, b0, b1], color, Stroke::NONE));
        }
        Icon::Crop => {
            // two crossing L-brackets, the classic crop glyph
            p.add(Shape::line(
                vec![g.pt(-0.35, -0.85), g.pt(-0.35, 0.35), g.pt(0.85, 0.35)],
                stroke,
            ));
            p.add(Shape::line(
                vec![g.pt(0.35, 0.85), g.pt(0.35, -0.35), g.pt(-0.85, -0.35)],
                stroke,
            ));
        }
        Icon::Undo => {
            // curved arrow sweeping back over the top, head pointing left
            let n = 22;
            let a0 = std::f32::consts::PI * 0.05;
            let a1 = std::f32::consts::PI * 0.95;
            let pts: Vec<Pos2> = (0..=n)
                .map(|i| {
                    let a = a0 + (a1 - a0) * (i as f32 / n as f32);
                    let wob = 1.0 - 0.28 * (a - std::f32::consts::FRAC_PI_2).cos().abs();
                    g.pt(0.62 * a.cos(), -1.55 * 0.55 * wob * a.sin())
                })
                .collect();
            p.add(Shape::line(pts, stroke));
            p.add(Shape::convex_polygon(
                vec![g.pt(-0.78, -0.28), g.pt(-0.30, -0.62), g.pt(-0.30, -0.10)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Open => {
            let mut pts = vec![
                g.pt(-0.75, -0.35),
                g.pt(-0.75, 0.5),
                g.pt(0.75, 0.5),
                g.pt(0.75, -0.1),
                g.pt(-0.05, -0.1),
                g.pt(-0.25, -0.35),
            ];
            pts.push(pts[0]);
            p.add(Shape::line(pts, stroke));
        }
        Icon::Save => {
            p.add(Shape::line(
                vec![
                    g.pt(-0.55, -0.55),
                    g.pt(0.55, -0.55),
                    g.pt(0.55, 0.55),
                    g.pt(-0.55, 0.55),
                    g.pt(-0.55, -0.55),
                ],
                stroke,
            ));
            p.add(Shape::line(
                vec![
                    g.pt(-0.05, -0.55),
                    g.pt(-0.05, -0.2),
                    g.pt(0.35, -0.2),
                    g.pt(0.35, -0.55),
                ],
                stroke,
            ));
            p.rect_filled(
                Rect::from_min_max(g.pt(-0.32, 0.08), g.pt(0.32, 0.55)),
                1.0,
                color,
            );
        }
        Icon::Check => {
            p.add(Shape::line(
                vec![g.pt(-0.5, 0.05), g.pt(-0.1, 0.5), g.pt(0.65, -0.55)],
                Stroke::new(w * 1.2, color),
            ));
        }
    }
}

fn chevron(p: &Painter, g: &Glyph, stroke: Stroke, dir: f32, x_off: f32) {
    let color = stroke.color;
    let pts = vec![
        g.pt(x_off - dir * 0.30, -0.62),
        g.pt(x_off + dir * 0.30, 0.0),
        g.pt(x_off - dir * 0.30, 0.62),
    ];
    p.add(Shape::line(pts.clone(), stroke));
    for pt in pts {
        p.add(Shape::circle_filled(pt, stroke.width * 0.5, color));
    }
}
