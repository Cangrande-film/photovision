//! The Album Look in the shell: the Library inspector section, the Project › Album Look items, the
//! Layers panel badge and context menu entries.

use egui_kittest::{Harness, kittest::Queryable};
use serde_json::{Value, json};

use super::*;
use crate::library_ui::Focus;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("pv-lookui-{tag}-{}", std::process::id()));
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

fn write_png(path: &str) {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 24, "height": 16, "background": "white"})).unwrap();
    let doc = s.active().unwrap().doc.clone();
    let r = photocraft_io::export(&doc, path, &photocraft_io::ExportOptions::default()).unwrap();
    std::fs::write(path, r.bytes).unwrap();
}

/// A project with album "Day 1" holding photos a and b; photo a is open with an Invert layer in
/// the album look. Returns the app, the album and the two photo ids.
fn app_with_look(t: &TempDir) -> (PhotocraftApp, u64, u64, u64) {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.run("project.new", json!({"path": t.path("Shoot.pvproj")})).unwrap();
    let album = app.run("album.new", json!({"name": "Day 1"})).unwrap()["id"].as_u64().unwrap();
    let (a, b) = (t.path("a.png"), t.path("b.png"));
    write_png(&a);
    write_png(&b);
    let r = app.run("album.import", json!({"album": album, "mode": "reference", "paths": [a, b]})).unwrap();
    let ids: Vec<u64> = r["results"].as_array().unwrap().iter().map(|x| x["id"].as_u64().unwrap()).collect();
    app.run("photo.open", json!({"id": ids[0]})).unwrap();
    app.run("layer.newAdjustmentLayer.invert", json!({})).unwrap();
    app.run("layer.toAlbumLook", json!({})).unwrap();
    (app, album, ids[0], ids[1])
}

fn harness(app: PhotocraftApp, f: impl FnMut(&mut egui::Ui, &mut PhotocraftApp) + 'static) -> Harness<'static, PhotocraftApp> {
    let h = Harness::builder().with_size(egui::vec2(1200.0, 900.0)).build_ui_state(f, app);
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ALL[0]);
    h
}

fn look_enabled(app: &PhotocraftApp, album: u64) -> bool {
    app.session.project.as_ref().unwrap().project.album(album).unwrap().look_enabled
}

fn uses_look(app: &PhotocraftApp, photo: u64) -> bool {
    app.session.project.as_ref().unwrap().project.find_photo(photo).unwrap().1.album_look
}

#[test]
fn the_library_inspector_shows_and_drives_the_album_look() {
    let t = TempDir::new("inspector");
    let (mut app, album, _a, b) = app_with_look(&t);
    app.ui.module = Module::Library;
    app.ui.library.album = Some(album);
    app.ui.library.focus = Focus::Album;
    // The theme's fonts load on the next frame: draw from the third one.
    let mut frame = 0;
    let mut h = harness(app, move |ui, app| {
        frame += 1;
        if frame > 4 {
            crate::library_ui::view(app, ui);
        }
    });
    h.run_steps(8);
    assert!(h.query_by_label("Album Look").is_some(), "the album level has an Album Look section");
    assert!(h.query_by_label("Invert 1").is_some(), "it lists the look's layers");
    assert!(h.query_by_label("Applied to every photo in Day 1, after each photo's own edits.").is_some());
    // The switch turns the look off for the album.
    h.get_by_label("Enabled").click();
    h.run_steps(3);
    assert!(!look_enabled(h.state(), album));
    h.get_by_label("Enabled").click();
    h.run_steps(3);
    assert!(look_enabled(h.state(), album));
    // Clear asks first.
    h.get_by_label("Clear").click();
    h.run_steps(2);
    let d = h.state().ui.dialogs.iter().find(|d| d.fields.get("__library") == Some(&json!("clearLook"))).map(|d| d.id).expect("a confirmation");
    crate::dialogs::confirm(h.state_mut(), d).unwrap();
    h.run_steps(3);
    assert!(h.state().session.project.as_ref().unwrap().looks.get(&album).unwrap().is_empty());
    assert!(h.query_by_label("Invert 1").is_none());

    // The photo level: Use album look.
    h.state_mut().ui.library.photos = vec![b];
    h.state_mut().ui.library.focus = Focus::Photos;
    h.run_steps(3);
    assert!(h.query_by_label("Use album look").is_some());
    h.get_by_label("Use album look").click();
    h.run_steps(3);
    assert!(!uses_look(h.state(), b), "unchecked: photo b bypasses the look");
    // Its tile says so.
    let info = crate::library_ui::cached_info(h.state_mut()).unwrap();
    let p = &info["albums"][0]["photos"][1];
    assert_eq!(p["albumLook"], false);
}

#[test]
fn project_album_look_menu_items_target_the_active_photo_or_the_library_selection() {
    let t = TempDir::new("menu");
    let (mut app, album, a, b) = app_with_look(&t);
    let ctx = egui::Context::default();
    let items = crate::menus::menu_items(&app);
    let look: Vec<&str> = items.iter().filter(|i| i.path == ["Project", "Album Look"]).map(|i| i.id.as_str()).collect();
    assert_eq!(look, ["album.look.setEnabled", "photo.setAlbumLook", "layer.toAlbumLook", "layer.fromAlbumLook", "album.look.clear"]);
    // Edit module: the active photo and its album.
    app.ui.module = Module::Edit;
    assert_eq!(target_album(&app), Some(album));
    assert_eq!(target_photo(&app), Some(a));
    assert!(crate::menus::is_enabled(&app, "album.look.setEnabled"));
    assert!(crate::menus::is_enabled(&app, "album.look.clear"));
    assert_eq!(checked(&app, "album.look.setEnabled"), Some(true));
    crate::menus::invoke(&mut app, &ctx, "album.look.setEnabled", json!({})).unwrap();
    assert!(!look_enabled(&app, album), "the item toggles");
    assert_eq!(checked(&app, "album.look.setEnabled"), Some(false));
    crate::menus::invoke(&mut app, &ctx, "album.look.setEnabled", Value::Null).unwrap();
    assert!(look_enabled(&app, album));
    crate::menus::invoke(&mut app, &ctx, "photo.setAlbumLook", json!({})).unwrap();
    assert!(!uses_look(&app, a));
    assert_eq!(checked(&app, "photo.setAlbumLook"), Some(false));
    crate::menus::invoke(&mut app, &ctx, "photo.setAlbumLook", json!({})).unwrap();
    assert!(uses_look(&app, a));
    // The look's layer is targeted after Move to Album Look: Copy from Album Look is live.
    assert!(crate::menus::is_enabled(&app, "layer.fromAlbumLook"));
    assert!(!crate::menus::is_enabled(&app, "layer.toAlbumLook"));
    // Library module: the shown album and the one selected photo.
    app.ui.module = Module::Library;
    app.ui.library.album = Some(album);
    app.ui.library.photos = vec![a, b];
    assert_eq!(target_photo(&app), None, "two photos: no single target");
    assert!(!crate::menus::is_enabled(&app, "photo.setAlbumLook"));
    app.ui.library.photos = vec![b];
    crate::menus::invoke(&mut app, &ctx, "photo.setAlbumLook", json!({})).unwrap();
    assert!(!uses_look(&app, b));
    // Clear asks first.
    let d = crate::menus::invoke(&mut app, &ctx, "album.look.clear", json!({})).unwrap()["dialog"].as_u64().unwrap();
    crate::dialogs::confirm(&mut app, d).unwrap();
    assert!(!crate::menus::is_enabled(&app, "album.look.clear"), "nothing left to clear");
    // Without a project nothing is enabled and the items fail with a message.
    app.run("project.close", json!({"discard": true})).unwrap();
    assert!(!crate::menus::is_enabled(&app, "album.look.setEnabled"));
    assert!(crate::menus::invoke(&mut app, &ctx, "album.look.setEnabled", json!({})).is_err());
    assert!(crate::menus::invoke(&mut app, &ctx, "photo.setAlbumLook", json!({})).is_err());
}

#[test]
fn the_layers_panel_badges_the_album_look_group() {
    let t = TempDir::new("badge");
    let (mut app, album, _a, _b) = app_with_look(&t);
    let gid = app.session.album_look_group(0).unwrap();
    let tip = group_tooltip(&app, gid).unwrap();
    assert!(tip.starts_with("Shared by all 2 photos in Day 1"), "{tip}");
    let own = app.session.active().unwrap().doc.layers[0].id;
    assert!(group_tooltip(&app, own).is_none());
    // The context menu offers Move to / Copy from Album Look by where the layer is.
    let doc = app.session.active().unwrap().doc.clone();
    let inside = doc.layer(gid).unwrap().children().unwrap()[0].clone();
    let ids = |v: Vec<crate::layer_menu_ui::Entry>| v.into_iter().flatten().map(|e| e.1).collect::<Vec<_>>();
    assert_eq!(ids(crate::layer_menu_ui::album_look_entries(&inside, Some(gid), true)), ["layer.fromAlbumLook"]);
    assert_eq!(ids(crate::layer_menu_ui::album_look_entries(&doc.layers[0], Some(gid), false)), ["layer.toAlbumLook"]);
    assert!(crate::layer_menu_ui::album_look_entries(doc.layer(gid).unwrap(), Some(gid), false).is_empty());
    assert!(crate::layer_menu_ui::album_look_entries(&doc.layers[0], None, false).is_empty());
    let _ = album;
    // The full window: the group's row carries the badge, clear of its name; other rows don't.
    let session = std::mem::take(&mut app.session);
    let mut h = Harness::builder().with_size(egui::vec2(1440.0, 900.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(session, crate::Services::default())
    });
    h.run_steps(8);
    let rows = crate::layer_row_ui::recorded(&h.ctx);
    let row = rows.iter().find(|r| r.layer == gid.0).expect("the look group's row is drawn");
    let badge = row.indicators.iter().find(|(k, _)| *k == crate::layer_row_ui::Indicator::AlbumLook).map(|(_, r)| *r).expect("the badge");
    if let Some(name) = row.name {
        assert!(!name.intersects(badge) || name.intersect(badge).area() <= 0.0, "name {name:?} clear of the badge {badge:?}");
    }
    assert!(rows.iter().filter(|r| r.layer != gid.0).all(|r| r.indicators.iter().all(|(k, _)| *k != crate::layer_row_ui::Indicator::AlbumLook)));
}
