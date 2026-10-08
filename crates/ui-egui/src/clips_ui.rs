//! PhotoVision's Clips bar: a filmstrip of the current album in the Edit module (like the Clips
//! strip of DaVinci Resolve's Color page).
//!
//! Docked under the canvas, above the status bar (between the Tools rail and the panel dock,
//! which keep their full height), while a project is open and the
//! Edit module shows (the Library has its own grid; full-screen mode hides it with the other
//! chrome). One tile per photo of the *current album*: the album picked in the bar's header
//! (until another photo becomes active), else the active photo's album, else the album
//! selected in the Library. Each tile: the thumbnail (aspect kept, letterboxed), `01 name`,
//! the Edited / Missing badges and a dot when the photo is open with unsaved changes; the
//! active photo has an accent border.
//!
//! Clicking a tile shows that photo ([`go_to`]): its tab when it is open, else `photo.open`.
//! The photo shown before is closed when it has no unsaved changes, so memory stays bounded
//! (one photo open at a time, as Resolve shows one clip); a dirty one stays open.
//! `view.clips.next` / `view.clips.prev` (⌘→ / ⌘←, like Lightroom's filmstrip; plain arrows
//! nudge with the Move tool, and Photoshop binds nothing to ⌘ + arrow) step through the album
//! and **stop at its ends** (no wrap-around, so holding the key can't loop past the last
//! photo unnoticed). `window.toggle.clips` (Window › Clips) shows or hides the bar.
//!
//! State: [`crate::state::Panels`] (`clips`, `clipsCollapsed`, `clipsHeight`) and
//! [`ClipsUi`] (the header's album pick), both serialised with the UI state. Thumbnails come
//! from the Library's cache and worker ([`library_ui::thumb_texture`]): off the UI thread, and
//! only for the tiles in view.

use egui::{Align, Rect, RichText, Sense, Stroke, StrokeKind, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::library_ui::{self, Focus, Module};
use crate::theme::Tokens;
use crate::widgets;

/// The Clips bar's own state (serialised with the UI state; `ui.inspect` reports it).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ClipsUi {
    /// The album picked in the header, shown instead of the active photo's album.
    pub album: Option<u64>,
    /// The active photo when `album` was picked: the pick lasts until another photo is shown.
    pub picked_with: Option<u64>,
}

/// Height of the bar (points) when never resized, and the limits of its draggable top edge.
pub const DEFAULT_HEIGHT: f32 = 150.0;
pub const MIN_HEIGHT: f32 = 96.0;
pub const MAX_HEIGHT: f32 = 360.0;
/// The header row (album, count, collapse and hide); all that shows when collapsed.
pub const HEADER_H: f32 = 30.0;
/// Under each thumbnail: `01  name`.
const LABEL_H: f32 = 18.0;
const GAP: f32 = 8.0;
/// Thumbnail boxes are 3:2 (the common camera aspect); other shapes are letterboxed.
const BOX_ASPECT: f32 = 1.5;

fn active_photo(app: &PhotocraftApp) -> Option<u64> {
    app.session.active()?.project_photo
}

/// The album the bar shows (see the module docs), if a project is open and has one.
pub fn current_album(app: &PhotocraftApp) -> Option<u64> {
    let st = app.session.project.as_ref()?;
    let albums = &st.project.albums;
    let has = |a: u64| albums.iter().any(|x| x.id == a);
    let active = active_photo(app);
    if let Some(a) = app.ui.clips.album.filter(|a| has(*a))
        && app.ui.clips.picked_with == active
    {
        return Some(a);
    }
    if let Some(p) = active
        && let Some(a) = albums.iter().find(|a| a.photos.iter().any(|x| x.id == p))
    {
        return Some(a.id);
    }
    app.ui.library.album.filter(|a| has(*a)).or_else(|| albums.first().map(|a| a.id))
}

/// The current album's photo ids, in album order.
pub fn album_photos(app: &PhotocraftApp) -> Vec<u64> {
    let Some(a) = current_album(app) else { return Vec::new() };
    app.session
        .project
        .as_ref()
        .and_then(|st| st.project.albums.iter().find(|x| x.id == a))
        .map(|x| x.photos.iter().map(|p| p.id).collect())
        .unwrap_or_default()
}

/// Is the bar on screen? (A project with an album is open, the Edit module shows, Window ›
/// Clips is on and the chrome isn't hidden by full-screen mode.)
pub fn visible(app: &PhotocraftApp) -> bool {
    app.ui.panels.clips && !app.ui.view.hides_chrome() && !library_ui::active(app) && current_album(app).is_some()
}

/// The bar's height: the remembered one, clamped (the header alone when collapsed).
pub fn height(app: &PhotocraftApp) -> f32 {
    if app.ui.panels.clips_collapsed {
        return HEADER_H;
    }
    let h = app.ui.panels.clips_height;
    if h.is_finite() && h > 0.0 { h.clamp(MIN_HEIGHT, MAX_HEIGHT) } else { DEFAULT_HEIGHT }
}

// ------------------------------------------------------------------ navigation

/// Shows photo `id` in the editor: activates its tab when it is open, else opens it
/// (`photo.open`). The project photo shown before is closed when it has no unsaved changes; a
/// dirty one stays open.
pub fn go_to(app: &mut PhotocraftApp, id: u64) -> Result<Value, String> {
    let prev = active_photo(app);
    let r = app.run("photo.open", json!({"id": id}))?;
    app.ui.module = Module::Edit;
    app.ui.chrome.home = None;
    app.jobs.focus = None;
    if let Some(w) = r["warnings"].as_array().filter(|w| !w.is_empty()) {
        let lines: Vec<String> = w.iter().map(|x| library_ui::str_of(x).to_string()).collect();
        crate::notices::io_warnings(app, tl!("Opened photo"), &lines);
    }
    if let Some(p) = prev.filter(|p| *p != id) {
        close_if_clean(app, p);
    }
    Ok(r)
}

/// Closes the document of project photo `photo` if it has nothing to lose: no unsaved changes,
/// no background job, no Free Transform, inline type or pen path in progress. Returns whether
/// it was closed.
pub fn close_if_clean(app: &mut PhotocraftApp, photo: u64) -> bool {
    let Some(i) = app.session.photo_document(photo) else { return false };
    let dirty = app.session.documents().get(i).is_none_or(|d| d.is_dirty());
    if dirty || app.session.has_jobs() || app.ui.transform.is_some() || app.ui.text_edit.is_some() || app.ui.pen.is_some() {
        return false;
    }
    // Views are index-aligned with the documents: drop the closed one's so the others keep theirs.
    let view = (i < app.ui.views.len()).then(|| app.ui.views.remove(i));
    match app.run("file.close", json!({"document": i})) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("clips: couldn't close photo {photo}: {e}");
            if let Some(v) = view {
                // `run` padded the views back to one per document at the end.
                app.ui.views.pop();
                let at = i.min(app.ui.views.len());
                app.ui.views.insert(at, v);
            }
            false
        }
    }
}

/// Steps `delta` photos through the current album from the active photo (from the first or
/// last when none is active). Stops at the ends: `{"moved": false}` there.
pub fn step(app: &mut PhotocraftApp, delta: i64) -> Result<Value, String> {
    let ids = album_photos(app);
    if ids.is_empty() {
        return Err("the current album has no photos".into());
    }
    let cur = active_photo(app).and_then(|p| ids.iter().position(|x| *x == p));
    let target = match cur {
        Some(i) => i64::try_from(i).unwrap_or(i64::MAX).saturating_add(delta),
        None if delta >= 0 => 0,
        None => i64::try_from(ids.len()).unwrap_or(i64::MAX) - 1,
    };
    let Some(&id) = usize::try_from(target).ok().and_then(|t| ids.get(t)) else {
        return Ok(json!({"photo": cur.and_then(|i| ids.get(i)), "moved": false}));
    };
    go_to(app, id)?;
    Ok(json!({"photo": id, "moved": true}))
}

// ------------------------------------------------------------------ menus

/// Shell commands of the bar. `None` when `id` is not ours.
pub fn menu(app: &mut PhotocraftApp, id: &str) -> Option<Result<Value, String>> {
    Some(match id {
        "view.clips.next" => step(app, 1),
        "view.clips.prev" => step(app, -1),
        "window.toggle.clips" => {
            app.ui.panels.clips = !app.ui.panels.clips;
            Ok(json!({"clips": app.ui.panels.clips}))
        }
        _ => return None,
    })
}

/// Enabled state of the bar's commands. `None` when `id` is not ours.
pub fn is_enabled(app: &PhotocraftApp, id: &str) -> Option<bool> {
    Some(match id {
        "view.clips.next" | "view.clips.prev" => !library_ui::active(app) && !album_photos(app).is_empty(),
        "window.toggle.clips" => true,
        _ => return None,
    })
}

/// Window › Clips checkmark.
pub fn checked(app: &PhotocraftApp, id: &str) -> Option<bool> {
    (id == "window.toggle.clips").then_some(app.ui.panels.clips)
}

// ------------------------------------------------------------------ the bar

/// What a tile asked for this frame.
enum Action {
    Show(u64),
    Reveal(u64),
    Remove(u64),
}

/// The Clips bar (a bottom panel; call after the status bar, toolbar and dock so it docks above
/// the status bar between them).
pub fn bar(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if !visible(app) {
        return;
    }
    let Some(aid) = current_album(app) else { return };
    let now = ui.input(|i| i.time);
    let Some(info) = library_ui::info(app, now) else { return };
    let Some(album) = library_ui::album(&info, aid).cloned() else { return };
    let t = Tokens::get(ui.ctx());
    let h = height(app);
    egui::Panel::bottom("clips-bar")
        .exact_size(h)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::NONE.fill(t.dock).inner_margin(egui::Margin { left: 10, right: 10, top: 0, bottom: 4 }))
        .show(ui, |ui| {
            let r = ui.max_rect().expand2(vec2(10.0, 0.0));
            ui.painter().line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, t.separator));
            resize_edge(app, ui, r);
            header(app, ui, &info, &album);
            let action = if app.ui.panels.clips_collapsed { None } else { strip(app, ui, &album) };
            match action {
                Some(Action::Show(id)) => {
                    if let Err(e) = go_to(app, id) {
                        crate::notices::error(app, e);
                    }
                }
                Some(Action::Reveal(id)) => reveal(app, aid, id),
                Some(Action::Remove(id)) => {
                    app.ui.library.photos = vec![id];
                    library_ui::open_remove(app);
                }
                None => {}
            }
        });
}

/// The draggable top edge: resizes the bar between [`MIN_HEIGHT`] and [`MAX_HEIGHT`].
fn resize_edge(app: &mut PhotocraftApp, ui: &mut egui::Ui, r: Rect) {
    if app.ui.panels.clips_collapsed {
        return;
    }
    let edge = Rect::from_min_max(r.left_top(), egui::pos2(r.right(), r.top() + 5.0));
    let resp = ui.interact(edge, ui.id().with("clips-resize"), Sense::drag());
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    if resp.dragged() {
        let dy = resp.drag_delta().y;
        if dy != 0.0 {
            app.ui.panels.clips_height = (height(app) - dy).clamp(MIN_HEIGHT, MAX_HEIGHT);
        }
    }
}

fn header(app: &mut PhotocraftApp, ui: &mut egui::Ui, info: &Value, album: &Value) {
    let t = Tokens::get(ui.ctx());
    let Some(aid) = album["id"].as_u64() else { return };
    let n = album["photos"].as_array().map_or(0, Vec::len);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_H), Sense::hover());
    ui.scope_builder(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::left_to_right(Align::Center)), |ui| {
        ui.label(RichText::new(tl!("Clips")).font(crate::theme::semibold(13.0)).color(t.text_dim));
        ui.add_space(6.0);
        let opts: Vec<(Value, &str)> = library_ui::albums(info).iter().map(|a| (a["id"].clone(), library_ui::str_of(&a["name"]))).collect();
        let mut sel = json!(aid);
        if widgets::dropdown(ui, "clips-album", &mut sel, &opts, 180.0)
            && let Some(picked) = sel.as_u64().filter(|a| *a != aid)
        {
            app.ui.clips = ClipsUi { album: Some(picked), picked_with: active_photo(app) };
            app.ui.library.album = Some(picked);
            app.ui.library.photos.clear();
        }
        ui.add_space(4.0);
        ui.label(RichText::new(crate::i18n::trn(crate::i18n::current(), n as u64, "{n} photo", "{n} photos")).color(t.text_faint));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if crate::icons::button(ui, "x", 22.0, false, tl!("Hide Clips")).clicked() {
                app.ui.panels.clips = false;
            }
            let collapsed = app.ui.panels.clips_collapsed;
            let (icon, tip) = if collapsed { ("chevron-up", tl!("Expand Clips")) } else { ("chevron-down", tl!("Collapse Clips")) };
            if crate::icons::button(ui, icon, 22.0, false, tip).clicked() {
                app.ui.panels.clips_collapsed = !collapsed;
            }
        });
    });
}

/// The tiles, in a horizontally scrolling row (the mouse wheel scrolls it too). Only tiles in
/// view are painted and ask for thumbnails.
fn strip(app: &mut PhotocraftApp, ui: &mut egui::Ui, album: &Value) -> Option<Action> {
    let photos: Vec<Value> = album["photos"].as_array().cloned().unwrap_or_default();
    if photos.is_empty() {
        let t = Tokens::get(ui.ctx());
        ui.centered_and_justified(|ui| ui.label(RichText::new(tl!("No photos in this album yet")).color(t.text_faint)));
        return None;
    }
    library_ui::begin_thumbs(app, ui.ctx());
    let thumb_h = (ui.available_height() - LABEL_H - 6.0).max(24.0);
    let tile_w = (thumb_h * BOX_ASPECT).round();
    let pitch = tile_w + GAP;
    let current = active_photo(app);
    // Keep the active photo in view when it changes (not on every frame: the user may scroll away).
    let centred_id = ui.id().with("clips-centred");
    // (Keyed by the tile pitch too: a resized bar re-centres.)
    let key = current.map(|c| (c, pitch.to_bits()));
    let centred: Option<(u64, u32)> = ui.data(|d| d.get_temp(centred_id)).flatten();
    let recentre = key.is_some() && centred != key;
    let mut action = None;
    ui.scope(|ui| {
        ui.style_mut().always_scroll_the_only_direction = true;
        egui::ScrollArea::horizontal().id_salt("clips-strip").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
            let origin = ui.max_rect().min;
            let total = photos.len() as f32 * pitch - GAP;
            ui.set_min_size(vec2(total, thumb_h + LABEL_H));
            let first = ((viewport.min.x / pitch).floor().max(0.0)) as usize;
            let last = (((viewport.max.x / pitch).ceil().max(0.0)) as usize + 1).min(photos.len());
            for (i, p) in photos.iter().enumerate().take(last).skip(first) {
                let rect = Rect::from_min_size(origin + vec2(i as f32 * pitch, 2.0), vec2(tile_w, thumb_h + LABEL_H));
                if let Some(a) = tile(app, ui, rect, i, p, current) {
                    action = Some(a);
                }
            }
            if recentre && let Some(i) = photos.iter().position(|p| p["id"].as_u64() == current) {
                let rect = Rect::from_min_size(origin + vec2(i as f32 * pitch, 2.0), vec2(tile_w, thumb_h + LABEL_H));
                ui.scroll_to_rect(rect, Some(Align::Center));
            }
        });
    });
    if recentre {
        ui.data_mut(|d| d.insert_temp(centred_id, key));
    }
    action
}

/// One clip: letterboxed thumbnail, badges, unsaved dot, `01  name`, accent border when shown.
fn tile(app: &mut PhotocraftApp, ui: &mut egui::Ui, rect: Rect, index: usize, p: &Value, current: Option<u64>) -> Option<Action> {
    let t = Tokens::get(ui.ctx());
    let id = p["id"].as_u64()?;
    let name = library_ui::str_of(&p["name"]).to_string();
    let resp = ui.interact(rect, ui.id().with(("clip", id)), Sense::click());
    let shown = current == Some(id);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, shown, &name));
    let img_rect = Rect::from_min_size(rect.min, vec2(rect.width(), rect.height() - LABEL_H));
    let painter = ui.painter_at(rect.expand(1.0));
    painter.rect_filled(img_rect, t.radius_sm, if resp.hovered() { t.hover } else { t.canvas });
    let exists = p["exists"] == json!(true);
    match library_ui::thumb_texture(app, ui.ctx(), p) {
        Some(tex) => {
            let size = tex.size_vec2();
            let room = img_rect.shrink(3.0);
            let k = (room.width() / size.x.max(1.0)).min(room.height() / size.y.max(1.0));
            let r = Rect::from_center_size(room.center(), size * k);
            painter.image(tex.id(), r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        }
        None => {
            let icon = if exists { "image" } else { "triangle-alert" };
            crate::icons::paint(ui, Rect::from_center_size(img_rect.center(), vec2(24.0, 24.0)), icon, 20.0, t.text_faint);
        }
    }
    library_ui::paint_badges(&painter, &t, img_rect.left_top() + vec2(4.0, 4.0), p);
    let dirty = app.session.photo_document(id).and_then(|i| app.session.documents().get(i)).is_some_and(|d| d.is_dirty());
    if dirty {
        painter.circle_filled(egui::pos2(img_rect.right() - 8.0, img_rect.top() + 8.0), 4.0, t.warning);
    }
    if shown {
        painter.rect_stroke(img_rect, t.radius_sm, Stroke::new(2.0, t.accent), StrokeKind::Inside);
    }
    let y = img_rect.bottom() + 3.0;
    let num = painter.layout_no_wrap(format!("{:02}", index + 1), egui::FontId::monospace(10.5), t.text_faint);
    let num_w = num.size().x;
    painter.galley(egui::pos2(rect.left() + 1.0, y + 1.0), num, t.text_faint);
    let mut job = egui::text::LayoutJob::simple_singleline(name.clone(), egui::FontId::proportional(11.5), if shown { t.text } else { t.text_dim });
    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - num_w - 8.0).max(8.0));
    painter.galley(egui::pos2(rect.left() + num_w + 6.0, y), painter.layout_job(job), t.text_dim);
    let mut action = None;
    if resp.clicked() || resp.double_clicked() {
        action = Some(Action::Show(id));
    }
    let tip = if !exists {
        crate::i18n::fmt(tl!("File not found: {path}"), &[("path", library_ui::str_of(&p["file"]))])
    } else if dirty {
        format!("{}\n{}", library_ui::str_of(&p["file"]), tl!("Unsaved changes"))
    } else {
        library_ui::str_of(&p["file"]).to_string()
    };
    let resp = resp.on_hover_text(tip);
    resp.context_menu(|ui| {
        if ui.button(tl!("Open")).clicked() {
            action = Some(Action::Show(id));
            ui.close();
        }
        if ui.button(tl!("Reveal in Library")).clicked() {
            action = Some(Action::Reveal(id));
            ui.close();
        }
        ui.separator();
        if ui.button(tl!("Remove from Project…")).clicked() {
            action = Some(Action::Remove(id));
            ui.close();
        }
    });
    action
}

/// Switches to the Library with the photo selected in its album.
fn reveal(app: &mut PhotocraftApp, album: u64, photo: u64) {
    app.ui.module = Module::Library;
    let lib = &mut app.ui.library;
    lib.album = Some(album);
    lib.photos = vec![photo];
    lib.focus = Focus::Photos;
}

#[cfg(test)]
#[path = "clips_ui_tests.rs"]
mod tests;
