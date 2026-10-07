//! Colour pipelines: level resolution, serde, space lists, open → working → export, the viewer's
//! output proof, and graceful failures of the commands.

use photocraft_cms::{Builtin, Intent, Profile, Transform};
use photocraft_color::{Color, ColorMode, SampleType};
use photocraft_doc::{Document, Size};
use serde_json::json;

use super::*;
use crate::Session;

fn sp(id: &str) -> SpaceId {
    SpaceId::parse(id).unwrap()
}

fn doc(profile: Option<&Profile>, rgb: [f32; 3], depth: SampleType) -> Document {
    let mut d = Document::with_background("t", Size::new(8, 8), ColorMode::Rgb, depth, Color::rgb(rgb[0], rgb[1], rgb[2]));
    d.icc_profile = profile.map(Profile::to_bytes);
    d
}

fn pipeline(working: &str, output: &str) -> ColorPipeline {
    ColorPipeline { working: sp(working), output: sp(output), ..Default::default() }
}

fn centre(d: &Document) -> [f32; 3] {
    let buf = photocraft_compose::flatten(d);
    let i = 4 * d.size.width as usize + 4;
    let p = buf.px[i];
    [p[0], p[1], p[2]]
}

fn lab(p: &Profile, v: [f32; 3]) -> [f32; 3] {
    let t = Transform::new(p, Builtin::LabD50.profile(), Intent::RelativeColorimetric, false).unwrap();
    let mut o = [0.0f32; 16];
    t.eval(&v, &mut o);
    [o[0] * 100.0, o[1] * 255.0 - 128.0, o[2] * 255.0 - 128.0]
}

fn delta_e(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

// ------------------------------------------------------------------ resolution

#[test]
fn default_pipeline_is_todays_behaviour() {
    let p = ColorPipeline::default();
    assert_eq!(p.input, InputSpace::Auto);
    assert_eq!(p.working.as_str(), "srgb");
    assert_eq!(p.output.as_str(), "srgb");
    assert_eq!(p.intent, Intent::RelativeColorimetric);
    assert!(p.bpc);
    assert_eq!(resolve(&[], &p), p);
}

#[test]
fn resolve_picks_the_most_specific_level_per_field() {
    let base = ColorPipeline::default();
    let project = ColorOverride { working: Some(sp("acescg")), output: Some(sp("rec709-bt1886")), intent: Some(Intent::Perceptual), ..Default::default() };
    let album = ColorOverride { output: Some(sp("display-p3")), bpc: Some(false), ..Default::default() };
    let photo = ColorOverride { input: Some(InputSpace::Space(sp("rec709-oetf"))), output: Some(sp("srgb")), ..Default::default() };
    let none = ColorOverride::default();
    // (levels, expected input, working, output, intent, bpc)
    type Row<'a> = (Vec<&'a ColorOverride>, &'a str, &'a str, &'a str, Intent, bool);
    let table: Vec<Row> = vec![
        (vec![], "auto", "srgb", "srgb", Intent::RelativeColorimetric, true),
        (vec![&none, &none, &none], "auto", "srgb", "srgb", Intent::RelativeColorimetric, true),
        (vec![&none, &none, &project], "auto", "acescg", "rec709-bt1886", Intent::Perceptual, true),
        (vec![&none, &album, &project], "auto", "acescg", "display-p3", Intent::Perceptual, false),
        (vec![&photo, &album, &project], "rec709-oetf", "acescg", "srgb", Intent::Perceptual, false),
        (vec![&photo, &none, &project], "rec709-oetf", "acescg", "srgb", Intent::Perceptual, true),
        (vec![&photo, &album, &none], "rec709-oetf", "srgb", "srgb", Intent::RelativeColorimetric, false),
        (vec![&none, &album, &none], "auto", "srgb", "display-p3", Intent::RelativeColorimetric, false),
        // Order matters: the first level wins.
        (vec![&album, &photo], "rec709-oetf", "srgb", "display-p3", Intent::RelativeColorimetric, false),
        (vec![&project, &album], "auto", "acescg", "rec709-bt1886", Intent::Perceptual, false),
    ];
    for (i, (levels, input, working, output, intent, bpc)) in table.into_iter().enumerate() {
        let r = resolve(&levels, &base);
        assert_eq!(r.input.as_str(), input, "row {i}");
        assert_eq!(r.working.as_str(), working, "row {i}");
        assert_eq!(r.output.as_str(), output, "row {i}");
        assert_eq!(r.intent, intent, "row {i}");
        assert_eq!(r.bpc, bpc, "row {i}");
    }
    // A non-default base fills what no level sets.
    let base2 = pipeline("linear-rec2020", "rec2020");
    assert_eq!(resolve(&[&album], &base2).working.as_str(), "linear-rec2020");
}

#[test]
fn serde_shapes() {
    let p = ColorPipeline::default();
    assert_eq!(serde_json::to_value(&p).unwrap(), json!({"input": "auto", "working": "srgb", "output": "srgb", "intent": "relative", "bpc": true}));
    let o = ColorOverride { output: Some(sp("ACEScg")), intent: Some(Intent::Perceptual), ..Default::default() };
    assert_eq!(serde_json::to_value(&o).unwrap(), json!({"output": "acescg", "intent": "perceptual"}));
    assert_eq!(serde_json::to_value(ColorOverride::default()).unwrap(), json!({}));
    let back: ColorOverride = serde_json::from_value(json!({"output": "acescg", "intent": "perceptual"})).unwrap();
    assert_eq!(back, o);
    let p2: ColorPipeline = serde_json::from_value(json!({"working": "linear-srgb", "input": "rec709-oetf"})).unwrap();
    assert_eq!(p2.working.as_str(), "linear-srgb");
    assert_eq!(p2.input, InputSpace::Space(sp("rec709-oetf")));
    assert_eq!(p2.output.as_str(), "srgb", "missing fields take the defaults");
    assert_eq!(serde_json::from_value::<ColorPipeline>(serde_json::to_value(&p2).unwrap()).unwrap(), p2);
    assert!(serde_json::from_value::<ColorOverride>(json!({"working": "nope"})).is_err());
    assert!(serde_json::from_value::<ColorOverride>(json!({"intent": "sideways"})).is_err());
    assert!(serde_json::from_value::<ColorOverride>(json!({"bpc": "yes"})).is_err());
}

#[test]
fn space_ids() {
    assert_eq!(sp("ACEScg").as_str(), "acescg", "aliases normalize to the canonical id");
    assert_eq!(sp("bt2020").as_builtin(), Some(Builtin::Rec2020));
    assert_eq!(sp("icc:/x/My Monitor.icc").icc_path(), Some("/x/My Monitor.icc"));
    assert_eq!(sp("icc:/x/My Monitor.icc").label(), "My Monitor.icc");
    assert_eq!(sp("linear-srgb").label(), "Linear Rec.709/sRGB");
    assert!(SpaceId::parse("icc:").is_err());
    assert!(SpaceId::parse("sgray").is_err(), "gray spaces are not pipeline spaces");
    assert!(SpaceId::parse("coated-cmyk").is_err());
    assert!(SpaceId::parse("").is_err());
    assert!(SpaceId::parse("unknown-space").unwrap_err().contains("acescg"), "the error lists the choices");
    assert_eq!(InputSpace::parse("AUTO").unwrap(), InputSpace::Auto);
}

#[test]
fn linear_spaces_promote_to_float() {
    for id in ["linear-srgb", "linear-rec2020", "linear-p3-d65", "acescg"] {
        assert!(is_linear(&sp(id)), "{id}");
        assert_eq!(working_depth(SampleType::U8, &sp(id)), SampleType::F32);
    }
    for id in ["srgb", "rec709-bt1886", "display-p3", "prophoto-compat"] {
        assert!(!is_linear(&sp(id)), "{id}");
        assert_eq!(working_depth(SampleType::U16, &sp(id)), SampleType::U16);
    }
    assert!(!is_linear(&sp("icc:/definitely/missing.icc")));
}

#[test]
fn space_lists() {
    let ids = |v: Vec<(&'static str, &'static str)>| v.into_iter().map(|(i, _)| i).collect::<Vec<_>>();
    let display = ["srgb", "rec709-bt1886", "rec709-oetf", "display-p3", "p3-d65", "dci-p3", "rec2020", "rec2020-g24", "adobe-rgb-compat", "prophoto-compat"];
    assert_eq!(ids(input_spaces()), display);
    assert_eq!(ids(output_spaces()), display);
    let w = ids(working_spaces());
    assert_eq!(w.len(), display.len() + 4);
    for id in ["linear-srgb", "linear-rec2020", "linear-p3-d65", "acescg"] {
        assert!(w.contains(&id), "{id}");
    }
    for (id, label) in working_spaces() {
        assert!(SpaceId::parse(id).is_ok() && !label.is_empty(), "{id}");
        assert!(profile(&sp(id)).is_ok());
    }
}

#[test]
fn custom_profile_files() {
    let missing = profile(&sp("icc:/definitely/missing/profile.icc")).unwrap_err().to_string();
    assert!(missing.contains("missing/profile.icc") && missing.contains("built-in"), "{missing}");
    #[cfg(not(target_arch = "wasm32"))]
    {
        let dir = std::env::temp_dir().join(format!("pc-pipeline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rgb = dir.join("p3.icc");
        std::fs::write(&rgb, Builtin::DisplayP3.profile().to_bytes().as_slice()).unwrap();
        let p = profile(&sp(&format!("icc:{}", rgb.display()))).unwrap();
        assert!(p.same_colors(Builtin::DisplayP3.profile()));
        let gray = dir.join("gray.icc");
        std::fs::write(&gray, Builtin::SGray.profile().to_bytes().as_slice()).unwrap();
        assert!(profile(&sp(&format!("icc:{}", gray.display()))).unwrap_err().to_string().contains("RGB"));
        let junk = dir.join("junk.icc");
        std::fs::write(&junk, b"not a profile").unwrap();
        assert!(profile(&sp(&format!("icc:{}", junk.display()))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ------------------------------------------------------------------ open / export

#[test]
fn open_srgb_8bit_into_acescg_then_export_rec709() {
    let colors = [[0.8, 0.5, 0.3], [0.1, 0.6, 0.9], [0.5, 0.5, 0.5], [0.95, 0.9, 0.1]];
    for c in colors {
        let mut s = Session::new();
        let pl = pipeline("acescg", "rec709-bt1886");
        let (i, r) = s.open_document_with_pipeline(doc(None, c, SampleType::U8), None, &pl).unwrap();
        assert_eq!(r["action"], "pipeline");
        assert_eq!(r["converted"], true);
        let d = s.documents()[i].doc.clone();
        assert_eq!(d.depth, SampleType::F32, "linear working space → 32-bit");
        assert!(crate::color_cmds::document_profile(&d).same_colors(Builtin::AcesCg.profile()));
        // The pixels are the sRGB colour in ACEScg.
        let mut want = [0.0f32; 16];
        Transform::new(Builtin::Srgb.profile(), Builtin::AcesCg.profile(), Intent::RelativeColorimetric, true).unwrap().eval(&c.map(|v: f32| (v * 255.0).round() / 255.0), &mut want);
        let got = centre(&d);
        assert!((0..3).all(|k| (got[k] - want[k]).abs() < 3e-3), "{c:?}: {got:?} vs {want:?}");
        // Export through the output space.
        let target = s.export_target(i).unwrap();
        assert!(target.profile.same_colors(Builtin::Rec709Bt1886.profile()));
        let (bytes, _) = crate::file_cmds::encode_to(&d, "x.tif", None, Some(target)).unwrap();
        let back = photocraft_io::import("x.tif", &bytes).unwrap().document;
        let back_profile = crate::color_cmds::document_profile(&back);
        assert!(back_profile.same_colors(Builtin::Rec709Bt1886.profile()), "tagged with the output space");
        // Same colour as converting the sRGB original straight to Rec.709 gamma 2.4.
        let mut direct = [0.0f32; 16];
        Transform::new(Builtin::Srgb.profile(), Builtin::Rec709Bt1886.profile(), Intent::RelativeColorimetric, true).unwrap().eval(&c, &mut direct);
        let out = centre(&back);
        let de = delta_e(lab(Builtin::Rec709Bt1886.profile(), out), lab(Builtin::Rec709Bt1886.profile(), [direct[0], direct[1], direct[2]]));
        assert!(de < 1.0, "{c:?}: ΔE {de} ({out:?} vs {direct:?})");
    }
}

#[test]
fn open_with_assigned_input_and_gamma_working() {
    let mut s = Session::new();
    // A P3-tagged file read as Rec.709 camera: the embedded profile is overridden.
    let pl = ColorPipeline { input: InputSpace::Space(sp("rec709-oetf")), ..pipeline("display-p3", "srgb") };
    let (i, _) = s.open_document_with_pipeline(doc(Some(Builtin::DisplayP3.profile()), [0.6, 0.4, 0.2], SampleType::U16), None, &pl).unwrap();
    let d = s.documents()[i].doc.clone();
    assert_eq!(d.depth, SampleType::U16, "gamma working spaces keep the depth");
    let mut want = [0.0f32; 16];
    Transform::new(Builtin::Rec709Oetf.profile(), Builtin::DisplayP3.profile(), Intent::RelativeColorimetric, true).unwrap().eval(&[0.6, 0.4, 0.2], &mut want);
    let got = centre(&d);
    assert!((0..3).all(|k| (got[k] - want[k]).abs() < 2e-3), "{got:?} vs {want:?}");
    assert_eq!(s.doc_pipeline(i).unwrap().pipeline, pl);
    // Already in the working space: tagged, not converted.
    let (_, r) = s.open_document_with_pipeline(doc(Some(Builtin::DisplayP3.profile()), [0.6, 0.4, 0.2], SampleType::U8), None, &pipeline("display-p3", "srgb")).unwrap();
    assert_eq!(r["converted"], false);
}

#[test]
fn non_rgb_documents_keep_the_color_settings_policy() {
    let mut s = Session::new();
    let gray = Document::with_background("g", Size::new(4, 4), ColorMode::Grayscale, SampleType::U8, Color::gray(0.5));
    let (i, r) = s.open_document_with_pipeline(gray, None, &pipeline("acescg", "srgb")).unwrap();
    assert_eq!(r["pipeline"], "skipped");
    assert!(s.doc_pipeline(i).is_none());
    assert_eq!(s.documents()[i].doc.mode, ColorMode::Grayscale);
    assert!(s.is_enabled("color.pipeline") && !s.is_enabled("color.setPipeline"));
}

#[test]
fn bad_pipeline_fails_before_opening() {
    let mut s = Session::new();
    let pl = ColorPipeline { output: SpaceId("icc:/nowhere/x.icc".into()), ..Default::default() };
    assert!(s.open_document_with_pipeline(doc(None, [0.5; 3], SampleType::U8), None, &pl).is_err());
    assert!(s.documents().is_empty(), "nothing half-opened");
}

// ------------------------------------------------------------------ viewer

#[test]
fn viewer_proofs_through_the_output() {
    let mut s = Session::new();
    let (i, _) = s.open_document_with_pipeline(doc(None, [0.9, 0.2, 0.1], SampleType::U8), None, &pipeline("prophoto-compat", "prophoto-compat")).unwrap();
    let d = s.documents()[i].doc.clone();
    let k_same = s.color.canvas_display(&d).unwrap().key;
    assert!(s.color.display_output(&d).is_none(), "output = working: plain display");
    let sig_same = s.color.display_signature(&d);
    s.execute("color.setPipeline", json!({"output": "srgb"})).unwrap();
    let d = s.documents()[i].doc.clone();
    assert!(s.color.display_output(&d).is_some());
    let disp = s.color.canvas_display(&d).unwrap();
    assert_ne!(disp.key, k_same, "the canvas key follows the output");
    assert_ne!(s.color.display_signature(&d), sig_same);
    assert!(disp.transform.is_some());
    // A saturated ProPhoto colour beyond sRGB is clipped to the sRGB gamut on screen.
    let t = s.color.display_transform(&d).unwrap();
    let mut o = [0.0f32; 16];
    t.eval(&[0.2, 1.0, 0.0], &mut o);
    assert!((0..3).all(|k| (-1e-5..=1.0 + 1e-5).contains(&o[k])), "{o:?}");
    // Another output → another key.
    s.execute("color.setPipeline", json!({"output": "rec2020"})).unwrap();
    assert_ne!(s.color.canvas_display(&d).unwrap().key, disp.key);
    // Proof Colors wins over the pipeline output.
    s.execute("view.proofSetup", json!({"profile": "display-p3"})).unwrap();
    s.execute("view.proofColors", json!({"on": true})).unwrap();
    let proofed = s.color.display_transform(&d).unwrap();
    let expect = Transform::proof(
        &s.color.canvas_display(&d).unwrap().source,
        Builtin::DisplayP3.profile(),
        &s.color.monitor(),
        Intent::RelativeColorimetric,
        true,
        false,
    )
    .unwrap();
    let (mut a, mut b) = ([0.0f32; 16], [0.0f32; 16]);
    proofed.eval(&[0.2, 0.9, 0.1], &mut a);
    expect.eval(&[0.2, 0.9, 0.1], &mut b);
    assert!((0..3).all(|k| (a[k] - b[k]).abs() < 1e-4), "{a:?} vs {b:?}");
    // Gamut warning still works with a pipeline.
    s.execute("view.gamutWarning", json!({"on": true})).unwrap();
    assert!(s.color.canvas_lut(&d, 9).unwrap().is_some());
    // Closing the document drops its pipeline.
    let id = d.id;
    s.close(i);
    assert!(s.color.pipeline(id).is_none());
}

// ------------------------------------------------------------------ commands

#[test]
fn set_pipeline_on_an_open_document() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 8, "height": 8, "mode": "rgb", "depth": 8})).unwrap();
    let r = s.execute("color.pipeline", json!({})).unwrap();
    assert!(r["pipeline"].is_null());
    assert_eq!(r["spaces"]["working"].as_array().unwrap().len(), 14);
    // First pipeline: applied in full (undoable).
    let r = s.execute("color.setPipeline", json!({"working": "linear-rec2020", "output": "rec709-bt1886"})).unwrap();
    assert_eq!(r["pipeline"]["working"], "linear-rec2020");
    assert_eq!(r["pipeline"]["input"], "auto");
    let d = s.active().unwrap().doc.clone();
    assert_eq!(d.depth, SampleType::F32);
    assert!(crate::color_cmds::document_profile(&d).same_colors(Builtin::LinearRec2020.profile()));
    // Output only: no pixel change.
    let rev = s.active().unwrap().revision;
    s.execute("color.setPipeline", json!({"output": "srgb", "intent": "perceptual", "bpc": false})).unwrap();
    assert_eq!(s.active().unwrap().revision, rev, "output changes don't edit the document");
    let ap = s.doc_pipeline(0).unwrap();
    assert_eq!((ap.pipeline.output.as_str(), ap.pipeline.intent, ap.pipeline.bpc), ("srgb", Intent::Perceptual, false));
    // New working space: re-converted.
    s.execute("color.setPipeline", json!({"working": "acescg"})).unwrap();
    assert!(crate::color_cmds::document_profile(&s.active().unwrap().doc).same_colors(Builtin::AcesCg.profile()));
    assert_ne!(s.active().unwrap().revision, rev);
    // Input can't change after the fact.
    let e = s.execute("color.setPipeline", json!({"input": "rec709-oetf"})).unwrap_err().to_string();
    assert!(e.contains("reopen the original"), "{e}");
    // Same input is fine.
    s.execute("color.setPipeline", json!({"input": "auto"})).unwrap();
    // Clear.
    let r = s.execute("color.setPipeline", json!({"clear": true})).unwrap();
    assert!(r["pipeline"].is_null());
    assert!(s.export_target(0).is_none());
}

#[test]
fn pipeline_commands_fail_gracefully() {
    let mut s = Session::new();
    assert!(s.execute("color.setPipeline", json!({"working": "srgb"})).is_err(), "no document");
    assert!(s.execute("color.pipeline", json!({})).is_ok());
    s.execute("file.new", json!({"width": 8, "height": 8, "mode": "rgb", "depth": 8})).unwrap();
    for bad in [
        json!({"working": "nope"}),
        json!({"working": 3}),
        json!({"output": "coated-cmyk"}),
        json!({"input": ["srgb"]}),
        json!({"intent": "sideways"}),
        json!({"intent": 1}),
        json!({"bpc": "yes"}),
        json!({"clear": "yes"}),
        json!({"working": "icc:"}),
        json!({"output": "icc:/no/such/file.icc"}),
        json!("srgb"),
        json!(null),
        json!([1, 2]),
    ] {
        assert!(s.execute("color.setPipeline", bad.clone()).is_err(), "{bad}");
    }
    assert!(s.doc_pipeline(0).is_none(), "failed calls change nothing");
    // A pipeline on file.openAs: bad shapes are refused before reading anything.
    #[cfg(not(target_arch = "wasm32"))]
    {
        assert!(s.execute("file.openAs", json!({"path": "/no/such/file.png", "colorPipeline": {"working": "nope"}})).is_err());
        assert!(s.execute("file.openAs", json!({"path": "/no/such/file.png", "colorPipeline": 5})).is_err());
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn open_as_with_a_pipeline_and_save_a_copy_in_the_output_space() {
    let dir = std::env::temp_dir().join(format!("pc-pipeline-open-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("in.png");
    let (bytes, _) = crate::file_cmds::encode(&doc(None, [0.7, 0.3, 0.2], SampleType::U8), "in.png", None).unwrap();
    std::fs::write(&src, bytes).unwrap();
    let mut s = Session::new();
    let r = s
        .execute("file.openAs", json!({"path": src.to_string_lossy(), "colorPipeline": {"working": "linear-srgb", "output": "display-p3"}}))
        .unwrap();
    assert_eq!(r["color"]["action"], "pipeline");
    assert_eq!(s.active().unwrap().doc.depth, SampleType::F32);
    let out = dir.join("out.png");
    s.execute("file.saveACopy", json!({"path": out.to_string_lossy()})).unwrap();
    let back = photocraft_io::import("out.png", &std::fs::read(&out).unwrap()).unwrap().document;
    assert!(crate::color_cmds::document_profile(&back).same_colors(Builtin::DisplayP3.profile()));
    let psd = dir.join("out.psd");
    s.execute("file.saveACopy", json!({"path": psd.to_string_lossy()})).unwrap();
    let back = photocraft_io::import("out.psd", &std::fs::read(&psd).unwrap()).unwrap().document;
    assert!(crate::color_cmds::document_profile(&back).same_colors(Builtin::LinearSrgb.profile()), "PSD keeps the working space");
    let _ = std::fs::remove_dir_all(&dir);
}
