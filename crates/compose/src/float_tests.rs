//! 32-bit float documents keep scene-linear values outside 0..1 through adjustments
//! (`adjust::apply_opts` unclipped); integer depths still clip, unchanged.

use super::*;
use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::adjust::{CurvePoint, LevelsChannel, ToneSpace};
use photocraft_doc::{Adjustment, Document, Layer, LayerContent};
use photocraft_geom::Size;

const F32: SampleType = SampleType::F32;

fn one(rgb: [f32; 3]) -> Buffer {
    Buffer::filled(Rect::new(0, 0, 1, 1), [rgb[0], rgb[1], rgb[2], 1.0])
}

/// `adj` on one pixel as a document of `depth` (linear transfer for F32, sRGB otherwise).
fn run(adj: &Adjustment, rgb: [f32; 3], depth: SampleType) -> [f32; 3] {
    let mut b = one(rgb);
    adjust::apply_doc(adj, &mut b, adjust::Transfer::for_document(ColorMode::Rgb, depth), depth);
    [b.px[0][0], b.px[0][1], b.px[0][2]]
}

fn near(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
    (0..3).all(|k| (a[k] - b[k]).abs() <= tol)
}

fn pts(v: &[(f32, f32)]) -> Vec<CurvePoint> {
    v.iter().map(|&(input, output)| CurvePoint { input, output }).collect()
}

fn curves(master: Vec<CurvePoint>) -> Adjustment {
    let id = || pts(&[(0.0, 0.0), (1.0, 1.0)]);
    Adjustment::Curves { master, per_channel: [id(), id(), id()], space: ToneSpace::Rgb, black: Vec::new() }
}

fn levels(master: &LevelsChannel) -> Adjustment {
    Adjustment::Levels { master: master.clone(), per_channel: Default::default(), space: ToneSpace::Rgb, black: LevelsChannel::default() }
}

/// An `n`³ table of `f` over the grid coordinates, red fastest.
fn table(n: usize, f: impl Fn([f32; 3]) -> [f32; 3]) -> Vec<f32> {
    let m = (n - 1) as f32;
    (0..n * n * n).flat_map(|i| f([(i % n) as f32 / m, ((i / n) % n) as f32 / m, (i / (n * n)) as f32 / m])).collect()
}

fn lookup(n: usize, f: impl Fn([f32; 3]) -> [f32; 3], domain: Option<[[f32; 3]; 2]>) -> Adjustment {
    Adjustment::ColorLookup { name: "t".into(), lut: Some(std::sync::Arc::new(table(n, f))), size: n as u32, tetrahedral: false, dither: false, domain }
}

#[test]
fn exposure_keeps_values_above_one_and_below_zero() {
    let plus_one = Adjustment::Exposure { exposure: 1.0, offset: 0.0, gamma: 1.0 };
    assert!(near(run(&plus_one, [2.0, 0.6, -0.1], F32), [4.0, 1.2, -0.2], 1e-5));
    // A negative offset pushes values below zero instead of clipping them; gamma mirrors.
    let off = Adjustment::Exposure { exposure: 0.0, offset: -0.25, gamma: 2.0 };
    let o = run(&off, [1.25, 0.0, 4.25], F32);
    assert!(near(o, [1.0, -0.5, 2.0], 1e-5), "{o:?}");
    // Integer depths still clip.
    let o = run(&plus_one, [0.9, 0.9, 0.9], SampleType::U8);
    assert!(o.iter().all(|v| *v <= 1.0), "{o:?}");
}

#[test]
fn curves_extend_with_their_end_tangent() {
    let id = curves(pts(&[(0.0, 0.0), (1.0, 1.0)]));
    assert!(near(run(&id, [3.0, -0.2, 0.5], F32), [3.0, -0.2, 0.5], 1e-4));
    // A straight darkening line keeps darkening linearly above 1.
    let half = curves(pts(&[(0.0, 0.0), (1.0, 0.5)]));
    assert!(near(run(&half, [2.0, 4.0, 1.0], F32), [1.0, 2.0, 0.5], 1e-3));
    // A white point below 1 is flat beyond it, so it still clips there (as in Photoshop).
    let white = curves(pts(&[(0.0, 0.0), (0.8, 1.0)]));
    assert!(near(run(&white, [2.0, 0.9, 5.0], F32), [1.0, 1.0, 1.0], 1e-4));
    // Integer documents clip.
    assert!(near(run(&id, [3.0, -0.2, 0.5], SampleType::U16), [1.0, 0.0, 0.5], 1e-4));
}

#[test]
fn levels_stretch_past_the_white_point() {
    let lc = LevelsChannel { in_black: 0.0, in_white: 0.5, gamma: 1.0, out_black: 0.0, out_white: 1.0 };
    assert!(near(run(&levels(&lc), [1.0, 2.0, 0.25], F32), [2.0, 4.0, 0.5], 1e-3));
    assert!(near(run(&levels(&lc), [1.0, 2.0, 0.25], SampleType::U8), [1.0, 1.0, 0.5], 2e-3));
    // With a midtone gamma the curve continues with slope 1/gamma above white.
    let g = LevelsChannel { in_black: 0.0, in_white: 1.0, gamma: 2.0, out_black: 0.0, out_white: 1.0 };
    let o = run(&levels(&g), [3.0, 1.0, 0.25], F32);
    assert!((o[0] - (o[1] + 1.0)).abs() < 1e-2 && (o[1] - 1.0).abs() < 1e-2, "{o:?}");
    assert!((o[2] - adjust::levels(&g, 0.25)).abs() < 1e-3, "{o:?}");
    // Inside the input range the unclipped Levels is the clipped one.
    for v in [0.0, 0.1, 0.37, 0.5, 0.93, 1.0] {
        for ch in [lc.clone(), g.clone(), LevelsChannel { in_black: 0.2, in_white: 0.9, gamma: 0.7, out_black: 0.1, out_white: 0.8 }] {
            if v >= ch.in_black && v <= ch.in_white {
                assert!((adjust::levels_unclipped(&ch, v) - adjust::levels(&ch, v)).abs() < 1e-6, "{ch:?} {v}");
            }
        }
    }
}

#[test]
fn color_lookup_clamps_the_coordinate_and_passes_the_excess() {
    let id = lookup(9, |c| c, None);
    assert!(near(run(&id, [2.0, 0.5, -0.25], F32), [2.0, 0.5, -0.25], 1e-5));
    // A darkening look: the in-range part goes through the table, the excess above 1 is kept.
    let dark = lookup(9, |c| c.map(|v| v * 0.5), None);
    assert!(near(run(&dark, [1.5, 0.5, 0.0], F32), [1.0, 0.25, 0.0], 1e-5));
    // Integer documents: sampled at the clamped input, nothing passed through.
    assert!(near(run(&dark, [1.5, 0.5, 0.0], SampleType::U8), [0.5, 0.25, 0.0], 1e-5));
}

#[test]
fn color_lookup_honours_the_cube_domain() {
    // A 0..2 domain: grid coordinate = x / 2. The table maps its grid identically, so the
    // input comes out halved.
    let two = lookup(5, |c| c, Some([[0.0; 3], [2.0; 3]]));
    assert!(near(run(&two, [1.0, 2.0, 0.5], F32), [0.5, 1.0, 0.25], 1e-5));
    assert!(near(run(&two, [1.0, 0.5, 0.0], SampleType::U8), [0.5, 0.25, 0.0], 1e-5));
    // A table holding its domain's input values (an identity over -0.5..1.5) is the identity
    // across the whole domain, below 0 and above 1 included.
    let wide = lookup(9, |c| c.map(|v| v * 2.0 - 0.5), Some([[-0.5; 3], [1.5; 3]]));
    assert!(near(run(&wide, [-0.4, 1.25, 0.3], F32), [-0.4, 1.25, 0.3], 1e-5));
    // Beyond the domain the excess passes through (float) or clips (integer).
    assert!(near(run(&wide, [2.5, -1.0, 0.0], F32), [2.5, -1.0, 0.0], 1e-5));
    assert!(near(run(&wide, [1.0, 0.0, 0.0], SampleType::U8), [1.0, 0.0, 0.0], 1e-5));
    // A degenerate (hand-edited) domain doesn't divide by zero.
    let bad = lookup(3, |c| c, Some([[0.5; 3], [0.5; 3]]));
    assert!(run(&bad, [0.7, 0.2, 0.9], F32).iter().all(|v| v.is_finite()));
}

#[test]
fn brightness_contrast_and_color_balance_unclipped() {
    let legacy = Adjustment::BrightnessContrast { brightness: 25.5, contrast: 0.0, legacy: true };
    assert!(near(run(&legacy, [1.5, 0.2, -0.3], F32), [1.6, 0.3, -0.2], 1e-5));
    assert!(near(run(&legacy, [0.95, 0.2, 0.0], SampleType::U8), [1.0, 0.3, 0.1], 1e-5));
    // Modern curves: their 0..1 result plus the excess (slope 1 outside).
    let modern = Adjustment::BrightnessContrast { brightness: 40.0, contrast: 30.0, legacy: false };
    let inside = run(&modern, [1.0, 0.0, 0.5], F32);
    let o = run(&modern, [2.5, -0.5, 0.5], F32);
    assert!(near(o, [inside[0] + 1.5, inside[1] - 0.5, inside[2]], 1e-5), "{o:?} {inside:?}");
    let cb = Adjustment::ColorBalance { shadows: [0.0; 3], midtones: [0.0; 3], highlights: [40.0, 0.0, 0.0], preserve_luminosity: false };
    let o = run(&cb, [3.0, 3.0, 3.0], F32);
    assert!(o[0] > 3.0 && (o[1] - 3.0).abs() < 1e-5, "{o:?}");
    assert!(run(&cb, [1.0, 1.0, 1.0], SampleType::U8)[0] <= 1.0);
}

/// Integer depths render exactly as before: `apply_doc` is `apply_depth` with the depth's quantum.
#[test]
fn integer_depths_unchanged() {
    let adjs = [
        Adjustment::Exposure { exposure: 0.7, offset: 0.02, gamma: 1.2 },
        levels(&LevelsChannel { in_black: 0.1, in_white: 0.7, gamma: 1.3, out_black: 0.05, out_white: 0.95 }),
        curves(pts(&[(0.0, 0.1), (0.4, 0.6), (1.0, 0.9)])),
        lookup(5, |c| [c[0] * c[0], c[1].sqrt(), 1.0 - c[2]], None),
        Adjustment::BrightnessContrast { brightness: 30.0, contrast: 40.0, legacy: false },
        Adjustment::BrightnessContrast { brightness: -20.0, contrast: -30.0, legacy: true },
        Adjustment::ColorBalance { shadows: [20.0, -10.0, 5.0], midtones: [-15.0, 10.0, 30.0], highlights: [0.0, 5.0, -20.0], preserve_luminosity: true },
    ];
    for depth in [SampleType::U8, SampleType::U16] {
        for adj in &adjs {
            for rgb in [[0.0, 0.5, 1.0], [0.2, 0.9, 0.33], [0.7, 0.1, 0.05]] {
                let t = adjust::Transfer::for_document(ColorMode::Rgb, depth);
                let mut a = one(rgb);
                adjust::apply_depth(adj, &mut a, t, adjustment_quantum(depth));
                let mut b = one(rgb);
                adjust::apply_doc(adj, &mut b, t, depth);
                assert_eq!(a.px, b.px, "{depth:?} {adj:?}");
            }
        }
    }
}

/// Through the compositor: a float layer above 1 keeps its value through Normal blending,
/// an Exposure layer and a pass-through group with a clipped layer.
#[test]
fn float_document_renders_values_above_one() {
    let mut d = Document::new("f", Size::new(2, 2), ColorMode::Rgb, F32);
    let mut l = Layer::raster("hot", PixelFormat::RGBA32F);
    l.surface_mut().unwrap().fill_rect(Rect::new(0, 0, 2, 2), &[2.0, 0.5, 1.5, 1.0]);
    d.layers = vec![l.clone()];
    let p = render(&d, d.bounds()).px[0];
    assert!((p[0] - 2.0).abs() < 1e-5 && (p[2] - 1.5).abs() < 1e-5, "{p:?}");
    d.layers.push(Layer::new("exp", LayerContent::Adjustment(Adjustment::Exposure { exposure: 1.0, offset: 0.0, gamma: 1.0 })));
    let p = render(&d, d.bounds()).px[0];
    assert!((p[0] - 4.0).abs() < 1e-4 && (p[1] - 1.0).abs() < 1e-4, "{p:?}");
    // A layer clipped to a pass-through group (compose's A + (with − without) path).
    let mut grp = Layer::group("g", vec![l.clone()]);
    grp.blend = photocraft_color::BlendMode::PassThrough;
    let mut clip = l;
    clip.clipped = true;
    clip.opacity = 0.5;
    d.layers = vec![grp, clip];
    let p = render(&d, d.bounds()).px[0];
    assert!((p[0] - 2.0).abs() < 1e-4, "{p:?}");
    // The same document in 16-bit clips.
    let mut d16 = d.clone();
    d16.depth = SampleType::U16;
    assert!(render(&d16, d16.bounds()).px[0][0] <= 1.0 + 1e-6);
}
