//! Album looks end to end on a temp folder: a look edited in one photo reaches the album's other
//! open photos, bypass, saving (sidecar without the look, `.pvlook`), reopening, export,
//! thumbnails, moving layers in and out, refusals, and graceful failures.

use std::path::PathBuf;

use photocraft_color::{Color, ColorMode, SampleType};
use photocraft_doc::{Adjustment, Document, Layer, LayerContent, LayerId, Size};
use serde_json::{Value, json};

use crate::Session;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("pv-look-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_image(path: &str, rgb: [f32; 3]) {
    let d = Document::with_background("t", Size::new(16, 12), ColorMode::Rgb, SampleType::U8, Color::rgb(rgb[0], rgb[1], rgb[2]));
    let (bytes, _) = crate::file_cmds::encode(&d, path, None).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn ok(s: &mut Session, id: &str, p: Value) -> Value {
    s.execute(id, p.clone()).unwrap_or_else(|e| panic!("{id} {p}: {e}"))
}

/// A project "P" with album "Day 1" (photos a, b) and album "Other" (photo c).
struct Fixture {
    t: TempDir,
    s: Session,
    album: u64,
    other: u64,
    a: u64,
    b: u64,
    c: u64,
}

fn fixture(tag: &str) -> Fixture {
    let t = TempDir::new(tag);
    for (n, rgb) in [("a.png", [0.8, 0.4, 0.2]), ("b.png", [0.2, 0.5, 0.9]), ("c.png", [0.6, 0.6, 0.6])] {
        write_image(&t.path(n), rgb);
    }
    let mut s = Session::new();
    ok(&mut s, "project.new", json!({"path": t.path("P.pvproj")}));
    let album = ok(&mut s, "album.new", json!({"name": "Day 1"}))["id"].as_u64().unwrap();
    let other = ok(&mut s, "album.new", json!({"name": "Other"}))["id"].as_u64().unwrap();
    let r = ok(&mut s, "album.import", json!({"album": album, "mode": "reference", "paths": [t.path("a.png"), t.path("b.png")]}));
    let (a, b) = (r["results"][0]["id"].as_u64().unwrap(), r["results"][1]["id"].as_u64().unwrap());
    let r = ok(&mut s, "album.import", json!({"album": other, "mode": "reference", "paths": [t.path("c.png")]}));
    let c = r["results"][0]["id"].as_u64().unwrap();
    Fixture { t, s, album, other, a, b, c }
}

fn open(s: &mut Session, photo: u64) -> usize {
    ok(s, "photo.open", json!({"id": photo}))["document"].as_u64().unwrap() as usize
}

fn activate(s: &mut Session, photo: u64) -> usize {
    let i = s.photo_document(photo).unwrap();
    s.set_active(i);
    i
}

/// The composite's centre pixel of open document `i`.
fn centre(s: &Session, i: usize) -> Vec<f32> {
    let d = &s.documents()[i].doc;
    crate::file_cmds::flattened(d, d.pixel_format()).pixel(8, 6)
}

fn near(a: &[f32], b: &[f32]) -> bool {
    a.iter().zip(b).take(3).all(|(x, y)| (x - y).abs() < 0.02)
}

fn group(s: &Session, i: usize) -> Layer {
    let id = s.album_look_group(i).expect("the document has its album look group");
    s.documents()[i].doc.layer(id).unwrap().clone()
}

fn look_count(s: &mut Session, album: u64) -> u64 {
    ok(s, "album.look.info", json!({"album": album}))["count"].as_u64().unwrap()
}

/// Adds an adjustment layer to the active photo's album look: a new adjustment layer goes into
/// the look by itself when a look layer is targeted, else Move to Album Look puts it there.
fn add_to_look(s: &mut Session, kind: &str, p: Value) -> LayerId {
    ok(s, &format!("layer.newAdjustmentLayer.{kind}"), p);
    let i = s.active_index().unwrap();
    let id = s.active().unwrap().active_layer.unwrap();
    let gid = s.album_look_group(i).unwrap();
    let inside = |s: &Session| group(s, i).children().unwrap().iter().any(|l| l.id == id);
    if !inside(s) {
        let r = ok(s, "layer.toAlbumLook", json!({}));
        assert_eq!(r["moved"], 1, "{r}");
    }
    assert!(inside(s) && s.album_look_group(i) == Some(gid));
    id
}

fn exposure_of(l: &Layer) -> f32 {
    match &l.content {
        LayerContent::Adjustment(Adjustment::Exposure { exposure, .. }) => *exposure,
        other => panic!("not an exposure layer: {}", other.kind_name()),
    }
}

#[test]
fn a_look_edited_in_one_photo_applies_to_the_albums_other_photos() {
    let Fixture { t: _t, mut s, album, a, b, c, .. } = fixture("sync");
    let ia = open(&mut s, a);
    let ib = open(&mut s, b);
    let ic = open(&mut s, c);
    // Every photo gets its album's (empty) look as the top group; new layers go below it.
    for (i, name) in [(ia, "Album Look — Day 1"), (ib, "Album Look — Day 1"), (ic, "Album Look — Other")] {
        let d = &s.documents()[i];
        let g = group(&s, i);
        assert_eq!(g.name, name);
        assert!(g.visible && g.children().unwrap().is_empty());
        assert_eq!(d.doc.layers.last().unwrap().id, g.id, "the look sits on top of the photo's own layers");
        assert_ne!(d.active_layer, Some(g.id), "the photo's own top layer is targeted");
    }
    let (b0, c0) = (centre(&s, ib), centre(&s, ic));
    // Invert in photo A's look: photo B (same album) is inverted too; C (other album) is not.
    activate(&mut s, a);
    add_to_look(&mut s, "invert", json!({}));
    assert_eq!(group(&s, ia).children().unwrap().len(), 1);
    assert_eq!(group(&s, ib).children().unwrap().len(), 1);
    let b1 = centre(&s, ib);
    assert!(near(&b1, &[1.0 - b0[0], 1.0 - b0[1], 1.0 - b0[2]]), "B inverted: {b0:?} → {b1:?}");
    assert!(near(&centre(&s, ic), &c0), "another album is unaffected");
    assert!(group(&s, ic).children().unwrap().is_empty());
    assert_eq!(look_count(&mut s, album), 1);
    assert!(s.project.as_ref().unwrap().dirty, "a look change dirties the project");
    assert!(!s.documents()[ib].is_dirty(), "B's own edits didn't change: its sidecar needs no save");

    // An exposure layer in the look, edited in A with the Properties command: B follows.
    let e = add_to_look(&mut s, "exposure", json!({"exposure": 1.0}));
    assert_eq!(s.active().unwrap().active_layer, Some(e));
    ok(&mut s, "layer.setAdjustment", json!({"exposure": 2.0}));
    let top = |s: &Session, i: usize| group(s, i).children().unwrap().last().unwrap().clone();
    assert_eq!(exposure_of(&top(&s, ib)), 2.0);
    // B's look layers keep their ids across updates (selection and panels stay put).
    let b_id = top(&s, ib).id;
    ok(&mut s, "layer.setAdjustment", json!({"exposure": 1.5}));
    assert_eq!(top(&s, ib).id, b_id);
    // Undo in A changes A's look again, which syncs again.
    assert!(s.undo());
    ok(&mut s, "project.info", json!({}));
    assert_eq!(exposure_of(&top(&s, ib)), 2.0, "undo in the editing photo reaches B");
    // Undoing an unrelated edit in B never brings back an older look.
    activate(&mut s, b);
    ok(&mut s, "layer.new.layer", json!({}));
    activate(&mut s, a);
    ok(&mut s, "layer.setAdjustment", json!({"exposure": 3.0}));
    activate(&mut s, b);
    assert!(s.undo());
    ok(&mut s, "project.info", json!({}));
    assert_eq!(exposure_of(&top(&s, ib)), 3.0, "B's undo states got the current look");
    assert_eq!(exposure_of(&top(&s, ia)), 3.0);
    // B can edit the look too.
    let id = top(&s, ib).id;
    ok(&mut s, "layer.select", json!({"layer": id.0}));
    ok(&mut s, "layer.setAdjustment", json!({"exposure": -1.0}));
    assert_eq!(exposure_of(&top(&s, ia)), -1.0);

    // Bypass for B: its group hides and its composite is the photo's own again.
    let r = ok(&mut s, "photo.setAlbumLook", json!({"id": b, "enabled": false}));
    assert_eq!(r["document"], ib);
    assert!(!group(&s, ib).visible);
    assert!(near(&centre(&s, ib), &b0), "bypassed: B is unaffected");
    assert!(group(&s, ia).visible, "A still shows the look");
    let info = ok(&mut s, "project.info", json!({}));
    let photos = info["albums"][0]["photos"].as_array().unwrap();
    assert_eq!(photos.iter().find(|p| p["id"] == b).unwrap()["albumLook"], false);
    assert_eq!(info["albums"][0]["look"]["bypassed"], json!([b]));
    // The group's eye is the same switch.
    activate(&mut s, b);
    let gid = group(&s, ib).id;
    ok(&mut s, "layer.setProps", json!({"layer": gid.0, "visible": true}));
    assert!(s.project.as_ref().unwrap().project.find_photo(b).unwrap().1.album_look, "the eye turned the look back on for B");
    // Album-wide switch.
    ok(&mut s, "album.look.setEnabled", json!({"album": album, "enabled": false}));
    assert!(!group(&s, ia).visible && !group(&s, ib).visible);
    assert!(near(&centre(&s, ib), &b0));
    // Showing the group again turns the album's look back on (with a notice).
    s.notices.clear();
    ok(&mut s, "layer.setProps", json!({"layer": gid.0, "visible": true}));
    assert!(s.project.as_ref().unwrap().project.album(album).unwrap().look_enabled);
    assert!(group(&s, ia).visible);
    assert_eq!(s.take_notices().len(), 1);
    // Renaming the album renames the groups.
    ok(&mut s, "album.rename", json!({"id": album, "name": "Day One"}));
    assert_eq!(group(&s, ia).name, "Album Look — Day One");
    // Clear: every photo of the album loses the look.
    let r = ok(&mut s, "album.look.clear", json!({"album": album}));
    assert_eq!(r["cleared"], 2);
    assert!(group(&s, ia).children().unwrap().is_empty() && group(&s, ib).children().unwrap().is_empty());
    assert!(near(&centre(&s, ib), &b0));
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn saving_writes_the_sidecar_without_the_look_and_the_pvlook() {
    let Fixture { t, mut s, album, a, b, .. } = fixture("save");
    let ia = open(&mut s, a);
    add_to_look(&mut s, "invert", json!({}));
    let own_layers = s.documents()[ia].doc.layers.len() - 1;
    let r = ok(&mut s, "photo.save", json!({}));
    let side = r["path"].as_str().unwrap().to_string();
    let look_file = t.path(&format!("P Looks/album-{album}.pvlook"));
    assert_eq!(r["look"].as_str().map(|p| p.replace('\\', "/")), Some(look_file.replace('\\', "/")), "{r}");
    assert!(std::path::Path::new(&look_file).is_file());
    // The sidecar holds only the photo's own layers.
    let saved = photocraft_io::import("a.png.pvision", &std::fs::read(&side).unwrap()).unwrap().document;
    assert_eq!(saved.layers.len(), own_layers);
    assert!(!saved.walk().iter().any(|(_, _, l)| l.name.starts_with("Album Look")));
    assert!(!s.active().unwrap().is_dirty());
    // Reopen from the sidecar: the look is injected again, once.
    s.close(ia);
    let ia = open(&mut s, a);
    assert_eq!(s.documents()[ia].doc.layers.len(), own_layers + 1);
    assert_eq!(group(&s, ia).children().unwrap().len(), 1);
    // A Revert keeps the look (the sidecar has none).
    ok(&mut s, "layer.new.layer", json!({}));
    ok(&mut s, "file.revert", json!({}));
    assert_eq!(group(&s, ia).children().unwrap().len(), 1);
    assert_eq!(s.documents()[ia].doc.layers.len(), own_layers + 1);
    // The project file references the look; a round trip keeps it.
    ok(&mut s, "photo.setAlbumLook", json!({"id": b, "enabled": false}));
    ok(&mut s, "project.save", json!({}));
    let text = std::fs::read_to_string(t.path("P.pvproj")).unwrap();
    assert!(text.contains(&format!("\"look\": \"P Looks/album-{album}.pvlook\"")), "{text}");
    ok(&mut s, "project.close", json!({}));
    let info = ok(&mut s, "project.open", json!({"path": t.path("P.pvproj")}));
    assert_eq!(info["lookWarnings"], json!([]));
    assert_eq!(info["albums"][0]["look"]["count"], 1, "{info}");
    assert_eq!(info["albums"][0]["look"]["bypassed"], json!([b]));
    let ib = open(&mut s, b);
    assert_eq!(group(&s, ib).children().unwrap().len(), 1);
    assert!(!group(&s, ib).visible, "the bypass survived the round trip");
    // Clearing the look deletes its file on the next save.
    ok(&mut s, "album.look.clear", json!({"album": album}));
    ok(&mut s, "project.save", json!({}));
    assert!(!std::path::Path::new(&look_file).exists());
    assert!(!std::fs::read_to_string(t.path("P.pvproj")).unwrap().contains("\"look\""));
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn shell_saves_strip_the_look_through_document_to_save() {
    let Fixture { t: _t, mut s, a, .. } = fixture("shellsave");
    let ia = open(&mut s, a);
    add_to_look(&mut s, "invert", json!({}));
    let side = s.active_photo_sidecar().unwrap();
    let full = s.documents()[ia].doc.layers.len();
    assert_eq!(s.document_to_save(ia, &side).unwrap().layers.len(), full - 1, "the sidecar gets no look");
    assert_eq!(s.document_to_save(ia, &side.replace('/', "\\")).unwrap().layers.len(), full - 1);
    assert_eq!(s.document_to_save(ia, "C:/elsewhere/copy.psd").unwrap().layers.len(), full, "a copy keeps the look");
    assert!(crate::project_cmds::save_photo_look(&mut s).unwrap().is_some());
    assert_eq!(crate::project_cmds::save_photo_look(&mut s).unwrap(), None, "unchanged looks aren't rewritten");
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn exports_and_thumbnails_include_the_look() {
    let Fixture { t, mut s, album, a, b, .. } = fixture("export");
    let plain_a = crate::project_cmds::thumbnail_plan(&s, a, 32).unwrap().path;
    let plain_b = crate::project_cmds::thumbnail_plan(&s, b, 32).unwrap().path;
    let thumb_px = |path: &str| {
        let img = photocraft_codecs::decode(&std::fs::read(path).unwrap()).unwrap();
        let rgba = img.to_rgba8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let i = ((h / 2) * w + w / 2) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    };
    let r = ok(&mut s, "photo.thumbnail", json!({"id": a, "maxSide": 32}));
    let before = thumb_px(r["path"].as_str().unwrap());
    open(&mut s, a);
    add_to_look(&mut s, "invert", json!({}));
    ok(&mut s, "photo.setAlbumLook", json!({"id": b, "enabled": false}));
    // A new look gives photo A a new thumbnail key; bypassed B keeps its key.
    let plan = crate::project_cmds::thumbnail_plan(&s, a, 32).unwrap();
    assert_ne!(plan.path, plain_a);
    assert_eq!(crate::project_cmds::thumbnail_plan(&s, b, 32).unwrap().path, plain_b);
    let r = ok(&mut s, "photo.thumbnail", json!({"id": a, "maxSide": 32}));
    assert_eq!(r["cached"], false);
    let after = thumb_px(r["path"].as_str().unwrap());
    for k in 0..3 {
        assert!((after[k] as i32 - (255 - before[k] as i32)).abs() <= 4, "inverted thumbnail {before:?} → {after:?}");
    }
    // The look's settings are part of the key.
    ok(&mut s, "album.look.setEnabled", json!({"album": album, "enabled": false}));
    assert_eq!(crate::project_cmds::thumbnail_plan(&s, a, 32).unwrap().path, plain_a, "a disabled look is not in the picture");
    ok(&mut s, "album.look.setEnabled", json!({"album": album, "enabled": true}));
    // Album export: A is inverted, bypassed B is not.
    let out = t.path("out");
    let r = ok(&mut s, "album.export", json!({"album": album, "folder": out, "format": "png"}));
    assert_eq!(r["exported"], 2, "{r}");
    let px = |name: &str| {
        let img = photocraft_codecs::decode(&std::fs::read(format!("{out}/{name}")).unwrap()).unwrap();
        let rgba = img.to_rgba8();
        [rgba[0], rgba[1], rgba[2]]
    };
    let (ea, eb) = (px("a.png"), px("b.png"));
    assert!((ea[0] as i32 - 51).abs() <= 3 && (ea[2] as i32 - 204).abs() <= 3, "A exported with the look: {ea:?}");
    assert!((eb[0] as i32 - 51).abs() <= 3 && (eb[2] as i32 - 230).abs() <= 3, "B bypasses it: {eb:?}");
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn layers_a_look_cannot_hold_are_refused_or_moved_out() {
    let Fixture { t: _t, mut s, album, a, b, .. } = fixture("refuse");
    let ia = open(&mut s, a);
    let ib = open(&mut s, b);
    activate(&mut s, a);
    // Move to Album Look refuses a pixel layer.
    ok(&mut s, "layer.new.layer", json!({}));
    let pixel = s.active().unwrap().active_layer.unwrap();
    let e = s.execute("layer.toAlbumLook", json!({})).unwrap_err().to_string();
    assert!(e.contains("only adjustment and fill layers"), "{e}");
    // Dragged into the group anyway: it is moved back out, just below the group, with a notice.
    let gid = group(&s, ia).id;
    ok(&mut s, "layer.moveTo", json!({"layer": pixel.0, "target": gid.0, "position": "into"}));
    let d = &s.documents()[ia];
    assert!(group(&s, ia).children().unwrap().is_empty(), "the look holds no pixels");
    let n = d.doc.layers.len();
    assert_eq!(d.doc.layers[n - 2].id, pixel, "moved out just below the look");
    assert_eq!(d.doc.layers[n - 1].id, gid);
    assert_eq!(look_count(&mut s, album), 0);
    assert!(group(&s, ib).children().unwrap().is_empty());
    let notes = s.take_notices();
    assert!(notes.iter().any(|n| n.contains("can't be part of an album look")), "{notes:?}");
    // A pixel mask on a look layer is removed (photos differ in size).
    ok(&mut s, "layer.newAdjustmentLayer.invert", json!({}));
    ok(&mut s, "layer.layerMask.revealAll", json!({}));
    let r = ok(&mut s, "layer.toAlbumLook", json!({}));
    assert_eq!(r["warnings"].as_array().unwrap().len(), 1, "{r}");
    assert!(group(&s, ia).children().unwrap()[0].mask.is_none());
    assert!(group(&s, ib).children().unwrap()[0].mask.is_none());
    // Nothing selected outside the look (the group itself is selected): disabled.
    ok(&mut s, "layer.select", json!({"layer": gid.0}));
    assert!(!s.is_enabled("layer.toAlbumLook"));
    assert!(s.execute("layer.toAlbumLook", json!({})).is_err());
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn deleting_the_group_bypasses_the_photo_and_keeps_the_look() {
    let Fixture { t: _t, mut s, album, a, b, .. } = fixture("delete");
    let ia = open(&mut s, a);
    let ib = open(&mut s, b);
    activate(&mut s, a);
    add_to_look(&mut s, "invert", json!({}));
    let gid = group(&s, ia).id;
    ok(&mut s, "layer.select", json!({"layer": gid.0}));
    ok(&mut s, "layer.delete", json!({}));
    assert!(s.album_look_group(ia).is_none());
    let st = s.project.as_ref().unwrap();
    assert!(!st.project.find_photo(a).unwrap().1.album_look, "deleting the group bypasses the look for A");
    assert_eq!(look_count(&mut s, album), 1, "the look itself is kept");
    assert_eq!(group(&s, ib).children().unwrap().len(), 1, "B still has it");
    let notes = s.take_notices();
    assert!(notes.iter().any(|n| n.contains("Use album look")), "{notes:?}");
    // Undo brings the group back and the photo uses the look again.
    assert!(s.undo());
    ok(&mut s, "project.info", json!({}));
    assert_eq!(s.album_look_group(ia), Some(gid));
    assert!(s.project.as_ref().unwrap().project.find_photo(a).unwrap().1.album_look);
    // Deleted again, then "Use album look" puts it back (same group, current look).
    ok(&mut s, "layer.select", json!({"layer": gid.0}));
    ok(&mut s, "layer.delete", json!({}));
    let r = ok(&mut s, "photo.setAlbumLook", json!({"id": a, "enabled": true}));
    assert_eq!(r["enabled"], true);
    assert_eq!(s.album_look_group(ia), Some(gid));
    assert_eq!(group(&s, ia).children().unwrap().len(), 1);
    // Undo of an older step doesn't lose it again (the undo states got it too).
    assert!(s.undo());
    ok(&mut s, "project.info", json!({}));
    assert_eq!(s.album_look_group(ia), Some(gid));
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn copy_from_album_look_makes_a_local_layer() {
    let Fixture { t: _t, mut s, album, a, .. } = fixture("from");
    let ia = open(&mut s, a);
    let e = add_to_look(&mut s, "exposure", json!({"exposure": 0.5}));
    assert!(s.is_enabled("layer.fromAlbumLook"));
    let r = ok(&mut s, "layer.fromAlbumLook", json!({}));
    assert_eq!(r["copied"], 1);
    let d = &s.documents()[ia];
    let n = d.doc.layers.len();
    let copy = &d.doc.layers[n - 2];
    assert_ne!(copy.id, e);
    assert_eq!(exposure_of(copy), 0.5);
    assert_eq!(d.active_layer, Some(copy.id));
    assert_eq!(look_count(&mut s, album), 1, "the look is unchanged");
    // The copy is the photo's own: not in the look, and Copy from Album Look is disabled for it.
    assert!(!s.is_enabled("layer.fromAlbumLook"));
    assert!(s.is_enabled("layer.toAlbumLook"));
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn a_broken_look_file_is_reported_and_kept() {
    let Fixture { t, mut s, album, a, .. } = fixture("broken");
    open(&mut s, a);
    add_to_look(&mut s, "invert", json!({}));
    ok(&mut s, "project.save", json!({}));
    let file = t.path(&format!("P Looks/album-{album}.pvlook"));
    std::fs::write(&file, b"not a bundle").unwrap();
    ok(&mut s, "project.close", json!({"discard": true}));
    let info = ok(&mut s, "project.open", json!({"path": t.path("P.pvproj")}));
    assert_eq!(info["lookWarnings"].as_array().unwrap().len(), 1, "{info}");
    assert_eq!(info["albums"][0]["look"]["count"], 0);
    assert!(info["albums"][0]["look"]["broken"].is_string());
    ok(&mut s, "project.save", json!({}));
    assert_eq!(std::fs::read(&file).unwrap(), b"not a bundle", "an unreadable look is left alone");
    // An album look path in the project that escapes the folder refuses to load.
    let text = std::fs::read_to_string(t.path("P.pvproj")).unwrap().replace("P Looks/", "../");
    std::fs::write(t.path("Q.pvproj"), text).unwrap();
    assert!(s.execute("project.open", json!({"path": t.path("Q.pvproj"), "discard": true})).is_err());
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn look_files_round_trip_and_reject_junk() {
    let mut g = Layer::group("x", vec![Layer::new("Invert", LayerContent::Adjustment(Adjustment::Invert))]);
    g.opacity = 0.5;
    let look = super::AlbumLook { group: g.clone(), revision: 1, dirty: true, broken: None };
    let (back, notes) = super::decode(&super::encode(&look).unwrap()).unwrap();
    assert!(notes.is_empty());
    assert_eq!(back.opacity, 0.5);
    assert_eq!(super::group_hash(&back), super::group_hash(&g), "the hash ignores ids and names");
    assert_ne!(super::group_hash(&back), 0);
    assert_eq!(super::group_hash(&Layer::group("e", vec![])), 0);
    for junk in [&b""[..], b"PK\x03\x04", b"{}", &[0xff; 64]] {
        assert!(super::decode(junk).is_err());
    }
    // A bundle holding pixels keeps only what a look may hold.
    let mut doc = Document::new("x", Size::new(4, 4), ColorMode::Rgb, SampleType::U8);
    let pix = Layer::raster("px", doc.pixel_format());
    doc.layers.push(Layer::group("g", vec![pix, Layer::new("Invert", LayerContent::Adjustment(Adjustment::Invert))]));
    let bytes = photocraft_format::save_to_bytes(&doc, &Default::default()).unwrap();
    let (g, notes) = super::decode(&bytes).unwrap();
    assert_eq!(g.children().unwrap().len(), 1);
    assert!(!notes.is_empty());
}

#[test]
fn album_look_commands_fail_gracefully() {
    let mut s = Session::new();
    for (id, p) in [
        ("album.look.info", json!({"album": 1})),
        ("album.look.setEnabled", json!({"album": 1, "enabled": true})),
        ("album.look.clear", json!({"album": 1})),
        ("photo.setAlbumLook", json!({"id": 1, "enabled": true})),
        ("layer.toAlbumLook", json!({})),
        ("layer.fromAlbumLook", json!({})),
    ] {
        assert!(s.execute(id, p.clone()).is_err(), "{id} {p} should fail without a project");
    }
    let Fixture { t: _t, mut s, album, a, other, .. } = fixture("bad");
    for (id, p) in [
        ("album.look.info", json!({})),
        ("album.look.info", json!({"album": 999})),
        ("album.look.info", json!({"album": "x"})),
        ("album.look.info", json!({"album": -1})),
        ("album.look.setEnabled", json!({"album": album})),
        ("album.look.setEnabled", json!({"album": album, "enabled": "yes"})),
        ("album.look.setEnabled", json!({"album": 999, "enabled": true})),
        ("album.look.setEnabled", json!({"enabled": true})),
        ("album.look.clear", json!({})),
        ("album.look.clear", json!({"album": 1.5})),
        ("album.look.clear", json!({"album": 999})),
        ("photo.setAlbumLook", json!({"id": a})),
        ("photo.setAlbumLook", json!({"id": a, "enabled": 1})),
        ("photo.setAlbumLook", json!({"id": album, "enabled": true})),
        ("photo.setAlbumLook", json!({"enabled": true})),
        ("photo.setAlbumLook", json!({"id": 999, "enabled": false})),
        ("layer.toAlbumLook", json!({})),
        ("layer.fromAlbumLook", json!({})),
    ] {
        assert!(s.execute(id, p.clone()).is_err(), "{id} {p} should fail");
    }
    // A plain document (not a project photo) has no album look.
    ok(&mut s, "file.new", json!({"width": 8, "height": 8}));
    ok(&mut s, "layer.newAdjustmentLayer.invert", json!({}));
    assert!(s.execute("layer.toAlbumLook", json!({})).is_err());
    assert!(s.execute("layer.fromAlbumLook", json!({})).is_err());
    // Bypassing a photo that isn't open is fine; the info reports it.
    ok(&mut s, "photo.setAlbumLook", json!({"id": a, "enabled": false}));
    assert_eq!(ok(&mut s, "album.look.info", json!({"album": album}))["bypassed"], json!([a]));
    assert_eq!(ok(&mut s, "album.look.info", json!({"album": other}))["count"], 0);
    ok(&mut s, "project.close", json!({"discard": true}));
}
