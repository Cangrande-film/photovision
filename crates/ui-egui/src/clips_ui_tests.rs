use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use serde_json::json;

use super::*;
use crate::PhotocraftApp;

/// A temp folder removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("pv-clips-{tag}-{}", std::process::id()));
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

fn write_png(path: &str, w: u32, h: u32) {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": w, "height": h, "background": "white"})).unwrap();
    let doc = s.active().unwrap().doc.clone();
    let r = photocraft_io::export(&doc, path, &photocraft_io::ExportOptions::default()).unwrap();
    std::fs::write(path, r.bytes).unwrap();
}

/// An app in the Edit module with a project: album `A` holding a.png, b.png, c.png (one tall)
/// and an empty album `B`. Returns the app and the photo ids in album order.
fn app_with_album(t: &TempDir) -> (PhotocraftApp, Vec<u64>) {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.run("project.new", json!({"path": t.path("Shoot.pvproj")})).unwrap();
    let album = app.run("album.new", json!({"name": "A"})).unwrap()["id"].as_u64().unwrap();
    app.run("album.new", json!({"name": "B"})).unwrap();
    let paths: Vec<String> = ["a.png", "b.png", "c.png"].iter().map(|n| t.path(n)).collect();
    write_png(&paths[0], 24, 16);
    write_png(&paths[1], 24, 16);
    write_png(&paths[2], 12, 30);
    let r = app.run("album.import", json!({"album": album, "paths": paths, "mode": "reference"})).unwrap();
    let ids = r["results"].as_array().unwrap().iter().map(|x| x["id"].as_u64().unwrap()).collect();
    app.ui.module = Module::Edit;
    (app, ids)
}

fn shown(app: &PhotocraftApp) -> Option<u64> {
    app.session.active().and_then(|d| d.project_photo)
}

fn open_photos(app: &PhotocraftApp) -> Vec<u64> {
    app.session.documents().iter().filter_map(|d| d.project_photo).collect()
}

fn harness(app: PhotocraftApp) -> Harness<'static, PhotocraftApp> {
    let h = Harness::builder().with_size(egui::vec2(1200.0, 700.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            // The theme's named fonts take effect from the second pass (as in the app; kittest runs passes while building).
            if ui.ctx().cumulative_pass_nr() > 2 {
                bar(app, ui);
            }
            egui::CentralPanel::default().show(ui, |_| {});
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ALL[0]);
    h
}

#[test]
fn the_bar_shows_only_in_the_edit_module_with_a_project() {
    let app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    assert!(!visible(&app), "no project, no bar");
    let mut h = harness(app);
    h.run_steps(3);
    assert!(h.query_by_label("Clips").is_none());

    let t = TempDir::new("visible");
    let (mut app, ids) = app_with_album(&t);
    assert!(visible(&app), "a project with an album shows the bar even before a photo is open");
    go_to(&mut app, ids[1]).unwrap();
    assert!(visible(&app));
    let mut h = harness(app);
    h.run_steps(4);
    assert!(h.query_by_label("Clips").is_some());
    // One tile per photo of the album, the shown one selected.
    for name in ["a.png", "b.png", "c.png"] {
        assert!(h.query_by_label(name).is_some(), "{name} has a tile");
    }
    assert!(h.query_by_label("3 photos").is_some());
    assert!(h.get_by_label("b.png").accesskit_node().toggled() == Some(egui::accesskit::Toggled::True));
    assert!(h.get_by_label("a.png").accesskit_node().toggled() == Some(egui::accesskit::Toggled::False));

    // Hidden in the Library, in full-screen mode, and with Window › Clips off.
    h.state_mut().ui.module = Module::Library;
    assert!(!visible(h.state()));
    h.run_steps(2);
    assert!(h.query_by_label("Clips").is_none());
    h.state_mut().ui.module = Module::Edit;
    h.state_mut().ui.view.screen_mode = "fullScreen".into();
    assert!(!visible(h.state()));
    h.state_mut().ui.view.screen_mode = "standard".into();
    let ctx = egui::Context::default();
    crate::menus::invoke(h.state_mut(), &ctx, "window.toggle.clips", json!({})).unwrap();
    assert!(!visible(h.state()));
    assert!(crate::menus::menu_items(h.state()).iter().any(|i| i.id == "window.toggle.clips" && i.checked == Some(false)));
    crate::menus::invoke(h.state_mut(), &ctx, "window.toggle.clips", json!({})).unwrap();
    assert!(visible(h.state()));
    // Closing the project removes it.
    h.state_mut().run("project.close", json!({"discard": true})).unwrap();
    assert!(!visible(h.state()));
}

#[test]
fn clicking_a_tile_opens_the_photo_and_closes_the_clean_one() {
    let t = TempDir::new("click");
    let (mut app, ids) = app_with_album(&t);
    go_to(&mut app, ids[0]).unwrap();
    assert!(!app.session.active().unwrap().is_dirty(), "a photo opens clean");
    let mut h = harness(app);
    h.run_steps(4);
    h.get_by_label("b.png").click();
    h.run_steps(3);
    assert_eq!(shown(h.state()), Some(ids[1]), "photo.open ran for the clicked tile");
    assert_eq!(open_photos(h.state()), vec![ids[1]], "a.png had no changes: closed");
    assert_eq!(h.state().ui.views.len(), h.state().session.documents().len());

    // A dirty photo stays open; clicking its tile again activates the existing document.
    h.state_mut().run("layer.new.layer", json!({})).unwrap();
    assert!(h.state().session.active().unwrap().is_dirty());
    h.get_by_label("c.png").click();
    h.run_steps(3);
    assert_eq!(shown(h.state()), Some(ids[2]));
    assert_eq!(open_photos(h.state()), vec![ids[1], ids[2]], "b.png has unsaved changes: kept");
    h.get_by_label("b.png").click();
    h.run_steps(3);
    assert_eq!(shown(h.state()), Some(ids[1]));
    assert_eq!(open_photos(h.state()), vec![ids[1]], "the existing tab was activated, clean c.png closed");
    assert_eq!(h.state().session.documents().len(), 1);
}

#[test]
fn next_and_previous_step_through_the_album_and_stop_at_the_ends() {
    let t = TempDir::new("step");
    let (mut app, ids) = app_with_album(&t);
    let ctx = egui::Context::default();
    // With no photo shown, Next starts at the first.
    assert_eq!(crate::menus::invoke(&mut app, &ctx, "view.clips.next", json!({})).unwrap()["photo"], ids[0]);
    assert_eq!(shown(&app), Some(ids[0]));
    crate::menus::invoke(&mut app, &ctx, "view.clips.next", json!({})).unwrap();
    assert_eq!(shown(&app), Some(ids[1]));
    assert_eq!(open_photos(&app), vec![ids[1]]);
    app.run("layer.new.layer", json!({})).unwrap();
    crate::menus::invoke(&mut app, &ctx, "view.clips.next", json!({})).unwrap();
    assert_eq!(shown(&app), Some(ids[2]));
    assert_eq!(open_photos(&app), vec![ids[1], ids[2]], "the dirty photo stays open");
    // At the last photo Next stops (no wrap-around).
    let r = crate::menus::invoke(&mut app, &ctx, "view.clips.next", json!({})).unwrap();
    assert_eq!(r["moved"], false);
    assert_eq!(shown(&app), Some(ids[2]));
    crate::menus::invoke(&mut app, &ctx, "view.clips.prev", json!({})).unwrap();
    assert_eq!(shown(&app), Some(ids[1]));
    assert_eq!(open_photos(&app), vec![ids[1]]);
    step(&mut app, -1).unwrap();
    assert_eq!(shown(&app), Some(ids[0]));
    assert_eq!(open_photos(&app), vec![ids[1], ids[0]]);
    assert_eq!(step(&mut app, -1).unwrap()["moved"], false, "stops at the first photo");
    assert_eq!(app.ui.views.len(), app.session.documents().len());

    // Disabled in the Library and for an empty album.
    assert!(crate::menus::is_enabled(&app, "view.clips.prev"));
    app.ui.module = Module::Library;
    assert!(!crate::menus::is_enabled(&app, "view.clips.next"));
    app.ui.module = Module::Edit;
    let b = app.session.project.as_ref().unwrap().project.albums[1].id;
    app.ui.clips = ClipsUi { album: Some(b), picked_with: shown(&app) };
    assert_eq!(current_album(&app), Some(b), "the header's pick wins while the same photo shows");
    assert!(!crate::menus::is_enabled(&app, "view.clips.next"));
    assert!(step(&mut app, 1).is_err());
}

#[test]
fn shortcuts_are_registered_without_conflicts() {
    let app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    let b = crate::shortcut_dispatch::bindings(&app);
    let find = |id: &str| b.iter().find(|(i, _)| i == id).map(|(_, sc)| *sc);
    let next = find("view.clips.next").expect("next is bound");
    assert_eq!(next.logical_key, egui::Key::ArrowRight);
    assert!(next.modifiers.command);
    assert_eq!(find("view.clips.prev").unwrap().logical_key, egui::Key::ArrowLeft);
    // The Move tool's nudges (plain/⇧/⌥ arrows) are not taken.
    assert!(!b.iter().any(|(_, sc)| sc.logical_key == egui::Key::ArrowRight && !sc.modifiers.command));
    // A focused widget or text field keeps ⌘ + arrow for itself… except widgets give ⌘ shortcuts on.
    use crate::shortcut_dispatch::Focus;
    assert!(Focus::None.allows(&next));
    assert!(!Focus::Text.allows(&next), "⌘→ moves by word in a text field");
}

#[test]
fn the_current_album_follows_the_active_photo_then_the_library() {
    let t = TempDir::new("album");
    let (mut app, ids) = app_with_album(&t);
    let st = app.session.project.as_ref().unwrap();
    let (a, b) = (st.project.albums[0].id, st.project.albums[1].id);
    assert_eq!(current_album(&app), Some(a), "first album by default");
    app.ui.library.album = Some(b);
    assert_eq!(current_album(&app), Some(b), "the Library's album");
    go_to(&mut app, ids[0]).unwrap();
    assert_eq!(current_album(&app), Some(a), "the active photo's album");
    app.ui.clips = ClipsUi { album: Some(b), picked_with: Some(ids[0]) };
    assert_eq!(current_album(&app), Some(b));
    go_to(&mut app, ids[1]).unwrap();
    assert_eq!(current_album(&app), Some(a), "the pick ends when another photo shows");
    // Reveal in Library selects the photo there.
    reveal(&mut app, a, ids[1]);
    assert!(library_ui::active(&app));
    assert_eq!(app.ui.library.photos, vec![ids[1]]);
}

#[test]
fn height_is_clamped_and_collapsing_leaves_the_header() {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    assert_eq!(height(&app), DEFAULT_HEIGHT);
    app.ui.panels.clips_height = 5000.0;
    assert_eq!(height(&app), MAX_HEIGHT);
    app.ui.panels.clips_height = f32::NAN;
    assert_eq!(height(&app), DEFAULT_HEIGHT);
    app.ui.panels.clips_height = 10.0;
    assert_eq!(height(&app), MIN_HEIGHT);
    app.ui.panels.clips_collapsed = true;
    assert_eq!(height(&app), HEADER_H);
    // Panels written before the bar existed load with it on.
    let old = json!({"layers": true, "history": false, "properties": true, "color": true, "navigator": false, "toolbar": true, "options_bar": true, "status_bar": true});
    let p: crate::state::Panels = serde_json::from_value(old).unwrap();
    assert!(p.clips && !p.clips_collapsed);
}
