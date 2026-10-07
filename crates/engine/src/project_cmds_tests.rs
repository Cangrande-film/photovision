//! Projects end to end on a temp folder (create → album → import reference/copy → colour
//! settings → open → edit → sidecar save → reopen → export), plus graceful failures.

use std::path::PathBuf;

use photocraft_cms::{Builtin, Profile};
use photocraft_color::{Color, ColorMode, SampleType};
use photocraft_doc::{Document, Size};
use serde_json::{Value, json};

use crate::Session;

/// A temp folder removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("pv-project-{tag}-{}", std::process::id()));
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

fn norm(p: &str) -> String {
    p.replace('\\', "/")
}

fn ok(s: &mut Session, id: &str, p: Value) -> Value {
    s.execute(id, p.clone()).unwrap_or_else(|e| panic!("{id} {p}: {e}"))
}

fn embedded_profile(path: &str) -> Profile {
    let bytes = std::fs::read(path).unwrap();
    let img = photocraft_codecs::decode(&bytes).unwrap();
    Profile::parse(img.icc.as_deref().expect("export embeds the output profile")).unwrap()
}

#[test]
fn project_workflow_end_to_end() {
    let t = TempDir::new("e2e");
    let pics = t.path("pics");
    std::fs::create_dir_all(&pics).unwrap();
    let a_jpg = format!("{pics}/a.jpg");
    let b_png = format!("{pics}/b.png");
    write_image(&a_jpg, [0.8, 0.4, 0.2]);
    write_image(&b_png, [0.1, 0.5, 0.9]);
    std::fs::write(format!("{pics}/notes.txt"), "x").unwrap();
    let proj = t.path("work/MyShoot.pvproj");
    std::fs::create_dir_all(t.path("work")).unwrap();

    let mut s = Session::new();
    let info = ok(&mut s, "project.new", json!({"path": t.path("work/MyShoot"), "name": "My Shoot"}));
    assert_eq!(info["name"], "My Shoot");
    assert!(std::path::Path::new(&proj).is_file(), "project.new writes the file");
    assert!(s.execute("project.new", json!({"path": proj})).is_err(), "refuses to overwrite");

    let album = ok(&mut s, "album.new", json!({"name": "Day 1"}))["id"].as_u64().unwrap();
    // Reference in place.
    let r = ok(
        &mut s,
        "album.import",
        json!({"album": album, "mode": "reference", "paths": [a_jpg, format!("{pics}/notes.txt"), format!("{pics}/missing.jpg"), 5]}),
    );
    assert_eq!(r["imported"], 1, "{r}");
    assert_eq!(r["skipped"], 3, "{r}");
    let a_id = r["results"][0]["id"].as_u64().unwrap();
    let r = ok(&mut s, "album.import", json!({"album": album, "mode": "reference", "paths": [a_jpg]}));
    assert_eq!(r["skipped"], 1, "duplicate: {r}");
    // Copy into the project.
    let r = ok(&mut s, "album.import", json!({"album": album, "mode": "copy", "paths": [b_png]}));
    assert_eq!(r["imported"], 1, "{r}");
    let b_id = r["results"][0]["id"].as_u64().unwrap();
    assert_eq!(r["results"][0]["stored"], "MyShoot Media/Day 1/b.png");
    assert!(std::path::Path::new(&t.path("work/MyShoot Media/Day 1/b.png")).is_file());
    let r = ok(&mut s, "album.import", json!({"album": album, "mode": "copy", "paths": [b_png]}));
    assert_eq!(r["skipped"], 1, "same file copied twice is a duplicate: {r}");

    // Colour: project Photo space ACEScg, album Output Rec.709.
    ok(&mut s, "project.setColor", json!({"level": "project", "field": "working", "value": "acescg"}));
    ok(&mut s, "project.setColor", json!({"level": "album", "id": album, "field": "output", "value": "rec709-bt1886"}));
    let info = ok(&mut s, "project.info", json!({}));
    assert_eq!(info["dirty"], true);
    let photo = &info["albums"][0]["photos"][0];
    assert_eq!(photo["resolvedColor"]["working"], "acescg");
    assert_eq!(photo["resolvedColor"]["output"], "rec709-bt1886");
    assert_eq!(photo["exists"], true);
    assert_eq!(photo["hasSidecar"], false);
    assert_eq!(info["albums"][0]["photos"][1]["managed"], true);
    ok(&mut s, "project.save", json!({}));

    // Open: F32 ACEScg with the album's output.
    let r = ok(&mut s, "photo.open", json!({"id": a_id}));
    assert_eq!(r["fromSidecar"], false);
    let i = r["document"].as_u64().unwrap() as usize;
    let d = &s.documents()[i];
    assert_eq!(d.project_photo, Some(a_id));
    assert_eq!(d.doc.depth, SampleType::F32);
    assert!(crate::color_cmds::document_profile(&d.doc).same_colors(Builtin::AcesCg.profile()));
    assert_eq!(s.doc_pipeline(i).unwrap().pipeline.output.as_str(), "rec709-bt1886");
    assert_eq!(ok(&mut s, "photo.open", json!({"id": a_id}))["alreadyOpen"], true);
    assert_eq!(norm(&s.active_photo_sidecar().unwrap()), norm(&format!("{a_jpg}.pvision")));

    // Edit, save the sidecar; the original is untouched.
    let before = std::fs::read(&a_jpg).unwrap();
    ok(&mut s, "layer.new.layer", json!({}));
    assert!(s.active().unwrap().is_dirty());
    let r = ok(&mut s, "photo.save", json!({}));
    assert_eq!(norm(r["path"].as_str().unwrap()), norm(&format!("{a_jpg}.pvision")));
    assert!(!s.active().unwrap().is_dirty());
    assert_eq!(std::fs::read(&a_jpg).unwrap(), before);
    let layers = s.active().unwrap().doc.layers.len();

    // Output change on an open photo: no conversion. Working change: converts.
    let r = ok(&mut s, "project.setColor", json!({"level": "photo", "id": a_id, "field": "output", "value": "display-p3"}));
    assert_eq!(r["documents"][0]["converted"], false, "{r}");
    assert_eq!(s.doc_pipeline(i).unwrap().pipeline.output.as_str(), "display-p3");
    let r = ok(&mut s, "project.setColor", json!({"level": "photo", "id": a_id, "field": "input", "value": "display-p3"}));
    assert_eq!(r["needsRebuild"], true, "{r}");
    ok(&mut s, "project.setColor", json!({"level": "photo", "id": a_id, "field": "input", "value": null}));
    ok(&mut s, "project.setColor", json!({"level": "photo", "id": a_id, "field": "output", "value": null}));

    // The sidecar is a native bundle any open path reads (by name and by content).
    let side = format!("{a_jpg}.pvision");
    let bytes = std::fs::read(&side).unwrap();
    assert_eq!(photocraft_io::import("IMG.jpg.pvision", &bytes).unwrap().document.layers.len(), layers);
    assert!(!std::path::Path::new(&format!("{a_jpg}.pcraft")).exists(), "no .pcraft sidecar");
    // Reopen from the sidecar.
    s.close(i);
    let r = ok(&mut s, "photo.open", json!({"id": a_id}));
    assert_eq!(r["fromSidecar"], true, "{r}");
    let d = s.active().unwrap();
    assert_eq!(d.doc.layers.len(), layers);
    assert_eq!(d.doc.depth, SampleType::F32);
    let info = ok(&mut s, "project.info", json!({}));
    assert_eq!(info["albums"][0]["photos"][0]["hasSidecar"], true);
    assert_eq!(info["albums"][0]["photos"][0]["document"], 0);

    // Album Photo space back to sRGB: the open sidecar converts.
    let r = ok(&mut s, "project.setColor", json!({"level": "album", "id": album, "field": "working", "value": "srgb"}));
    assert_eq!(r["documents"][0]["converted"], true, "{r}");
    assert!(crate::color_cmds::document_profile(&s.active().unwrap().doc).same_colors(Builtin::Srgb.profile()));
    ok(&mut s, "project.setColor", json!({"level": "album", "id": album, "field": "working", "value": null}));

    // Export: both photos, Rec.709 embedded, no overwrite the second time.
    let out = t.path("out");
    let r = ok(&mut s, "album.export", json!({"album": album, "folder": out, "format": "jpeg", "quality": 10}));
    assert_eq!(r["exported"], 2, "{r}");
    for name in ["a.jpg", "b.jpg"] {
        let p = embedded_profile(&format!("{out}/{name}"));
        assert!(p.same_colors(Builtin::Rec709Bt1886.profile()), "{name}: {}", p.description);
    }
    let r = ok(&mut s, "album.export", json!({"album": album, "folder": out, "format": "png"}));
    assert_eq!(r["exported"], 2, "{r}");
    let r = ok(&mut s, "album.export", json!({"album": album, "folder": out, "format": "png"}));
    assert_eq!(r["skipped"], 2, "{r}");
    assert_eq!(ok(&mut s, "album.export", json!({"album": album, "folder": out, "format": "png", "overwrite": true}))["exported"], 2);

    // Thumbnails are cached.
    let r = ok(&mut s, "photo.thumbnail", json!({"id": b_id, "maxSide": 32}));
    assert_eq!(r["cached"], false);
    let png = r["path"].as_str().unwrap().to_string();
    assert!(png.replace('\\', "/").contains("MyShoot.pvcache/thumbs/"), "{png}");
    let img = photocraft_codecs::decode(&std::fs::read(&png).unwrap()).unwrap();
    assert!(img.width() <= 32 && img.height() <= 32);
    assert_eq!(ok(&mut s, "photo.thumbnail", json!({"id": b_id, "maxSide": 32}))["cached"], true);
    assert_eq!(ok(&mut s, "photo.thumbnail", json!({"id": a_id}))["cached"], false, "sidecar thumbnail");

    // Relink, remove, rename, delete.
    let moved = format!("{pics}/moved.jpg");
    std::fs::copy(&a_jpg, &moved).unwrap();
    ok(&mut s, "photo.relink", json!({"id": a_id, "path": moved}));
    assert!(s.execute("photo.relink", json!({"id": a_id, "path": format!("{pics}/nope.jpg")})).is_err());
    ok(&mut s, "album.rename", json!({"id": album, "name": "Day One"}));
    ok(&mut s, "photo.remove", json!({"id": a_id}));
    assert_eq!(s.active().unwrap().project_photo, None, "removed photos unlink their documents");
    assert!(s.execute("photo.save", json!({})).is_err());

    // Round trip through the file.
    ok(&mut s, "project.save", json!({}));
    let saved = std::fs::read_to_string(&proj).unwrap();
    let p = photocraft_project::Project::from_json(&saved).unwrap();
    assert_eq!(p.albums[0].name, "Day One");
    assert_eq!(p.albums[0].photos.len(), 1);
    assert_eq!(p.color.working.as_ref().unwrap().as_str(), "acescg");
    ok(&mut s, "album.delete", json!({"id": album}));
    assert!(s.execute("project.close", json!({})).is_err(), "unsaved changes");
    ok(&mut s, "project.close", json!({"discard": true}));
    let info = ok(&mut s, "project.open", json!({"path": proj}));
    assert_eq!(info["albums"][0]["name"], "Day One");
    assert_eq!(info["dirty"], false);
    ok(&mut s, "project.close", json!({}));
}

#[test]
fn commands_fail_gracefully_without_a_project() {
    let mut s = Session::new();
    for (id, p) in [
        ("project.save", json!({})),
        ("project.close", json!({})),
        ("project.info", json!({})),
        ("project.setColor", json!({"level": "project", "field": "working", "value": "srgb"})),
        ("album.new", json!({"name": "A"})),
        ("album.rename", json!({"id": 1, "name": "A"})),
        ("album.delete", json!({"id": 1})),
        ("album.import", json!({"album": 1, "mode": "reference", "paths": []})),
        ("album.export", json!({"album": 1, "folder": "x", "format": "png"})),
        ("photo.remove", json!({"id": 1})),
        ("photo.relink", json!({"id": 1, "path": "x.jpg"})),
        ("photo.open", json!({"id": 1})),
        ("photo.save", json!({})),
        ("photo.thumbnail", json!({"id": 1})),
        ("project.open", json!({})),
        ("project.open", json!({"path": 5})),
        ("project.new", json!({})),
        ("project.new", json!({"path": ""})),
    ] {
        assert!(s.execute(id, p.clone()).is_err(), "{id} {p} should fail");
    }
}

#[test]
fn commands_reject_bad_params() {
    let t = TempDir::new("bad");
    let mut s = Session::new();
    // Not a project file / missing file.
    let junk = t.path("junk.pvproj");
    std::fs::write(&junk, b"\xff\xfe not json").unwrap();
    assert!(s.execute("project.open", json!({"path": junk})).is_err());
    std::fs::write(&junk, br#"{"version": 99, "name": "x"}"#).unwrap();
    assert!(s.execute("project.open", json!({"path": junk})).is_err());
    assert!(s.execute("project.open", json!({"path": t.path("missing.pvproj")})).is_err());
    assert!(s.project.is_none());

    ok(&mut s, "project.new", json!({"path": t.path("p.pvproj")}));
    assert!(s.execute("project.new", json!({"path": t.path("p.pvproj"), "overwrite": "yes"})).is_err());
    assert!(s.execute("project.new", json!({"path": t.path("q.pvproj"), "name": 3})).is_err());
    let album = ok(&mut s, "album.new", json!({"name": "A"}))["id"].as_u64().unwrap();
    // A dirty project refuses to be replaced without discard.
    assert!(s.execute("project.new", json!({"path": t.path("q.pvproj")})).is_err());
    for (id, p) in [
        ("album.new", json!({})),
        ("album.new", json!({"name": ""})),
        ("album.new", json!({"name": "a"})),
        ("album.new", json!({"name": 7})),
        ("album.rename", json!({"id": 999, "name": "B"})),
        ("album.rename", json!({"id": "1", "name": "B"})),
        ("album.rename", json!({"id": -1, "name": "B"})),
        ("album.rename", json!({"id": 1.5, "name": "B"})),
        ("album.delete", json!({"id": 999})),
        ("album.delete", json!({})),
        ("album.import", json!({"album": album, "mode": "move", "paths": []})),
        ("album.import", json!({"album": album, "mode": "copy"})),
        ("album.import", json!({"album": album, "mode": "copy", "paths": "x.jpg"})),
        ("album.import", json!({"album": 999, "mode": "copy", "paths": []})),
        ("album.import", json!({"mode": "copy", "paths": []})),
        ("album.export", json!({"album": album, "folder": t.path("o"), "format": "gif"})),
        ("album.export", json!({"album": album, "folder": t.path("o"), "format": "png", "quality": "high"})),
        ("album.export", json!({"album": 999, "folder": t.path("o"), "format": "png"})),
        ("album.export", json!({"album": album, "format": "png"})),
        ("project.setColor", json!({"level": "galaxy", "field": "working", "value": "srgb"})),
        ("project.setColor", json!({"level": "album", "field": "working", "value": "srgb"})),
        ("project.setColor", json!({"level": "album", "id": 999, "field": "working", "value": "srgb"})),
        ("project.setColor", json!({"level": "photo", "id": 999, "field": "working", "value": "srgb"})),
        ("project.setColor", json!({"level": "project", "field": "gamma", "value": "srgb"})),
        ("project.setColor", json!({"level": "project", "field": "working"})),
        ("project.setColor", json!({"level": "project", "field": "working", "value": "no-such-space"})),
        ("project.setColor", json!({"level": "project", "field": "working", "value": 3})),
        ("project.setColor", json!({"level": "project", "field": "bpc", "value": "yes"})),
        ("project.setColor", json!({"level": "project", "field": "intent", "value": "sideways"})),
        ("project.setColor", json!({"level": "project", "field": "output", "value": "icc:/no/such/profile.icc"})),
        ("project.setColor", json!({"level": "project", "field": "output", "value": "lab-d50"})),
        ("photo.open", json!({"id": 999})),
        ("photo.open", json!({})),
        ("photo.remove", json!({"id": 999})),
        ("photo.relink", json!({"id": 999, "path": "x.jpg"})),
        ("photo.thumbnail", json!({"id": 999})),
        ("photo.thumbnail", json!({"id": album, "maxSide": "big"})),
        ("photo.save", json!({})),
    ] {
        assert!(s.execute(id, p.clone()).is_err(), "{id} {p} should fail");
    }
    // A missing original fails to open, thumbnail and export (reported per file), not panics.
    let gone = t.path("gone.jpg");
    write_image(&gone, [0.5, 0.5, 0.5]);
    let r = ok(&mut s, "album.import", json!({"album": album, "mode": "reference", "paths": [gone]}));
    let id = r["results"][0]["id"].as_u64().unwrap();
    std::fs::remove_file(&gone).unwrap();
    assert!(s.execute("photo.open", json!({"id": id})).is_err());
    assert!(s.execute("photo.thumbnail", json!({"id": id})).is_err());
    let r = ok(&mut s, "album.export", json!({"album": album, "folder": t.path("o"), "format": "png"}));
    assert_eq!(r["failed"], 1, "{r}");
    // Inherit (null) is accepted.
    ok(&mut s, "project.setColor", json!({"level": "project", "field": "working", "value": null}));
    ok(&mut s, "project.setColor", json!({"level": "project", "field": "bpc", "value": false}));
    ok(&mut s, "project.close", json!({"discard": true}));
}

/// The centre pixel of a cached thumbnail PNG.
fn thumb_centre(path: &str) -> [u8; 4] {
    let img = photocraft_codecs::decode(&std::fs::read(path).unwrap()).unwrap();
    let rgba = img.to_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let i = ((h / 2) * w + w / 2) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

#[test]
fn thumbnails_of_linear_sidecars_are_converted_for_display() {
    let t = TempDir::new("thumb");
    let src = t.path("grey.png");
    write_image(&src, [0.5, 0.5, 0.5]);
    let mut s = Session::new();
    ok(&mut s, "project.new", json!({"path": t.path("p.pvproj")}));
    let album = ok(&mut s, "album.new", json!({"name": "A"}))["id"].as_u64().unwrap();
    let id = ok(&mut s, "album.import", json!({"album": album, "mode": "reference", "paths": [src]}))["results"][0]["id"].as_u64().unwrap();
    let plain = ok(&mut s, "photo.thumbnail", json!({"id": id, "maxSide": 32}));
    let c = thumb_centre(plain["path"].as_str().unwrap());
    assert!((c[0] as i32 - 128).abs() <= 3, "original thumbnail {c:?}");
    // A linear ACEScg sidecar: its stored thumbnail is linear, the cached one must not be dark.
    ok(&mut s, "project.setColor", json!({"level": "project", "field": "working", "value": "acescg"}));
    ok(&mut s, "photo.open", json!({"id": id}));
    ok(&mut s, "photo.save", json!({}));
    let r = ok(&mut s, "photo.thumbnail", json!({"id": id, "maxSide": 32}));
    assert_eq!(r["cached"], false, "a new sidecar gets a new thumbnail");
    let c = thumb_centre(r["path"].as_str().unwrap());
    assert!((c[0] as i32 - 128).abs() <= 6 && (c[2] as i32 - 128).abs() <= 6, "sidecar thumbnail {c:?} (dark = not converted)");
    // The plan API used by the Library's worker thread agrees.
    let plan = crate::project_cmds::thumbnail_plan(&s, id, 32).unwrap();
    assert!(plan.cached);
    let img = plan.load().unwrap();
    assert!(img.width <= 32 && img.height <= 32);
    ok(&mut s, "project.close", json!({"discard": true}));
}

#[test]
fn rebuild_rereads_the_original_and_keeps_the_layers_above() {
    let t = TempDir::new("rebuild");
    let src = t.path("a.png");
    write_image(&src, [0.8, 0.4, 0.2]);
    let mut s = Session::new();
    ok(&mut s, "project.new", json!({"path": t.path("p.pvproj")}));
    let album = ok(&mut s, "album.new", json!({"name": "A"}))["id"].as_u64().unwrap();
    let id = ok(&mut s, "album.import", json!({"album": album, "mode": "reference", "paths": [src]}))["results"][0]["id"].as_u64().unwrap();
    // Not open yet: a clear error.
    assert!(s.execute("photo.rebuild", json!({"id": id})).is_err());
    ok(&mut s, "photo.open", json!({"id": id}));
    ok(&mut s, "layer.new.layer", json!({}));
    let layers = s.active().unwrap().doc.layers.len();
    let r = ok(&mut s, "project.setColor", json!({"level": "photo", "id": id, "field": "input", "value": "display-p3"}));
    assert_eq!(r["needsRebuild"], true, "{r}");
    let before = s.active().unwrap().doc.clone();
    let r = ok(&mut s, "photo.rebuild", json!({"id": id}));
    assert_eq!(r["pipeline"]["input"], "display-p3", "{r}");
    let d = s.active().unwrap();
    assert_eq!(d.doc.layers.len(), layers, "layers above the bottom one are kept");
    assert_eq!(s.doc_pipeline(0).unwrap().pipeline.input.as_str(), "display-p3");
    assert!(!std::sync::Arc::ptr_eq(&before, &d.doc));
    // The canvas changed size: refused, nothing changes.
    ok(&mut s, "image.canvasSize", json!({"width": 20, "height": 20}));
    let rev = s.active().unwrap().revision;
    assert!(s.execute("photo.rebuild", json!({"id": id})).is_err());
    assert_eq!(s.active().unwrap().revision, rev);
    for p in [json!({}), json!({"id": "x"}), json!({"id": -3}), json!({"id": 999})] {
        assert!(s.execute("photo.rebuild", p.clone()).is_err(), "{p}");
    }
    ok(&mut s, "project.close", json!({"discard": true}));
    assert!(s.execute("photo.rebuild", json!({"id": id})).is_err(), "no project");
}

#[test]
fn sidecars_are_pvision_files_and_never_imported() {
    let t = TempDir::new("pvision");
    let src = t.path("a.png");
    write_image(&src, [0.2, 0.6, 0.4]);
    let mut s = Session::new();
    ok(&mut s, "project.new", json!({"path": t.path("p.pvproj")}));
    let album = ok(&mut s, "album.new", json!({"name": "A"}))["id"].as_u64().unwrap();
    let id = ok(&mut s, "album.import", json!({"album": album, "mode": "reference", "paths": [src]}))["results"][0]["id"].as_u64().unwrap();
    ok(&mut s, "photo.open", json!({"id": id}));
    ok(&mut s, "layer.new.layer", json!({}));
    let r = ok(&mut s, "photo.save", json!({}));
    let side = r["path"].as_str().unwrap().to_string();
    assert!(side.ends_with("a.png.pvision"), "{side}");
    assert!(crate::file_cmds::saves_in_place(&side));
    // Edits are not photos: .pvision and .pcraft files are skipped as sidecars.
    let doc = t.path("other.pcraft");
    std::fs::copy(&side, &doc).unwrap();
    let r = ok(&mut s, "album.import", json!({"album": album, "mode": "reference", "paths": [side, doc]}));
    assert_eq!(r["skipped"], 2, "{r}");
    assert!(r["results"].as_array().unwrap().iter().all(|x| x["reason"] == "sidecar"), "{r}");
    // A legacy `.pcraft` sidecar is still read when there is no `.pvision`.
    let i = s.photo_document(id).unwrap();
    s.close(i);
    std::fs::rename(t.path("a.png.pvision"), t.path("a.png.pcraft")).unwrap();
    let r = ok(&mut s, "photo.open", json!({"id": id}));
    assert_eq!(r["fromSidecar"], true, "{r}");
    assert_eq!(s.active().unwrap().doc.layers.len(), 2);
    ok(&mut s, "project.close", json!({"discard": true}));
}
