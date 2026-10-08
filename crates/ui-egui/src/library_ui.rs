//! PhotoVision's Library module: the open project's albums and photos, and its colour management.
//!
//! Shown instead of the document area while a project is open and the title bar's
//! "Library | Edit" switch is on Library ([`Module`], in [`crate::UiState`] so the control
//! channel can flip it). Left: the project and its albums; centre: the selected album's
//! thumbnails; right: the Color Management inspector for the selection (project, album or
//! photos). Everything it changes goes through the engine's `project.*`, `album.*` and `photo.*`
//! commands; the shell only keeps the selection and the thumbnail textures.
//!
//! Thumbnails come from `photo.thumbnail`'s cache (sRGB PNGs in the project's `.pvcache`), via
//! [`photocraft_engine::project_cmds::ThumbPlan`]: on a worker thread when background jobs are
//! on (the desktop app), else a few per frame, so the UI never stalls on a large album.
//!
//! Projects need a file system: on the web the Project menu items are disabled (the engine's
//! predicates say why) and the pickers are absent.

use std::collections::HashMap;

use egui::{Color32, Rect, RichText, Sense, Stroke, StrokeKind, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::state::DialogKind;
use crate::theme::Tokens;
use crate::widgets;

/// Which module the window shows while a project is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Module {
    /// The project's albums and photos.
    Library,
    /// The image editor (the documents).
    #[default]
    Edit,
}

/// What the Color Management inspector edits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Focus {
    #[default]
    Project,
    Album,
    Photos,
}

/// Library selection (serialised with the UI state: `ui.inspect` reports it, `ui.set` drives it).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LibraryUi {
    /// The album shown in the grid.
    pub album: Option<u64>,
    /// Selected photos (by photo id).
    pub photos: Vec<u64>,
    /// Which level the inspector edits.
    pub focus: Focus,
    /// Open photos whose Input space changed since they were decoded: offered "Rebuild from
    /// original" (`photo.rebuild`).
    pub rebuild: Vec<u64>,
}

/// The inspector's target level.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Project,
    Album(u64),
    Photos(Vec<u64>),
}

impl LibraryUi {
    pub fn target(&self) -> Target {
        match (self.focus, self.album) {
            (Focus::Photos, _) if !self.photos.is_empty() => Target::Photos(self.photos.clone()),
            (Focus::Album | Focus::Photos, Some(a)) => Target::Album(a),
            _ => Target::Project,
        }
    }
}

/// Pipeline fields in the order shown, with their labels (Resolve-style wording).
pub const FIELDS: [(&str, &str); 5] = [
    ("input", "Input color space"),
    ("working", "Photo color space"),
    ("output", "Output color space"),
    ("intent", "Rendering intent"),
    ("bpc", "Black point compensation"),
];

const INTENTS: [(&str, &str); 4] =
    [("perceptual", "Perceptual"), ("relative", "Relative Colorimetric"), ("saturation", "Saturation"), ("absolute", "Absolute Colorimetric")];

/// Tile size of the thumbnail grid (points) and the thumbnails' longer side (pixels).
const TILE: f32 = 156.0;
const LABEL_H: f32 = 22.0;
const GAP: f32 = 12.0;
const THUMB_PX: u32 = 256;
/// Thumbnails decoded per frame when they are not made on a worker thread.
const INLINE_PER_FRAME: usize = 4;
/// `project.info` is re-read at most this often (seconds) without a change, to notice files
/// that appeared or went missing outside the app.
const INFO_REFRESH_S: f64 = 3.0;

// ------------------------------------------------------------------ runtime state

enum Thumb {
    /// The PNG at this path is (being) loaded; its texture is in `textures` once ready.
    Requested(String),
    Failed,
}

#[cfg(not(target_arch = "wasm32"))]
type ThumbResult = (u64, String, Result<photocraft_raster::Rgba8Image, String>);

#[cfg(not(target_arch = "wasm32"))]
struct Worker {
    tx: std::sync::mpsc::Sender<photocraft_engine::project_cmds::ThumbPlan>,
    rx: std::sync::mpsc::Receiver<ThumbResult>,
}

/// Shell-side cache: the last `project.info`, thumbnail textures (by PNG path) and background work.
#[derive(Default)]
pub struct Runtime {
    info: Option<Value>,
    info_time: f64,
    /// A command ran since `info` was read (set by [`PhotocraftApp::run`]).
    pub(crate) stale: bool,
    thumbs: HashMap<u64, Thumb>,
    textures: HashMap<String, egui::TextureHandle>,
    inline_budget: usize,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<Worker>,
    #[cfg(not(target_arch = "wasm32"))]
    export: Option<std::sync::mpsc::Receiver<Result<Value, String>>>,
}

/// Is the Library showing (a project is open and the switch is on Library)?
pub fn active(app: &PhotocraftApp) -> bool {
    app.ui.module == Module::Library && app.session.project.is_some()
}

/// The project's `project.info`, re-read when stale.
pub(crate) fn info(app: &mut PhotocraftApp, now: f64) -> Option<Value> {
    if app.session.project.is_none() {
        app.library.info = None;
        return None;
    }
    let rt = &mut app.library;
    if rt.stale || rt.info.is_none() || now - rt.info_time > INFO_REFRESH_S || now < rt.info_time {
        if rt.stale {
            // A sidecar may have been saved or a photo relinked: plan the thumbnails again.
            rt.thumbs.clear();
            if rt.textures.len() > 2000 {
                rt.textures.clear();
            }
        }
        rt.stale = false;
        rt.info_time = now;
        rt.info = app.session.execute("project.info", json!({})).ok();
    }
    app.library.info.clone()
}

/// The last-read project info without refreshing it (dialogs; tests).
pub fn cached_info(app: &mut PhotocraftApp) -> Option<Value> {
    if app.library.info.is_none() || app.library.stale {
        let t = app.library.info_time;
        return info(app, t);
    }
    app.library.info.clone()
}

pub(crate) fn albums(info: &Value) -> &[Value] {
    info["albums"].as_array().map_or(&[], Vec::as_slice)
}

pub(crate) fn album(info: &Value, id: u64) -> Option<&Value> {
    albums(info).iter().find(|a| a["id"].as_u64() == Some(id))
}

fn photo(info: &Value, id: u64) -> Option<&Value> {
    albums(info).iter().flat_map(|a| a["photos"].as_array().map_or(&[][..], Vec::as_slice)).find(|p| p["id"].as_u64() == Some(id))
}

pub(crate) fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// Keep the selection pointing at things that exist.
fn sanitize(app: &mut PhotocraftApp, info: &Value) {
    let lib = &mut app.ui.library;
    if lib.album.is_some_and(|a| album(info, a).is_none()) {
        lib.album = None;
    }
    if lib.album.is_none() {
        lib.album = albums(info).first().and_then(|a| a["id"].as_u64());
        if lib.focus == Focus::Album && lib.album.is_none() {
            lib.focus = Focus::Project;
        }
    }
    let shown = lib.album.and_then(|a| album(info, a)).and_then(|a| a["photos"].as_array());
    lib.photos.retain(|p| shown.is_some_and(|ps| ps.iter().any(|x| x["id"].as_u64() == Some(*p))));
    if lib.focus == Focus::Photos && lib.photos.is_empty() {
        lib.focus = if lib.album.is_some() { Focus::Album } else { Focus::Project };
    }
}

// ------------------------------------------------------------------ colour fields

/// A display label for a pipeline field's value (a space id, `auto`, an intent id or a bool).
pub fn value_label(spaces: &Value, field: &str, v: &Value) -> String {
    match field {
        "intent" => INTENTS.iter().find(|(id, _)| Some(*id) == v.as_str()).map_or_else(|| str_of(v).to_string(), |(_, l)| tl!(l).to_string()),
        "bpc" => match v.as_bool() {
            Some(true) => tl!("On").into(),
            Some(false) => tl!("Off").into(),
            None => "—".into(),
        },
        _ => {
            let s = str_of(v);
            if s.eq_ignore_ascii_case("auto") {
                return tl!("Auto (embedded profile)").into();
            }
            for list in ["input", "working", "output"] {
                if let Some(l) = spaces[list].as_array().and_then(|a| a.iter().find(|e| e["id"].as_str() == Some(s))) {
                    return str_of(&l["label"]).to_string();
                }
            }
            photocraft_engine::project::color::SpaceId::parse(s).map(|id| id.label()).unwrap_or_else(|_| s.to_string())
        }
    }
}

/// The choices of a pipeline field's dropdown: `Inherit (<inherited value>)` first when the level
/// inherits (album, photo), then every value; a custom profile in use is kept as a choice.
pub fn field_options(spaces: &Value, field: &str, inherited: Option<&Value>, current: &Value) -> Vec<(Value, String)> {
    let mut v: Vec<(Value, String)> = Vec::new();
    if let Some(parent) = inherited {
        let shown = value_label(spaces, field, &parent[field]);
        v.push((Value::Null, crate::i18n::fmt(tl!("Inherit ({value})"), &[("value", &shown)])));
    }
    match field {
        "intent" => v.extend(INTENTS.iter().map(|(id, l)| (json!(id), tl!(l).to_string()))),
        "bpc" => v.extend([(json!(true), tl!("On").to_string()), (json!(false), tl!("Off").to_string())]),
        _ => {
            if field == "input" {
                v.push((json!("auto"), tl!("Auto (embedded profile)").into()));
            }
            let list = spaces[field].as_array().map_or(&[][..], Vec::as_slice);
            v.extend(list.iter().map(|e| (e["id"].clone(), str_of(&e["label"]).to_string())));
        }
    }
    if !current.is_null() && !v.iter().any(|(id, _)| id == current) {
        v.push((current.clone(), value_label(spaces, field, current)));
    }
    v
}

/// `project.setColor` params for setting `field` to `value` (`null` = inherit) on `target`:
/// one call per photo when several are selected.
pub fn set_color_params(target: &Target, field: &str, value: &Value) -> Vec<Value> {
    match target {
        Target::Project => vec![json!({"level": "project", "field": field, "value": value})],
        Target::Album(id) => vec![json!({"level": "album", "id": id, "field": field, "value": value})],
        Target::Photos(ids) => ids.iter().map(|id| json!({"level": "photo", "id": id, "field": field, "value": value})).collect(),
    }
}

/// Runs `project.setColor` for `target` and remembers open photos that now need a rebuild.
pub fn apply_color(app: &mut PhotocraftApp, target: &Target, field: &str, value: &Value) -> Result<(), String> {
    for p in set_color_params(target, field, value) {
        let r = app.run("project.setColor", p)?;
        for d in r["documents"].as_array().into_iter().flatten() {
            if d["needsRebuild"] == json!(true)
                && let Some(ph) = d["photo"].as_u64()
                && !app.ui.library.rebuild.contains(&ph)
            {
                app.ui.library.rebuild.push(ph);
            }
        }
    }
    Ok(())
}

/// The pipeline dropdowns. `current` holds the level's own settings (missing = unset),
/// `inherited` the parent level's resolved pipeline (None at project level, where an unset field
/// shows `fallback`'s value). Returns the field the user changed and its new value.
fn color_rows(ui: &mut egui::Ui, salt: &str, spaces: &Value, current: &Value, inherited: Option<&Value>, fallback: &Value) -> Option<(&'static str, Value)> {
    let t = Tokens::get(ui.ctx());
    let mut changed = None;
    // Stacked (label above a full-width dropdown): the long space names fit a narrow inspector.
    let width = (ui.available_width() - 16.0).clamp(160.0, 420.0);
    for (field, label) in FIELDS {
        ui.label(RichText::new(tl!(label)).color(t.text_dim).size(12.0));
        let mut cur = current.get(field).cloned().unwrap_or(Value::Null);
        if inherited.is_none() && cur.is_null() {
            cur = fallback.get(field).cloned().unwrap_or(Value::Null);
        }
        let opts = field_options(spaces, field, inherited, &cur);
        let refs: Vec<(Value, &str)> = opts.iter().map(|(v, l)| (v.clone(), l.as_str())).collect();
        let mut sel = cur.clone();
        if widgets::dropdown(ui, &format!("{salt}-{field}"), &mut sel, &refs, width) && sel != cur {
            changed = Some((field, sel));
        }
        ui.add_space(6.0);
    }
    changed
}

/// "Monitor: <profile>" (the display profile the canvas converts to; read-only here).
fn monitor_line(app: &PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let monitor = app.session.color.monitor();
    ui.label(RichText::new(crate::i18n::fmt(tl!("Monitor: {profile}"), &[("profile", &monitor.description)])).color(t.text_faint).size(12.0))
        .on_hover_text(tl!("Set in Edit › Color Settings › Monitor Profile"));
}

// ------------------------------------------------------------------ the view

/// The Library view (in place of the document area).
pub fn view(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let now = ui.input(|i| i.time);
    let Some(info) = info(app, now) else { return };
    sanitize(app, &info);
    begin_thumbs(app, ui.ctx());
    let t = Tokens::get(ui.ctx());
    egui::Panel::left("library-tree")
        .exact_size(230.0)
        .resizable(false)
        .frame(egui::Frame::NONE.fill(t.dock).inner_margin(egui::Margin::same(10)))
        .show(ui, |ui| sidebar(app, ui, &info));
    egui::Panel::right("library-inspector")
        .exact_size(330.0)
        .resizable(false)
        .frame(egui::Frame::NONE.fill(t.dock).inner_margin(egui::Margin::same(12)))
        .show(ui, |ui| inspector(app, ui, &info));
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.canvas).inner_margin(egui::Margin::same(14))).show(ui, |ui| grid(app, ui, &info));
    // Files dragged over the grid from the file manager: they are imported on drop.
    if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) {
        let r = ui.max_rect();
        ui.painter().rect_stroke(r.shrink(3.0), t.radius, Stroke::new(2.0, t.accent), StrokeKind::Inside);
    }
}

fn sidebar(app: &mut PhotocraftApp, ui: &mut egui::Ui, info: &Value) {
    let t = Tokens::get(ui.ctx());
    ui.horizontal(|ui| {
        let name = format!("{}{}", str_of(&info["name"]), if info["dirty"] == json!(true) { "  •" } else { "" });
        let on = app.ui.library.focus == Focus::Project;
        let r = ui.add(egui::Button::selectable(on, RichText::new(name).font(crate::theme::semibold(14.0))));
        if r.clicked() {
            app.ui.library.focus = Focus::Project;
        }
        r.on_hover_text(str_of(&info["path"]));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::icons::button(ui, "settings", 24.0, false, tl!("Project Settings…")).clicked() {
                open_settings(app);
            }
        });
    });
    ui.add_space(6.0);
    widgets::hairline(ui);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Albums")).color(t.text_faint).size(12.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::icons::button(ui, "plus", 22.0, false, tl!("New Album…")).clicked() {
                open_new_album(app);
            }
        });
    });
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for a in albums(info) {
            let Some(id) = a["id"].as_u64() else { continue };
            let n = a["photos"].as_array().map_or(0, Vec::len);
            let on = app.ui.library.album == Some(id) && app.ui.library.focus != Focus::Project;
            let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
            if on || r.hovered() {
                ui.painter().rect_filled(rect, t.radius_sm, if on { t.row_selected } else { t.hover });
            }
            let font = egui::FontId::proportional(13.0);
            let name = ui.painter().layout_no_wrap(str_of(&a["name"]).to_string(), font.clone(), t.text);
            ui.painter().galley(egui::pos2(rect.left() + 8.0, rect.center().y - name.size().y / 2.0), name, t.text);
            let count = ui.painter().layout_no_wrap(n.to_string(), font, t.text_faint);
            ui.painter().galley(egui::pos2(rect.right() - 8.0 - count.size().x, rect.center().y - count.size().y / 2.0), count, t.text_faint);
            let album_name = str_of(&a["name"]).to_string();
            r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, &album_name));
            let r = r.on_hover_text(&album_name);
            if r.clicked() {
                let lib = &mut app.ui.library;
                lib.album = Some(id);
                lib.photos.clear();
                lib.focus = Focus::Album;
            }
            r.context_menu(|ui| {
                if ui.button(tl!("Import Photos…")).clicked() {
                    app.ui.library.album = Some(id);
                    let _ = pick_and_import(app);
                    ui.close();
                }
                if ui.button(tl!("Export Album…")).clicked() {
                    open_export(app, id);
                    ui.close();
                }
                ui.separator();
                if ui.button(tl!("Rename Album…")).clicked() {
                    open_rename(app, id);
                    ui.close();
                }
                if ui.button(tl!("Delete Album…")).clicked() {
                    open_delete(app, id);
                    ui.close();
                }
            });
        }
        if albums(info).is_empty() {
            ui.label(RichText::new(tl!("No albums yet")).color(t.text_faint));
        }
    });
}

/// Paints a photo tile: thumbnail (or a placeholder), name, badges and selection.
fn tile(app: &mut PhotocraftApp, ui: &mut egui::Ui, p: &Value) -> egui::Response {
    let t = Tokens::get(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(TILE, TILE + LABEL_H), Sense::click());
    let id = p["id"].as_u64().unwrap_or(0);
    let selected = app.ui.library.photos.contains(&id);
    let img_rect = Rect::from_min_size(rect.min, vec2(TILE, TILE));
    let painter = ui.painter_at(rect);
    painter.rect_filled(img_rect, t.radius_sm, if resp.hovered() { t.hover } else { t.card });
    let exists = p["exists"] == json!(true);
    match thumb_texture(app, ui.ctx(), p) {
        Some(tex) => {
            let size = tex.size_vec2();
            let k = ((TILE - 10.0) / size.x.max(1.0)).min((TILE - 10.0) / size.y.max(1.0));
            let r = Rect::from_center_size(img_rect.center(), size * k);
            painter.image(tex.id(), r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            let icon = if exists { "image" } else { "triangle-alert" };
            crate::icons::paint(ui, Rect::from_center_size(img_rect.center(), vec2(28.0, 28.0)), icon, 24.0, t.text_faint);
        }
    }
    if selected {
        painter.rect_stroke(img_rect.shrink(1.0), t.radius_sm, Stroke::new(2.0, t.accent), StrokeKind::Inside);
    }
    paint_badges(&painter, &t, img_rect.left_top() + vec2(6.0, 6.0), p);
    let name = str_of(&p["name"]);
    let mut job = egui::text::LayoutJob::simple_singleline(name.to_string(), egui::FontId::proportional(12.0), if selected { t.text } else { t.text_dim });
    job.wrap = egui::text::TextWrapping::truncate_at_width(TILE - 4.0);
    let g = painter.layout_job(job);
    painter.galley(egui::pos2(rect.center().x - g.size().x / 2.0, img_rect.bottom() + 4.0), g, t.text_dim);
    let missing_reason = (!exists).then(|| crate::i18n::fmt(tl!("File not found: {path}"), &[("path", str_of(&p["file"]))]));
    match missing_reason {
        Some(why) => resp.on_hover_text(why),
        None => resp.on_hover_text(str_of(&p["file"])),
    }
}

fn grid(app: &mut PhotocraftApp, ui: &mut egui::Ui, info: &Value) {
    let t = Tokens::get(ui.ctx());
    let Some(aid) = app.ui.library.album else {
        empty_state(app, ui, tl!("Create an album to start"), tl!("Albums group the photos of a project."), true);
        return;
    };
    let Some(a) = album(info, aid) else { return };
    let photos: Vec<Value> = a["photos"].as_array().cloned().unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label(RichText::new(str_of(&a["name"])).font(crate::theme::semibold(16.0)).color(t.text));
        ui.label(RichText::new(crate::i18n::trn(crate::i18n::current(), photos.len() as u64, "{n} photo", "{n} photos")).color(t.text_faint));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::secondary_button(ui, tl!("Export Album…"), 0.0).clicked() {
                open_export(app, aid);
            }
            if widgets::secondary_button(ui, tl!("Import Photos…"), 0.0).clicked()
                && let Err(e) = pick_and_import(app)
            {
                crate::notices::error(app, e);
            }
        });
    });
    ui.add_space(10.0);
    if photos.is_empty() {
        empty_state(app, ui, tl!("No photos in this album yet"), tl!("Import photos, or drop image files here."), false);
        return;
    }
    let cols = (((ui.available_width() + GAP) / (TILE + GAP)).floor() as usize).max(1);
    let rows = photos.len().div_ceil(cols);
    let mut clicked: Option<(u64, bool, bool)> = None;
    let mut open: Option<u64> = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, TILE + LABEL_H + GAP, rows, |ui, range| {
        for row in range {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for p in photos.iter().skip(row * cols).take(cols) {
                    let Some(id) = p["id"].as_u64() else { continue };
                    let r = tile(app, ui, p);
                    if r.double_clicked() {
                        open = Some(id);
                    } else if r.clicked() {
                        let m = ui.input(|i| i.modifiers);
                        clicked = Some((id, m.command, m.shift));
                    }
                    r.context_menu(|ui| {
                        if ui.button(tl!("Open")).clicked() {
                            open = Some(id);
                            ui.close();
                        }
                        if p["exists"] != json!(true) && ui.button(tl!("Relink…")).clicked() {
                            relink(app, id);
                            ui.close();
                        }
                        if ui.button(tl!("Remove from Project…")).clicked() {
                            if !app.ui.library.photos.contains(&id) {
                                app.ui.library.photos = vec![id];
                            }
                            open_remove(app);
                            ui.close();
                        }
                    });
                }
            });
            ui.add_space(GAP - ui.spacing().item_spacing.y);
        }
    });
    if let Some((id, toggle, range)) = clicked {
        select(app, &photos, id, toggle, range);
    }
    if let Some(id) = open {
        open_photo(app, id);
    }
}

/// Click selection: plain click selects one; ⌘/Ctrl toggles; ⇧ extends from the last selected.
fn select(app: &mut PhotocraftApp, photos: &[Value], id: u64, toggle: bool, range: bool) {
    let lib = &mut app.ui.library;
    let ids: Vec<u64> = photos.iter().filter_map(|p| p["id"].as_u64()).collect();
    if range && let (Some(&last), Some(to)) = (lib.photos.last(), ids.iter().position(|x| *x == id)) {
        if let Some(from) = ids.iter().position(|x| *x == last) {
            let (a, b) = (from.min(to), from.max(to));
            for x in ids.get(a..=b).unwrap_or(&[]) {
                if !lib.photos.contains(x) {
                    lib.photos.push(*x);
                }
            }
        }
    } else if toggle {
        if let Some(i) = lib.photos.iter().position(|x| *x == id) {
            lib.photos.remove(i);
        } else {
            lib.photos.push(id);
        }
    } else {
        lib.photos = vec![id];
    }
    lib.focus = if lib.photos.is_empty() { Focus::Album } else { Focus::Photos };
}

/// Opens a photo in the editor (`photo.open`) and switches to Edit.
pub fn open_photo(app: &mut PhotocraftApp, id: u64) {
    match app.run("photo.open", json!({"id": id})) {
        Ok(r) => {
            app.ui.module = Module::Edit;
            app.ui.chrome.home = None;
            if let Some(w) = r["warnings"].as_array().filter(|w| !w.is_empty()) {
                let lines: Vec<String> = w.iter().map(|x| str_of(x).to_string()).collect();
                crate::notices::io_warnings(app, tl!("Opened photo"), &lines);
            }
        }
        Err(e) => crate::notices::error(app, e),
    }
}

fn empty_state(app: &mut PhotocraftApp, ui: &mut egui::Ui, title: &str, hint: &str, no_album: bool) {
    let t = Tokens::get(ui.ctx());
    let area = ui.available_rect_before_wrap();
    let card = Rect::from_center_size(area.center() - vec2(0.0, 40.0), vec2(360.0, 170.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
        ui.vertical_centered(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            crate::icons::paint(ui, r, if no_album { "folder-plus" } else { "image" }, 34.0, t.text_faint);
            ui.add_space(8.0);
            ui.label(RichText::new(title).font(crate::theme::semibold(16.0)).color(t.text));
            ui.add_space(4.0);
            ui.label(RichText::new(hint).color(t.text_dim));
            ui.add_space(16.0);
            if no_album {
                if widgets::primary_button(ui, tl!("New Album…"), 180.0).clicked() {
                    open_new_album(app);
                }
            } else if widgets::primary_button(ui, tl!("Import Photos…"), 180.0).clicked()
                && let Err(e) = pick_and_import(app)
            {
                crate::notices::error(app, e);
            }
        });
    });
}

fn inspector(app: &mut PhotocraftApp, ui: &mut egui::Ui, info: &Value) {
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(tl!("Color Management")).font(crate::theme::semibold(15.0)).color(t.text));
    ui.add_space(2.0);
    let target = app.ui.library.target();
    let spaces = &info["spaces"];
    let (heading, current, inherited): (String, Value, Option<Value>) = match &target {
        Target::Project => (crate::i18n::fmt(tl!("Project: {name}"), &[("name", str_of(&info["name"]))]), info["color"].clone(), None),
        Target::Album(id) => {
            let a = album(info, *id).cloned().unwrap_or(Value::Null);
            (crate::i18n::fmt(tl!("Album: {name}"), &[("name", str_of(&a["name"]))]), a["color"].clone(), Some(info["resolvedColor"].clone()))
        }
        Target::Photos(ids) => {
            let first = ids.first().and_then(|id| photo(info, *id)).cloned().unwrap_or(Value::Null);
            let parent = first["album"].as_u64().and_then(|a| album(info, a)).map(|a| a["resolvedColor"].clone()).unwrap_or(Value::Null);
            let h = if ids.len() == 1 {
                crate::i18n::fmt(tl!("Photo: {name}"), &[("name", str_of(&first["name"]))])
            } else {
                crate::i18n::trn(crate::i18n::current(), ids.len() as u64, "{n} photo", "{n} photos")
            };
            (h, first["color"].clone(), Some(parent))
        }
    };
    ui.label(RichText::new(heading).color(t.text_dim));
    ui.add_space(8.0);
    widgets::hairline(ui);
    ui.add_space(10.0);
    if let Some((field, value)) = color_rows(ui, "inspector", spaces, &current, inherited.as_ref(), &info["resolvedColor"])
        && let Err(e) = apply_color(app, &target, field, &value)
    {
        crate::notices::error(app, e);
    }
    ui.add_space(10.0);
    monitor_line(app, ui);
    ui.add_space(6.0);
    ui.label(
        RichText::new(tl!("Photos open in the Input space and are edited in the Photo space; the Output space is used for export and soft-proofed on screen."))
            .color(t.text_faint)
            .size(11.5),
    );
    rebuild_notice(app, ui);
}

/// "Input space changed" card with Rebuild from original.
fn rebuild_notice(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    // Only photos that are still open can be rebuilt.
    let open: Vec<u64> = app.ui.library.rebuild.iter().copied().filter(|p| app.session.photo_document(*p).is_some()).collect();
    app.ui.library.rebuild = open.clone();
    if open.is_empty() {
        return;
    }
    let t = Tokens::get(ui.ctx());
    ui.add_space(14.0);
    egui::Frame::NONE.fill(t.card).stroke(Stroke::new(1.0, t.warning)).corner_radius(t.radius).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
        ui.label(
            RichText::new(crate::i18n::trn(
                crate::i18n::current(),
                open.len() as u64,
                "The Input space changed for {n} open photo. It was decoded with the old one.",
                "The Input space changed for {n} open photos. They were decoded with the old one.",
            ))
            .color(t.text),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if widgets::primary_button(ui, tl!("Rebuild from original"), 0.0)
                .on_hover_text(tl!("Re-read the original files; layers above the bottom one are kept"))
                .clicked()
            {
                rebuild(app, &open);
            }
            if widgets::secondary_button(ui, tl!("Dismiss"), 0.0).clicked() {
                app.ui.library.rebuild.clear();
            }
        });
    });
}

/// `photo.rebuild` for each photo; failures become notices.
pub fn rebuild(app: &mut PhotocraftApp, photos: &[u64]) {
    let mut failed = Vec::new();
    for &id in photos {
        if let Err(e) = app.run("photo.rebuild", json!({"id": id})) {
            failed.push(e);
        }
    }
    app.ui.library.rebuild.retain(|p| !photos.contains(p));
    if !failed.is_empty() {
        crate::notices::post(app, tl!("Couldn't rebuild from the original"), failed, true);
    }
}

/// A small filled label (a tile's "Edited" / "Missing" badge) at `at`; returns its right edge.
pub(crate) fn paint_badge(painter: &egui::Painter, t: &Tokens, at: egui::Pos2, text: &str, fill: Color32) -> f32 {
    let g = painter.layout_no_wrap(text.to_string(), egui::FontId::proportional(10.5), Color32::WHITE);
    let r = Rect::from_min_size(at, g.size() + vec2(8.0, 4.0));
    painter.rect_filled(r, t.radius_sm, fill);
    painter.galley(r.min + vec2(4.0, 2.0), g, Color32::WHITE);
    r.right()
}

/// A photo's badges from its `project.info` entry, left to right from `at`: Edited (a sidecar
/// exists), Missing (the original is gone). Returns the x after the last one.
pub(crate) fn paint_badges(painter: &egui::Painter, t: &Tokens, at: egui::Pos2, p: &Value) -> f32 {
    let mut x = at.x;
    if p["hasSidecar"] == json!(true) {
        x = paint_badge(painter, t, egui::pos2(x, at.y), tl!("Edited"), t.accent) + 4.0;
    }
    if p["exists"] != json!(true) {
        x = paint_badge(painter, t, egui::pos2(x, at.y), tl!("Missing"), t.danger) + 4.0;
    }
    x
}

// ------------------------------------------------------------------ thumbnails

/// Once per frame before tiles ask for thumbnails (the Library grid or the Clips bar): resets
/// the inline decode budget and takes the worker's finished thumbnails.
pub(crate) fn begin_thumbs(app: &mut PhotocraftApp, ctx: &egui::Context) {
    app.library.inline_budget = INLINE_PER_FRAME;
    #[cfg(not(target_arch = "wasm32"))]
    receive_thumbs(app, ctx);
    #[cfg(target_arch = "wasm32")]
    let _ = ctx;
}

/// The photo's thumbnail texture (`p` is its `project.info` entry), or `None` while it is being
/// made (on the worker thread, or a few per frame inline) or when it can't be.
pub(crate) fn thumb_texture(app: &mut PhotocraftApp, ctx: &egui::Context, p: &Value) -> Option<egui::TextureHandle> {
    let id = p["id"].as_u64()?;
    match app.library.thumbs.get(&id) {
        Some(Thumb::Requested(path)) => return app.library.textures.get(path).cloned(),
        Some(Thumb::Failed) => return None,
        None => {}
    }
    if p["exists"] != json!(true) && p["hasSidecar"] != json!(true) {
        app.library.thumbs.insert(id, Thumb::Failed);
        return None;
    }
    let plan = match photocraft_engine::project_cmds::thumbnail_plan(&app.session, id, THUMB_PX) {
        Ok(plan) => plan,
        Err(e) => {
            log::warn!("thumbnail of photo {id}: {e}");
            app.library.thumbs.insert(id, Thumb::Failed);
            return None;
        }
    };
    if let Some(tex) = app.library.textures.get(&plan.path).cloned() {
        app.library.thumbs.insert(id, Thumb::Requested(plan.path));
        return Some(tex);
    }
    #[cfg(not(target_arch = "wasm32"))]
    if app.background_jobs {
        let worker = app.library.worker.get_or_insert_with(|| spawn_worker(ctx.clone()));
        let path = plan.path.clone();
        if worker.tx.send(plan).is_ok() {
            app.library.thumbs.insert(id, Thumb::Requested(path));
        } else {
            app.library.worker = None;
        }
        return None;
    }
    if app.library.inline_budget == 0 {
        ctx.request_repaint();
        return None;
    }
    app.library.inline_budget -= 1;
    let loaded = plan.load().map_err(|e| e.to_string());
    store_thumb(app, ctx, id, plan.path, loaded)
}

fn store_thumb(
    app: &mut PhotocraftApp,
    ctx: &egui::Context,
    id: u64,
    path: String,
    loaded: Result<photocraft_raster::Rgba8Image, String>,
) -> Option<egui::TextureHandle> {
    match loaded {
        Ok(img) => {
            let image = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.pixels);
            let tex = ctx.load_texture(format!("library-thumb-{id}"), image, egui::TextureOptions::LINEAR);
            app.library.textures.insert(path.clone(), tex.clone());
            app.library.thumbs.insert(id, Thumb::Requested(path));
            Some(tex)
        }
        Err(e) => {
            log::warn!("thumbnail of photo {id}: {e}");
            app.library.thumbs.insert(id, Thumb::Failed);
            None
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_worker(ctx: egui::Context) -> Worker {
    let (tx, jobs) = std::sync::mpsc::channel::<photocraft_engine::project_cmds::ThumbPlan>();
    let (done, rx) = std::sync::mpsc::channel::<ThumbResult>();
    let spawned = std::thread::Builder::new().name("library-thumbnails".into()).spawn(move || {
        while let Ok(plan) = jobs.recv() {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| plan.load()))
                .map_err(|_| "thumbnail decoder crashed".to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            if done.send((plan.photo, plan.path.clone(), r)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    if let Err(e) = spawned {
        log::warn!("library thumbnails: no worker thread ({e}); thumbnails stay blank");
    }
    Worker { tx, rx }
}

#[cfg(not(target_arch = "wasm32"))]
fn receive_thumbs(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let done: Vec<ThumbResult> = match &app.library.worker {
        Some(w) => w.rx.try_iter().collect(),
        None => return,
    };
    for (id, path, r) in done {
        store_thumb(app, ctx, id, path, r);
    }
}

// ------------------------------------------------------------------ per frame

/// Per-frame work outside the view: a finished background album export.
pub fn tick(app: &mut PhotocraftApp) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(rx) = &app.library.export else { return };
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("the export stopped unexpectedly".into()),
        };
        app.library.export = None;
        export_done(app, r);
    }
    #[cfg(target_arch = "wasm32")]
    let _ = app;
}

/// Files dropped on the window while the Library shows: import them into the shown album.
/// Returns whether the drop was taken.
pub fn take_drop(app: &mut PhotocraftApp, files: &[egui::DroppedFileHandle]) -> bool {
    if !active(app) || files.is_empty() {
        return false;
    }
    let paths: Vec<String> = files.iter().map(|f| f.path().to_string_lossy().into_owned()).filter(|p| !p.is_empty()).collect();
    if paths.is_empty() {
        return false;
    }
    open_import(app, paths);
    true
}

/// Library keys: Delete / Backspace removes the selected photos (after a confirmation).
/// Returns whether a key was used.
pub fn keys(app: &mut PhotocraftApp, ctx: &egui::Context) -> bool {
    if !active(app) || app.ui.library.photos.is_empty() || ctx.text_edit_focused() {
        return false;
    }
    let pressed = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete) || i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace));
    if pressed {
        open_remove(app);
    }
    pressed
}

/// Status bar text while the Library shows.
pub fn status_text(app: &mut PhotocraftApp) -> Option<String> {
    if !active(app) {
        return None;
    }
    let info = cached_info(app)?;
    let total: usize = albums(&info).iter().map(|a| a["photos"].as_array().map_or(0, Vec::len)).sum();
    let n = albums(&info).len();
    Some(format!(
        "{} · {}",
        crate::i18n::trn(crate::i18n::current(), n as u64, "{n} album", "{n} albums"),
        crate::i18n::trn(crate::i18n::current(), total as u64, "{n} photo", "{n} photos")
    ))
}

/// The window title while the Library shows: the project name (• when unsaved).
pub fn window_title(app: &PhotocraftApp) -> Option<String> {
    if !active(app) {
        return None;
    }
    let st = app.session.project.as_ref()?;
    Some(format!("{} — {}{}", st.project.name, tl!("Library"), if st.dirty { "  •" } else { "" }))
}

/// The title bar's "Library | Edit" switch (only while a project is open).
pub fn module_switch(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if app.session.project.is_none() {
        return;
    }
    // Laid out right to left: Edit first so Library reads first.
    for (m, label) in [(Module::Edit, tl!("Edit")), (Module::Library, tl!("Library"))] {
        if ui.add(egui::Button::selectable(app.ui.module == m, label)).clicked() {
            app.ui.module = m;
        }
    }
}

// ------------------------------------------------------------------ menus

/// Paths the platform's file dialogs pick for the Library (see [`crate::Services::pick_paths`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathPick {
    /// Save dialog for a new `.pvproj`.
    NewProject,
    /// Open dialog for a `.pvproj`.
    OpenProject,
    /// Multi-file open dialog for images.
    ImportPhotos,
    /// Folder dialog.
    Folder,
}

fn pick(app: &mut PhotocraftApp, what: PathPick) -> Result<Vec<String>, String> {
    match app.services.pick_paths.as_mut() {
        Some(f) => Ok(f(what)),
        None => Err(tl!("Projects need the desktop app (there is no file system here)").into()),
    }
}

/// Menu items fronted by a picker or dialog (when called without their params). `None` when not ours.
pub fn menu(app: &mut PhotocraftApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let has = |k: &str| params.get(k).is_some_and(|v| !v.is_null());
    Some(match id {
        "project.new" | "project.open" if !has("path") => {
            let what = if id == "project.new" { PathPick::NewProject } else { PathPick::OpenProject };
            let path = match pick(app, what) {
                Ok(p) => p.into_iter().next(),
                Err(e) => return Some(Err(e)),
            };
            let Some(path) = path else { return Some(Ok(Value::Null)) };
            let mut p = params.as_object().cloned().unwrap_or_default();
            p.insert("path".into(), json!(path));
            if id == "project.new" {
                // The save dialog already asked about replacing an existing file.
                p.insert("overwrite".into(), json!(true));
            }
            app.run(id, Value::Object(p)).inspect(|_| project_opened(app))
        }
        "project.new" | "project.open" => app.run(id, params.clone()).inspect(|_| project_opened(app)),
        "project.settings" => Ok(json!({"dialog": open_settings(app)})),
        "album.new" if !has("name") => Ok(json!({"dialog": open_new_album(app)})),
        "album.import" if !has("paths") => pick_and_import(app),
        "album.export" if !has("folder") => match target_album(app) {
            Some(a) => Ok(json!({"dialog": open_export(app, a)})),
            None => Err(tl!("Select an album first").into()),
        },
        "album.rename" if !has("name") => match target_album(app) {
            Some(a) => Ok(json!({"dialog": open_rename(app, a)})),
            None => Err(tl!("Select an album first").into()),
        },
        "album.delete" if !has("id") => match target_album(app) {
            Some(a) => Ok(json!({"dialog": open_delete(app, a)})),
            None => Err(tl!("Select an album first").into()),
        },
        _ => return None,
    })
}

/// Enabled state of the Library menu items the shell fronts. `None` when not ours.
pub fn is_enabled(app: &PhotocraftApp, id: &str) -> Option<bool> {
    let project = app.session.project.as_ref();
    Some(match id {
        "project.settings" => project.is_some(),
        "album.rename" | "album.delete" | "album.export" => project.is_some_and(|st| {
            app.ui.library.album.or_else(|| st.project.albums.first().map(|a| a.id)).is_some_and(|a| st.project.albums.iter().any(|x| x.id == a))
        }),
        _ => return None,
    })
}

fn target_album(app: &PhotocraftApp) -> Option<u64> {
    let st = app.session.project.as_ref()?;
    app.ui.library.album.filter(|a| st.project.albums.iter().any(|x| x.id == *a)).or_else(|| st.project.albums.first().map(|a| a.id))
}

fn project_opened(app: &mut PhotocraftApp) {
    app.ui.module = Module::Library;
    app.ui.library = LibraryUi::default();
    app.library.thumbs.clear();
    app.library.textures.clear();
    app.library.stale = true;
}

fn pick_and_import(app: &mut PhotocraftApp) -> Result<Value, String> {
    let paths = pick(app, PathPick::ImportPhotos)?;
    if paths.is_empty() {
        return Ok(Value::Null);
    }
    Ok(json!({"dialog": open_import(app, paths)}))
}

fn relink(app: &mut PhotocraftApp, id: u64) {
    match pick(app, PathPick::ImportPhotos) {
        Ok(paths) => {
            if let Some(path) = paths.into_iter().next()
                && let Err(e) = app.run("photo.relink", json!({"id": id, "path": path}))
            {
                crate::notices::error(app, e);
            }
        }
        Err(e) => crate::notices::error(app, e),
    }
}

// ------------------------------------------------------------------ dialogs

fn dialog(app: &mut PhotocraftApp, kind: &str, label: &str, fields: Value) -> u64 {
    let mut f = Map::new();
    f.insert("__library".into(), json!(kind));
    f.insert("__label".into(), json!(label));
    if let Value::Object(m) = fields {
        f.extend(m);
    }
    app.ui.open_dialog(DialogKind::Command, f)
}

fn album_name(app: &PhotocraftApp, id: u64) -> String {
    app.session.project.as_ref().and_then(|st| st.project.albums.iter().find(|a| a.id == id)).map(|a| a.name.clone()).unwrap_or_default()
}

pub fn open_settings(app: &mut PhotocraftApp) -> u64 {
    let color = app.session.project.as_ref().and_then(|st| serde_json::to_value(&st.project.color).ok()).unwrap_or(json!({}));
    let mut fields = color.as_object().cloned().unwrap_or_default();
    fields.insert("__orig".into(), color);
    dialog(app, "settings", "Project Settings…", Value::Object(fields))
}

pub fn open_new_album(app: &mut PhotocraftApp) -> u64 {
    let n = app.session.project.as_ref().map_or(0, |st| st.project.albums.len());
    let name = crate::i18n::fmt(tl!("Album {n}"), &[("n", &(n + 1).to_string())]);
    dialog(app, "newAlbum", "New Album…", json!({"name": name}))
}

pub fn open_rename(app: &mut PhotocraftApp, id: u64) -> u64 {
    let name = album_name(app, id);
    dialog(app, "renameAlbum", "Rename Album…", json!({"album": id, "name": name}))
}

pub fn open_delete(app: &mut PhotocraftApp, id: u64) -> u64 {
    dialog(app, "deleteAlbum", "Delete Album…", json!({"album": id}))
}

pub fn open_remove(app: &mut PhotocraftApp) -> u64 {
    let photos = app.ui.library.photos.clone();
    dialog(app, "removePhotos", "Remove from Project…", json!({"photos": photos}))
}

/// The Import dialog for `paths`, into the shown album (or a new one when there is none).
pub fn open_import(app: &mut PhotocraftApp, paths: Vec<String>) -> u64 {
    let album = target_album(app);
    dialog(app, "import", "Import Photos…", json!({"album": album, "paths": paths, "mode": "reference", "newAlbum": tl!("Imported")}))
}

pub fn open_export(app: &mut PhotocraftApp, id: u64) -> u64 {
    let name = album_name(app, id);
    let dir = app.session.project.as_ref().map(|st| st.path.rfind(['/', '\\']).map_or(".", |i| &st.path[..i]).to_string()).unwrap_or_else(|| ".".into());
    let sep = if dir.contains('\\') { '\\' } else { '/' };
    let folder = format!("{dir}{sep}Export{sep}{name}");
    dialog(app, "export", "Export Album…", json!({"album": id, "folder": folder, "format": "jpeg", "quality": 10, "overwrite": false}))
}

/// Dialogs whose body and confirm live here.
pub fn owns(f: &Map<String, Value>) -> bool {
    f.contains_key("__library")
}

fn kind(f: &Map<String, Value>) -> &str {
    f.get("__library").and_then(Value::as_str).unwrap_or("")
}

pub fn ok_label(f: &Map<String, Value>) -> Option<&'static str> {
    Some(match kind(f) {
        "newAlbum" => tl!("Create"),
        "renameAlbum" => tl!("Rename"),
        "deleteAlbum" => tl!("Delete"),
        "removePhotos" => tl!("Remove"),
        "import" => tl!("Import"),
        "export" => tl!("Export"),
        "settings" => tl!("OK"),
        _ => return None,
    })
}

pub fn dialog_width(f: &Map<String, Value>) -> Option<f32> {
    match kind(f) {
        "settings" => Some(500.0),
        "import" | "export" => Some(460.0),
        _ => None,
    }
}

fn text_field(ui: &mut egui::Ui, f: &mut Map<String, Value>, key: &str, width: f32) {
    let mut s = f.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    if ui.add(egui::TextEdit::singleline(&mut s).desired_width(width)).changed() {
        f.insert(key.into(), json!(s));
    }
}

fn album_picker(app: &PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) -> bool {
    let Some(st) = app.session.project.as_ref() else { return false };
    if st.project.albums.is_empty() {
        return false;
    }
    let opts: Vec<(Value, &str)> = st.project.albums.iter().map(|a| (json!(a.id), a.name.as_str())).collect();
    let mut cur = f.get("album").cloned().unwrap_or(Value::Null);
    if cur.is_null() {
        cur = opts.first().map(|o| o.0.clone()).unwrap_or(Value::Null);
        f.insert("album".into(), cur.clone());
    }
    if widgets::dropdown(ui, "library-album-picker", &mut cur, &opts, 240.0) {
        f.insert("album".into(), cur);
    }
    true
}

pub fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    match kind(f).to_string().as_str() {
        "newAlbum" | "renameAlbum" => {
            ui.horizontal(|ui| {
                ui.label(tl!("Name:"));
                text_field(ui, f, "name", 260.0);
            });
        }
        "deleteAlbum" => {
            let id = f.get("album").and_then(Value::as_u64).unwrap_or(0);
            let n = app.session.project.as_ref().and_then(|st| st.project.albums.iter().find(|a| a.id == id)).map_or(0, |a| a.photos.len());
            ui.label(crate::i18n::fmt(tl!("Delete the album “{name}” from the project?"), &[("name", &album_name(app, id))]));
            ui.label(
                RichText::new(crate::i18n::trn(
                    crate::i18n::current(),
                    n as u64,
                    "Its {n} photo leaves the project; files on disk are kept.",
                    "Its {n} photos leave the project; files on disk are kept.",
                ))
                .color(t.text_dim),
            );
        }
        "removePhotos" => {
            let n = f.get("photos").and_then(Value::as_array).map_or(0, Vec::len);
            ui.label(crate::i18n::trn(crate::i18n::current(), n as u64, "Remove {n} photo from the project?", "Remove {n} photos from the project?"));
            ui.label(RichText::new(tl!("The files on disk are kept, and so are their edits.")).color(t.text_dim));
        }
        "import" => import_body(app, ui, f),
        "export" => export_body(app, ui, f),
        "settings" => settings_body(app, ui, f),
        _ => {}
    }
}

fn import_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    let paths: Vec<String> =
        f.get("paths").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    egui::Grid::new("library-import").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
        ui.label(RichText::new(tl!("Album:")).color(t.text_dim));
        if !album_picker(app, ui, f) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(tl!("New album")).color(t.text_faint));
                text_field(ui, f, "newAlbum", 180.0);
            });
        }
        ui.end_row();
        ui.label(RichText::new(tl!("Files:")).color(t.text_dim));
        ui.vertical(|ui| {
            ui.label(crate::i18n::trn(crate::i18n::current(), paths.len() as u64, "{n} file", "{n} files"));
            for p in paths.iter().take(5) {
                let name = p.rsplit(['/', '\\']).next().unwrap_or(p);
                ui.label(RichText::new(name).color(t.text_faint).size(12.0)).on_hover_text(p);
            }
            if paths.len() > 5 {
                ui.label(RichText::new(crate::i18n::fmt(tl!("…and {n} more"), &[("n", &(paths.len() - 5).to_string())])).color(t.text_faint).size(12.0));
            }
        });
        ui.end_row();
    });
    ui.add_space(10.0);
    let mode = f.get("mode").and_then(Value::as_str).unwrap_or("reference").to_string();
    for (m, label, hint) in [
        ("reference", tl!("Add (reference in place)"), tl!("The photos stay where they are; the project points to them.")),
        ("copy", tl!("Copy into project folder"), tl!("Copies the files next to the project, so it can move as one folder.")),
    ] {
        if ui.radio(mode == m, label).clicked() {
            f.insert("mode".into(), json!(m));
        }
        ui.indent(m, |ui| ui.label(RichText::new(hint).color(t.text_faint).size(12.0)));
    }
}

fn export_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    egui::Grid::new("library-export").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
        ui.label(RichText::new(tl!("Album:")).color(t.text_dim));
        album_picker(app, ui, f);
        ui.end_row();
        ui.label(RichText::new(tl!("Folder:")).color(t.text_dim));
        ui.horizontal(|ui| {
            text_field(ui, f, "folder", 250.0);
            if widgets::secondary_button(ui, tl!("Choose…"), 0.0).clicked() {
                match pick(app, PathPick::Folder) {
                    Ok(p) => {
                        if let Some(dir) = p.into_iter().next() {
                            f.insert("folder".into(), json!(dir));
                        }
                    }
                    Err(e) => crate::notices::error(app, e),
                }
            }
        });
        ui.end_row();
        ui.label(RichText::new(tl!("Format:")).color(t.text_dim));
        let mut fmt = f.get("format").cloned().unwrap_or(json!("jpeg"));
        let opts = [(json!("jpeg"), "JPEG"), (json!("png"), "PNG"), (json!("tiff"), "TIFF"), (json!("webp"), "WebP")];
        if widgets::dropdown(ui, "library-export-format", &mut fmt, &opts, 120.0) {
            f.insert("format".into(), fmt.clone());
        }
        ui.end_row();
        if matches!(fmt.as_str(), Some("jpeg" | "webp")) {
            ui.label(RichText::new(tl!("Quality:")).color(t.text_dim));
            let mut q = f.get("quality").and_then(Value::as_f64).unwrap_or(10.0) as f32;
            if widgets::value_field(ui, &mut q, 0.0..=12.0, "", 60.0).changed() {
                f.insert("quality".into(), json!(q.round()));
            }
            ui.end_row();
        }
    });
    ui.add_space(6.0);
    let mut over = f.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    if widgets::checkbox(ui, &mut over, tl!("Replace existing files")).changed() {
        f.insert("overwrite".into(), json!(over));
    }
    ui.label(RichText::new(tl!("Each photo is flattened and converted to its Output color space.")).color(t.text_faint).size(12.0));
}

fn settings_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    let Some(info) = cached_info(app) else {
        ui.label(tl!("No project is open."));
        return;
    };
    ui.label(
        RichText::new(crate::i18n::fmt(
            tl!("Color management for every photo in “{name}”. Albums and photos can override each setting."),
            &[("name", str_of(&info["name"]))],
        ))
        .color(t.text_dim),
    );
    ui.add_space(10.0);
    let current = Value::Object(f.iter().filter(|(k, _)| !k.starts_with("__")).map(|(k, v)| (k.clone(), v.clone())).collect());
    if let Some((field, value)) = color_rows(ui, "project-settings", &info["spaces"], &current, None, &info["resolvedColor"]) {
        f.insert(field.into(), value);
    }
    ui.add_space(10.0);
    monitor_line(app, ui);
}

/// `album.import` params from the Import dialog's fields.
pub fn import_params(f: &Map<String, Value>) -> Result<Value, String> {
    let album = f.get("album").and_then(Value::as_u64).ok_or("choose an album")?;
    let paths: Vec<Value> = f.get("paths").and_then(Value::as_array).cloned().unwrap_or_default();
    if paths.is_empty() {
        return Err("no files to import".into());
    }
    let mode = match f.get("mode").and_then(Value::as_str) {
        Some("copy") => "copy",
        _ => "reference",
    };
    Ok(json!({"album": album, "paths": paths, "mode": mode}))
}

/// Notice lines for an `album.import` / `album.export` result: one per skipped or failed file.
pub fn result_lines(r: &Value) -> Vec<String> {
    r["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|x| matches!(x["status"].as_str(), Some("skipped" | "failed")))
        .map(|x| {
            let path = str_of(&x["path"]);
            let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
            format!("{name}: {}", str_of(&x["reason"]))
        })
        .collect()
}

pub fn confirm(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let u = |k: &str| f.get(k).and_then(Value::as_u64);
    match kind(f) {
        "newAlbum" => {
            let r = app.run("album.new", json!({"name": s("name")}))?;
            if let Some(id) = r["id"].as_u64() {
                let lib = &mut app.ui.library;
                lib.album = Some(id);
                lib.photos.clear();
                lib.focus = Focus::Album;
            }
            Ok(r)
        }
        "renameAlbum" => app.run("album.rename", json!({"id": u("album"), "name": s("name")})),
        "deleteAlbum" => {
            let id = u("album");
            let r = app.run("album.delete", json!({"id": id}))?;
            if app.ui.library.album == id {
                app.ui.library = LibraryUi { rebuild: std::mem::take(&mut app.ui.library.rebuild), ..Default::default() };
            }
            Ok(r)
        }
        "removePhotos" => {
            let ids: Vec<u64> = f.get("photos").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
            for id in &ids {
                app.run("photo.remove", json!({"id": id}))?;
            }
            app.ui.library.photos.retain(|p| !ids.contains(p));
            Ok(json!({"removed": ids}))
        }
        "import" => {
            let mut f = f.clone();
            if f.get("album").and_then(Value::as_u64).is_none() {
                let name = Some(s("newAlbum")).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| tl!("Imported").to_string());
                let id = app.run("album.new", json!({"name": name}))?["id"].clone();
                f.insert("album".into(), id);
            }
            let params = import_params(&f)?;
            let r = app.run("album.import", params.clone())?;
            if let Some(a) = params["album"].as_u64() {
                app.ui.library.album = Some(a);
                app.ui.library.focus = Focus::Album;
            }
            let n = r["imported"].as_u64().unwrap_or(0);
            let title = crate::i18n::trn(crate::i18n::current(), n, "Imported {n} photo", "Imported {n} photos");
            let lines = result_lines(&r);
            app.ui.status = title.clone();
            app.ui.status_error = false;
            if !lines.is_empty() || n == 0 {
                crate::notices::post(app, title, lines, n == 0);
            }
            Ok(r)
        }
        "export" => {
            let mut p = json!({"album": u("album"), "folder": s("folder"), "format": s("format"), "overwrite": f.get("overwrite").and_then(Value::as_bool).unwrap_or(false)});
            if matches!(s("format").as_str(), "jpeg" | "webp") {
                p["quality"] = f.get("quality").cloned().unwrap_or(json!(10));
            }
            export(app, p)
        }
        "settings" => {
            let orig = f.get("__orig").cloned().unwrap_or(json!({}));
            for (field, _) in FIELDS {
                let now = f.get(field).cloned().unwrap_or(Value::Null);
                let was = orig.get(field).cloned().unwrap_or(Value::Null);
                if now != was {
                    apply_color(app, &Target::Project, field, &now)?;
                }
            }
            Ok(Value::Null)
        }
        other => Err(format!("unknown Library dialog `{other}`")),
    }
}

/// Exports an album: on a worker thread with background jobs on, else inline.
fn export(app: &mut PhotocraftApp, params: Value) -> Result<Value, String> {
    #[cfg(not(target_arch = "wasm32"))]
    if app.background_jobs {
        if app.library.export.is_some() {
            return Err(tl!("An album export is already running").into());
        }
        let plan = photocraft_engine::project_cmds::export_plan(&app.session, &params).map_err(|e| e.to_string())?;
        let n = plan.len();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("album-export".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| plan.run()))
                    .map_err(|_| "the export crashed".to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()));
                let _ = tx.send(r);
            })
            .map_err(|e| format!("couldn't start the export: {e}"))?;
        app.library.export = Some(rx);
        app.ui.status = crate::i18n::trn(crate::i18n::current(), n as u64, "Exporting {n} photo…", "Exporting {n} photos…");
        app.ui.status_error = false;
        return Ok(json!({"started": n}));
    }
    let r = app.run("album.export", params);
    export_done(app, r.clone());
    r
}

fn export_done(app: &mut PhotocraftApp, r: Result<Value, String>) {
    match r {
        Ok(r) => {
            let n = r["exported"].as_u64().unwrap_or(0);
            let title = crate::i18n::fmt(
                &crate::i18n::trn(crate::i18n::current(), n, "Exported {n} photo to {folder}", "Exported {n} photos to {folder}"),
                &[("folder", str_of(&r["folder"]))],
            );
            let lines = result_lines(&r);
            app.ui.status = title.clone();
            app.ui.status_error = false;
            crate::notices::post(app, title, lines, r["failed"].as_u64().unwrap_or(0) > 0);
        }
        Err(e) => crate::notices::error(app, e),
    }
}

#[cfg(test)]
#[path = "library_ui_tests.rs"]
mod tests;
