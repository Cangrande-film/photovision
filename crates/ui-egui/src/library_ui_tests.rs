use egui_kittest::{Harness, kittest::Queryable};
use serde_json::{Map, Value, json};

use super::*;
use crate::PhotocraftApp;

/// A temp folder removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("pv-library-{tag}-{}", std::process::id()));
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

fn app() -> PhotocraftApp {
    PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default())
}

/// An app with a project holding one album (`A`), and that album's id.
fn app_with_project(t: &TempDir) -> (PhotocraftApp, u64) {
    let mut app = app();
    app.run("project.new", json!({"path": t.path("Shoot.pvproj")})).unwrap();
    let album = app.run("album.new", json!({"name": "A"})).unwrap()["id"].as_u64().unwrap();
    app.ui.module = Module::Library;
    (app, album)
}

fn write_png(path: &str) {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 24, "height": 16, "background": "white"})).unwrap();
    let doc = s.active().unwrap().doc.clone();
    let r = photocraft_io::export(&doc, path, &photocraft_io::ExportOptions::default()).unwrap();
    std::fs::write(path, r.bytes).unwrap();
}

fn harness(app: PhotocraftApp, f: impl FnMut(&mut egui::Ui, &mut PhotocraftApp) + 'static) -> Harness<'static, PhotocraftApp> {
    let h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_ui_state(f, app);
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ALL[0]);
    h
}

#[test]
fn the_module_switch_appears_only_with_a_project() {
    let mut h = harness(app(), |ui, app| {
        ui.horizontal(|ui| module_switch(app, ui));
    });
    h.run_steps(2);
    assert!(h.query_by_label("Library").is_none(), "no project, no switch");
    assert!(!active(h.state()));

    let t = TempDir::new("switch");
    let (mut app, _) = app_with_project(&t);
    app.ui.module = Module::Edit;
    let mut h = harness(app, |ui, app| {
        ui.horizontal(|ui| module_switch(app, ui));
    });
    h.run_steps(2);
    assert!(h.query_by_label("Edit").is_some());
    h.get_by_label("Library").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.module, Module::Library);
    assert!(active(h.state()));
    // Closing the project leaves the Library even though the switch still says so.
    h.state_mut().run("project.close", json!({"discard": true})).unwrap();
    assert!(!active(h.state()));
}

#[test]
fn inspector_dropdowns_offer_inherit_with_the_inherited_value() {
    let t = TempDir::new("inherit");
    let (mut app, album) = app_with_project(&t);
    app.run("project.setColor", json!({"level": "project", "field": "working", "value": "acescg"})).unwrap();
    let info = cached_info(&mut app).unwrap();
    let spaces = &info["spaces"];
    // Album level inherits the project's resolved pipeline.
    let opts = field_options(spaces, "working", Some(&info["resolvedColor"]), &Value::Null);
    assert_eq!(opts[0], (Value::Null, "Inherit (ACEScg)".to_string()));
    assert!(opts.iter().any(|(v, _)| v == "srgb"));
    let input = field_options(spaces, "input", Some(&info["resolvedColor"]), &Value::Null);
    assert_eq!(input[0].1, "Inherit (Auto (embedded profile))");
    assert_eq!(input[1].0, "auto");
    let bpc = field_options(spaces, "bpc", Some(&info["resolvedColor"]), &Value::Null);
    assert_eq!(bpc.iter().map(|o| o.1.as_str()).collect::<Vec<_>>(), ["Inherit (On)", "On", "Off"]);
    let intent = field_options(spaces, "intent", Some(&info["resolvedColor"]), &Value::Null);
    assert_eq!(intent[0].1, "Inherit (Relative Colorimetric)");
    // Project level: no Inherit entry.
    let opts = field_options(spaces, "output", None, &json!("srgb"));
    assert_ne!(opts[0].0, Value::Null);
    // A custom profile in use stays selectable.
    let opts = field_options(spaces, "output", None, &json!("icc:C:/profiles/Paper.icc"));
    assert_eq!(opts.last().unwrap().1, "Paper.icc");
    // The labels shown for the inspector's target.
    app.ui.library.album = Some(album);
    app.ui.library.focus = Focus::Album;
    assert_eq!(app.ui.library.target(), Target::Album(album));
}

#[test]
fn choosing_inherit_calls_set_color_with_null() {
    assert_eq!(set_color_params(&Target::Album(3), "working", &Value::Null), vec![json!({"level": "album", "id": 3, "field": "working", "value": null})]);
    assert_eq!(set_color_params(&Target::Photos(vec![4, 5]), "bpc", &json!(false)).len(), 2);
    assert_eq!(set_color_params(&Target::Project, "output", &json!("display-p3"))[0]["level"], "project");

    let t = TempDir::new("null");
    let (mut app, album) = app_with_project(&t);
    apply_color(&mut app, &Target::Album(album), "working", &json!("acescg")).unwrap();
    let info = cached_info(&mut app).unwrap();
    assert_eq!(info["albums"][0]["color"]["working"], "acescg");
    apply_color(&mut app, &Target::Album(album), "working", &Value::Null).unwrap();
    let info = cached_info(&mut app).unwrap();
    assert!(info["albums"][0]["color"].get("working").is_none(), "Inherit clears the album's override: {info}");
}

#[test]
fn the_import_dialog_builds_album_import_params() {
    let mut f = Map::new();
    f.insert("__library".into(), json!("import"));
    f.insert("album".into(), json!(7));
    f.insert("paths".into(), json!(["C:/a.jpg", "C:/b.png"]));
    f.insert("mode".into(), json!("copy"));
    assert_eq!(import_params(&f).unwrap(), json!({"album": 7, "paths": ["C:/a.jpg", "C:/b.png"], "mode": "copy"}));
    f.insert("mode".into(), json!("anything else"));
    assert_eq!(import_params(&f).unwrap()["mode"], "reference");
    f.insert("paths".into(), json!([]));
    assert!(import_params(&f).is_err());
    f.remove("album");
    assert!(import_params(&f).is_err());

    // End to end: the dialog imports into the album and reports skipped files.
    let t = TempDir::new("import");
    let (mut app, album) = app_with_project(&t);
    let good = t.path("good.png");
    write_png(&good);
    let id = open_import(&mut app, vec![good.clone(), t.path("missing.jpg")]);
    let fields = app.ui.dialogs.iter().find(|d| d.id == id).unwrap().fields.clone();
    assert_eq!(fields["album"], album);
    assert_eq!(fields["mode"], "reference");
    let r = crate::dialogs::confirm(&mut app, id).unwrap();
    assert_eq!(r["imported"], 1, "{r}");
    assert_eq!(app.ui.notices.last().unwrap().lines.len(), 1, "the missing file is reported");
    assert!(app.ui.notices.last().unwrap().lines[0].starts_with("missing.jpg"));
}

#[test]
fn the_library_shows_empty_states_and_the_grid() {
    let t = TempDir::new("empty");
    // Without a project the view draws nothing (the theme's fonts load on the next frame).
    let mut h = harness(app(), |ui, app| view(app, ui));
    h.run_steps(2);
    h.state_mut().run("project.new", json!({"path": t.path("Shoot.pvproj")})).unwrap();
    h.state_mut().ui.module = Module::Library;
    h.run_steps(3);
    assert!(h.query_by_label("Create an album to start").is_some());
    assert!(h.query_all_by_label("New Album…").count() >= 1);
    // The inspector edits the project.
    assert!(h.query_by_label("Color Management").is_some());
    assert!(h.query_by_label("Input color space").is_some());
    assert!(h.query_by_label("Photo color space").is_some());
    assert!(h.query_by_label("Output color space").is_some());

    h.state_mut().run("album.new", json!({"name": "Day 1"})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("No photos in this album yet").is_some());
    assert!(h.query_all_by_label("Import Photos…").count() >= 1);

    let png = t.path("one.png");
    write_png(&png);
    let album = h.state().ui.library.album.unwrap();
    h.state_mut().run("album.import", json!({"album": album, "paths": [png], "mode": "reference"})).unwrap();
    h.run_steps(4);
    assert!(h.query_by_label("No photos in this album yet").is_none());
    assert!(h.state().library.textures.len() == 1, "the thumbnail loaded");
}

#[test]
fn file_project_menu_items_and_dialogs() {
    let items = crate::menus::menu_items(&app());
    let project: Vec<&str> = items.iter().filter(|i| i.path == ["Project"]).map(|i| i.id.as_str()).collect();
    assert_eq!(
        project,
        [
            "project.new",
            "project.open",
            "project.save",
            "project.close",
            "project.settings",
            "album.new",
            "album.import",
            "album.export",
            "album.rename",
            "album.delete"
        ]
    );
    // Without a project only New/Open are live, and they need the desktop's file dialogs here.
    let t = TempDir::new("menus");
    let mut a = app();
    let ctx = egui::Context::default();
    assert!(crate::menus::invoke(&mut a, &ctx, "project.new", json!({})).is_err());
    assert!(!crate::menus::is_enabled(&a, "project.settings"));
    // With a pick_paths service the menu creates the project and shows the Library.
    let path = t.path("Picked.pvproj");
    a.services.pick_paths = Some(Box::new(move |_| vec![path.clone()]));
    crate::menus::invoke(&mut a, &ctx, "project.new", json!({})).unwrap();
    assert!(a.session.project.is_some());
    assert!(active(&a));
    // New Album… opens a dialog; OK creates and selects the album.
    let d = crate::menus::invoke(&mut a, &ctx, "album.new", json!({})).unwrap()["dialog"].as_u64().unwrap();
    crate::dialogs::confirm(&mut a, d).unwrap();
    let album = a.ui.library.album.unwrap();
    assert!(crate::menus::is_enabled(&a, "album.rename"));
    let d = crate::menus::invoke(&mut a, &ctx, "album.rename", json!({})).unwrap()["dialog"].as_u64().unwrap();
    a.ui.dialog_mut(d).unwrap().fields.insert("name".into(), json!("Renamed"));
    crate::dialogs::confirm(&mut a, d).unwrap();
    assert_eq!(a.session.project.as_ref().unwrap().project.albums[0].name, "Renamed");
    // Project Settings… stages changes and applies them on OK.
    let d = crate::menus::invoke(&mut a, &ctx, "project.settings", json!({})).unwrap()["dialog"].as_u64().unwrap();
    a.ui.dialog_mut(d).unwrap().fields.insert("working".into(), json!("acescg"));
    crate::dialogs::confirm(&mut a, d).unwrap();
    assert_eq!(a.session.project.as_ref().unwrap().project.color.working.as_ref().unwrap().as_str(), "acescg");
    // Delete Album… asks, then deletes.
    let d = crate::menus::invoke(&mut a, &ctx, "album.delete", json!({})).unwrap()["dialog"].as_u64().unwrap();
    assert_eq!(a.ui.dialogs.iter().find(|x| x.id == d).unwrap().fields["album"], album);
    crate::dialogs::confirm(&mut a, d).unwrap();
    assert!(a.session.project.as_ref().unwrap().project.albums.is_empty());
    assert!(!crate::menus::is_enabled(&a, "album.delete"));
}

#[test]
fn dropped_files_open_the_import_dialog_only_in_the_library() {
    #[derive(Debug)]
    struct Dropped(std::path::PathBuf);
    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }
    let t = TempDir::new("drop");
    let (mut app, _) = app_with_project(&t);
    let files: Vec<egui::DroppedFileHandle> = vec![std::sync::Arc::new(Dropped(t.0.join("x.jpg")))];
    app.ui.module = Module::Edit;
    assert!(!take_drop(&mut app, &files), "the editor opens dropped files");
    app.ui.module = Module::Library;
    assert!(take_drop(&mut app, &files));
    assert_eq!(app.ui.dialogs.last().unwrap().fields["__library"], "import");
}

#[test]
fn rebuild_failures_become_a_notice() {
    let t = TempDir::new("rebuild");
    let (mut app, _) = app_with_project(&t);
    app.ui.library.rebuild = vec![999];
    rebuild(&mut app, &[999]);
    assert!(app.ui.library.rebuild.is_empty());
    assert!(app.ui.notices.last().unwrap().error);
}

#[test]
fn library_state_round_trips_through_the_control_channel() {
    let t = TempDir::new("control");
    let (mut app, album) = app_with_project(&t);
    let ctx = egui::Context::default();
    let (req, _rx) = crate::ControlRequest::new("ui.set", json!({"module": "edit", "library": {"album": album, "focus": "album"}}));
    let r = crate::control::handle(&mut app, &ctx, &req);
    assert!(matches!(r, crate::control::Outcome::Done(ref v) if v["ok"] == true));
    assert_eq!(app.ui.module, Module::Edit);
    assert_eq!(app.ui.library.target(), Target::Album(album));
    let (req, _rx) = crate::ControlRequest::new("ui.set", json!({"module": "sideways"}));
    assert!(matches!(crate::control::handle(&mut app, &ctx, &req), crate::control::Outcome::Done(ref v) if v["ok"] == false));
}

#[test]
fn file_open_reads_a_pvision_sidecar() {
    let t = TempDir::new("open-pvision");
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    s.execute("layer.new.layer", json!({})).unwrap();
    let doc = s.active().unwrap().doc.clone();
    let path = t.path("IMG_1.jpg.pvision");
    std::fs::write(&path, photocraft_io::export(&doc, &path, &photocraft_io::ExportOptions::default()).unwrap().bytes).unwrap();
    let services = crate::Services {
        import: Some(Box::new(|name: &str, bytes: &[u8]| photocraft_io::import(name, bytes).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string()))),
        ..Default::default()
    };
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
    app.open_path(&path).unwrap();
    assert_eq!(app.session.active().unwrap().doc.layers.len(), 2);
    assert!(photocraft_engine::file_cmds::saves_in_place(&path), "File › Save writes it back");
}
