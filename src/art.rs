//! Full-colour images drawn in code: snowballs and the snowman.
//! Each returns a premultiplied RGBA pixmap plus its anchor point (the
//! pixel that goes at the object's position).

use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, Paint, PathBuilder, Pixmap, Point, RadialGradient,
    SpreadMode, Stroke, Transform,
};

const PAD: f32 = 2.0;

/// Fill a shaded snowball of radius `r` centred at (`cx`, `cy`).
fn ball(pm: &mut Pixmap, cx: f32, cy: f32, r: f32) -> Option<()> {
    let shader = RadialGradient::new(
        Point::from_xy(cx - r * 0.35, cy - r * 0.4),
        0.0,
        Point::from_xy(cx, cy),
        r * 1.15,
        vec![
            GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 255)),
            GradientStop::new(0.55, Color::from_rgba8(236, 242, 252, 255)),
            GradientStop::new(1.0, Color::from_rgba8(170, 186, 214, 255)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )?;
    let circle = PathBuilder::from_circle(cx, cy, r)?;
    let paint = Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    };
    pm.fill_path(
        &circle,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut edge = Paint::default();
    edge.set_color_rgba8(120, 138, 170, 110);
    let stroke = Stroke {
        width: (r * 0.06).clamp(0.7, 1.5),
        ..Stroke::default()
    };
    pm.stroke_path(&circle, &edge, &stroke, Transform::identity(), None);
    Some(())
}

/// A snowball of radius `r`; anchor = centre.
pub fn snowball(r: f32) -> Option<(Pixmap, (i32, i32))> {
    let size = (2.0 * (r + PAD)).ceil() as u32;
    let mut pm = Pixmap::new(size, size)?;
    let c = size as f32 / 2.0;
    ball(&mut pm, c, c, r)?;
    Some((pm, (c as i32, c as i32)))
}

fn fill(pm: &mut Pixmap, path: &tiny_skia::Path, rgba: [u8; 4]) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
    pm.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn line(pm: &mut Pixmap, pts: &[(f32, f32)], width: f32, rgba: [u8; 4]) -> Option<()> {
    let mut pb = PathBuilder::new();
    let (first, rest) = pts.split_first()?;
    pb.move_to(first.0, first.1);
    for p in rest {
        pb.line_to(p.0, p.1);
    }
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
    let stroke = Stroke {
        width,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    pm.stroke_path(&pb.finish()?, &paint, &stroke, Transform::identity(), None);
    Some(())
}

const COAL: [u8; 4] = [35, 35, 40, 255];
const CARROT: [u8; 4] = [240, 130, 30, 255];
const SCARF: [u8; 4] = [200, 35, 45, 255];
const STICK: [u8; 4] = [110, 72, 40, 255];

/// A snowman from three ball radii (bottom, middle, head), with coal eyes
/// and buttons, a carrot nose, a red scarf and stick arms.
/// Anchor = bottom centre (where it stands).
pub fn snowman([rb, rm, rt]: [f32; 3]) -> Option<(Pixmap, (i32, i32))> {
    let arm = rm * 1.3;
    let w = (2.0 * rb).max(2.0 * (rm + arm)) + 2.0 * PAD;
    // Each ball sits a little into the one below.
    let (yb, ym, yt) = (rb, 2.0 * rb + rm * 0.85, 2.0 * rb + 1.7 * rm + rt * 0.85);
    let h = yt + rt + PAD * 2.0;
    let mut pm = Pixmap::new(w.ceil() as u32, h.ceil() as u32)?;
    let cx = w / 2.0;
    let base = h - PAD;
    let y = |up: f32| base - up;

    // Arms behind the body: up and out from the middle ball.
    let (ay, aw) = (y(ym + rm * 0.2), (rm * 0.09).max(1.2));
    for dir in [-1.0f32, 1.0] {
        let x0 = cx + dir * rm * 0.7;
        let x1 = cx + dir * (rm + arm * 0.85);
        line(&mut pm, &[(x0, ay), (x1, ay - arm * 0.55)], aw, STICK)?;
        // A little twig.
        let (tx, ty) = (cx + dir * (rm + arm * 0.5), ay - arm * 0.33);
        line(
            &mut pm,
            &[(tx, ty), (tx + dir * arm * 0.12, ty - arm * 0.3)],
            aw * 0.8,
            STICK,
        )?;
    }
    ball(&mut pm, cx, y(yb), rb)?;
    ball(&mut pm, cx, y(ym), rm)?;
    // Buttons.
    for k in [-0.45f32, 0.0, 0.45] {
        let path = PathBuilder::from_circle(cx, y(ym) + k * rm, (rm * 0.09).max(1.0))?;
        fill(&mut pm, &path, COAL);
    }
    ball(&mut pm, cx, y(yt), rt)?;
    // Scarf: a band at the neck and a tail hanging down one side.
    let neck = y(yt) + rt * 0.85;
    let band = (rt * 0.32).max(2.0);
    let mut pb = PathBuilder::new();
    pb.push_rect(tiny_skia::Rect::from_xywh(
        cx - rt * 0.95,
        neck - band / 2.0,
        rt * 1.9,
        band,
    )?);
    pb.push_rect(tiny_skia::Rect::from_xywh(
        cx + rt * 0.25,
        neck,
        band * 0.9,
        rm * 0.7,
    )?);
    fill(&mut pm, &pb.finish()?, SCARF);
    // Face: coal eyes and a carrot nose pointing right.
    let eye = (rt * 0.11).max(1.0);
    for dx in [-0.33f32, 0.33] {
        let path = PathBuilder::from_circle(cx + dx * rt, y(yt) - rt * 0.2, eye)?;
        fill(&mut pm, &path, COAL);
    }
    let mut pb = PathBuilder::new();
    let ny = y(yt) + rt * 0.08;
    pb.move_to(cx, ny - rt * 0.12);
    pb.line_to(cx + rt * 0.75, ny + rt * 0.02);
    pb.line_to(cx, ny + rt * 0.14);
    pb.close();
    fill(&mut pm, &pb.finish()?, CARROT);
    Some((pm, (cx.round() as i32, base.round() as i32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque(pm: &Pixmap) -> usize {
        pm.data().chunks(4).filter(|p| p[3] > 200).count()
    }

    #[test]
    fn snowball_is_a_disc() {
        let (pm, anchor) = snowball(10.0).unwrap();
        assert_eq!(anchor, (12, 12));
        let area = std::f32::consts::PI * 100.0;
        let n = opaque(&pm) as f32;
        assert!((n - area).abs() < area * 0.15, "{n} vs {area}");
    }

    #[test]
    fn snowman_has_colours() {
        let (pm, (ax, ay)) = snowman([20.0, 14.0, 10.0]).unwrap();
        assert!(
            (ay - (pm.height() as i32 - 2)).abs() <= 1,
            "anchor at the bottom"
        );
        assert!((ax - pm.width() as i32 / 2).abs() <= 1);
        let has = |rgb: [u8; 3]| {
            pm.pixels()
                .iter()
                .any(|p| p.alpha() == 255 && [p.red(), p.green(), p.blue()] == rgb)
        };
        assert!(has([200, 35, 45]), "red scarf");
        assert!(has([240, 130, 30]), "carrot");
        assert!(has([35, 35, 40]), "coal");
    }
}

/// Writes the art to `$FLURRY_ART_OUT/*.png` for a look:
/// `FLURRY_ART_OUT=/tmp/art cargo test -- --ignored art_to_png`
#[cfg(test)]
#[test]
#[ignore]
fn art_to_png() {
    let Some(dir) = std::env::var_os("FLURRY_ART_OUT") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    // On a dark background, enlarged, like it would sit on a dark panel.
    for (name, (img, _)) in [
        ("snowball", snowball(16.0).unwrap()),
        ("snowman", snowman([30.0, 21.0, 14.0]).unwrap()),
    ] {
        let mut bg = Pixmap::new(img.width() + 20, img.height() + 20).unwrap();
        bg.fill(Color::from_rgba8(40, 52, 84, 255));
        bg.draw_pixmap(
            10,
            10,
            img.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        let mut big = Pixmap::new(bg.width() * 3, bg.height() * 3).unwrap();
        big.draw_pixmap(
            0,
            0,
            bg.as_ref(),
            &tiny_skia::PixmapPaint {
                quality: tiny_skia::FilterQuality::Nearest,
                ..Default::default()
            },
            Transform::from_scale(3.0, 3.0),
            None,
        );
        big.save_png(dir.join(format!("{name}.png"))).unwrap();
    }
}
