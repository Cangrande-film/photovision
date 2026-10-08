//! PhotoVision's Album Look in the shell (the engine side is `photocraft_engine::album_look`):
//! the Library inspector's Album Look section (album: on/off, the look's layers read-only,
//! Clear; photos: Use album look), the Layers panel badge and tooltip on a photo's look group,
//! and the targets of the Project › Album Look menu items. Everything it changes goes through the
//! engine's `album.look.*`, `photo.setAlbumLook` and `layer.toAlbumLook` / `layer.fromAlbumLook`
//! commands; this module only reads the project and shows it.

use egui::{RichText, vec2};
use photocraft_doc::LayerId;
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::library_ui::{self, Module};
use crate::theme::Tokens;
use crate::widgets;

/// The album the Album Look menu items act on: in Edit, the active photo's album; in the
/// Library, the album shown.
pub fn target_album(app: &PhotocraftApp) -> Option<u64> {
    let st = app.session.project.as_ref()?;
    let from_doc = app.session.active_index().and_then(|i| app.session.album_look_album(i));
    let pick = match app.ui.module {
        Module::Edit => from_doc.or(app.ui.library.album),
        Module::Library => app.ui.library.album.or(from_doc),
    };
    pick.filter(|a| st.project.albums.iter().any(|x| x.id == *a))
}

/// The photo "Use Album Look for This Photo" acts on: in Edit, the active photo; in the Library,
/// the one selected photo.
pub fn target_photo(app: &PhotocraftApp) -> Option<u64> {
    let st = app.session.project.as_ref()?;
    let pick = match app.ui.module {
        Module::Edit => app.session.active().and_then(|d| d.project_photo),
        Module::Library => match app.ui.library.photos.as_slice() {
            [one] => Some(*one),
            _ => None,
        },
    };
    pick.filter(|p| st.project.find_photo(*p).is_ok())
}

fn album_enabled(app: &PhotocraftApp, album: u64) -> Option<bool> {
    app.session.project.as_ref()?.project.album(album).ok().map(|a| a.look_enabled)
}

fn photo_uses_look(app: &PhotocraftApp, photo: u64) -> Option<bool> {
    app.session.project.as_ref()?.project.find_photo(photo).ok().map(|(_, p)| p.album_look)
}

/// Project › Album Look items fronted here: they fill in their album or photo and toggle.
/// `None` when not ours or already complete.
pub fn menu(app: &mut PhotocraftApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let has = |k: &str| params.get(k).is_some_and(|v| !v.is_null());
    Some(match id {
        "album.look.setEnabled" if !has("album") || !has("enabled") => {
            let Some(album) = params.get("album").and_then(Value::as_u64).or_else(|| target_album(app)) else {
                return Some(Err(tl!("Open a photo of an album, or select an album in the Library").into()));
            };
            let enabled = params.get("enabled").and_then(Value::as_bool).unwrap_or_else(|| !album_enabled(app, album).unwrap_or(true));
            app.run(id, json!({"album": album, "enabled": enabled}))
        }
        "album.look.clear" if !has("album") => match target_album(app) {
            Some(a) => Ok(json!({"dialog": open_clear(app, a)})),
            None => Err(tl!("Open a photo of an album, or select an album in the Library").into()),
        },
        "photo.setAlbumLook" if !has("id") || !has("enabled") => {
            let Some(photo) = params.get("id").and_then(Value::as_u64).or_else(|| target_photo(app)) else {
                return Some(Err(tl!("Open a project photo, or select one photo in the Library").into()));
            };
            let enabled = params.get("enabled").and_then(Value::as_bool).unwrap_or_else(|| !photo_uses_look(app, photo).unwrap_or(true));
            app.run(id, json!({"id": photo, "enabled": enabled}))
        }
        _ => return None,
    })
}

/// Enabled state of the items above. `None` when not ours.
pub fn is_enabled(app: &PhotocraftApp, id: &str) -> Option<bool> {
    Some(match id {
        "album.look.setEnabled" => target_album(app).is_some(),
        "album.look.clear" => target_album(app).is_some_and(|a| app.session.project.as_ref().and_then(|st| st.looks.get(&a)).is_some_and(|l| !l.is_empty())),
        "photo.setAlbumLook" => target_photo(app).is_some(),
        _ => return None,
    })
}

/// Checkmarks: the album's look is on; the photo uses it.
pub fn checked(app: &PhotocraftApp, id: &str) -> Option<bool> {
    match id {
        "album.look.setEnabled" => Some(target_album(app).and_then(|a| album_enabled(app, a)).unwrap_or(false)),
        "photo.setAlbumLook" => Some(target_photo(app).and_then(|p| photo_uses_look(app, p)).unwrap_or(false)),
        _ => None,
    }
}

/// The confirmation for Clear Album Look (it is not undoable).
pub fn open_clear(app: &mut PhotocraftApp, album: u64) -> u64 {
    library_ui::dialog(app, "clearLook", "Clear Album Look…", json!({"album": album}))
}

// ------------------------------------------------------------------ Layers panel

/// The album look group of the active document, when `layer` is it: its tooltip, "Shared by all
/// N photos in <album>".
pub fn group_tooltip(app: &PhotocraftApp, layer: LayerId) -> Option<String> {
    let i = app.session.active_index()?;
    if app.session.album_look_group(i) != Some(layer) {
        return None;
    }
    let st = app.session.project.as_ref()?;
    let album = st.project.album(app.session.album_look_album(i)?).ok()?;
    let n = album.photos.len();
    let mut tip = crate::i18n::fmt(
        crate::i18n::trn(crate::i18n::current(), n as u64, "Shared by the {n} photo in {album}", "Shared by all {n} photos in {album}").as_str(),
        &[("album", &album.name)],
    );
    tip.push('\n');
    tip.push_str(tl!("The album look applies after this photo's own layers. Edit its layers here and every photo of the album follows; hide it to bypass it for this photo."));
    Some(tip)
}

// ------------------------------------------------------------------ Library inspector

fn heading(ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    ui.add_space(16.0);
    widgets::hairline(ui);
    ui.add_space(12.0);
    ui.label(RichText::new(tl!("Album Look")).font(crate::theme::semibold(15.0)).color(t.text));
    ui.add_space(4.0);
}

fn report(app: &mut PhotocraftApp, r: Result<Value, String>) {
    if let Err(e) = r {
        crate::notices::error(app, e);
    }
}

/// The album level: on/off, the look's layers (top first, read-only) and Clear.
pub fn album_section(app: &mut PhotocraftApp, ui: &mut egui::Ui, info: &Value, album: u64) {
    let t = Tokens::get(ui.ctx());
    let Some(a) = library_ui::album(info, album) else { return };
    let look = &a["look"];
    let name = library_ui::str_of(&a["name"]).to_string();
    heading(ui);
    ui.label(
        RichText::new(crate::i18n::fmt(tl!("Applied to every photo in {album}, after each photo's own edits."), &[("album", &name)]))
            .color(t.text_faint)
            .size(11.5),
    );
    ui.add_space(8.0);
    let mut on = look["enabled"].as_bool().unwrap_or(true);
    if widgets::toggle(ui, &mut on, tl!("Enabled")).on_hover_text(tl!("Show the album look on the album's photos")).changed() {
        let r = app.run("album.look.setEnabled", json!({"album": album, "enabled": on}));
        report(app, r);
    }
    ui.add_space(8.0);
    let layers = look["layers"].as_array().map_or(&[][..], Vec::as_slice);
    if layers.is_empty() {
        ui.label(
            RichText::new(tl!("No layers yet. In Edit, select adjustment layers and choose Move to Album Look (Layers panel menu or Project › Album Look)."))
                .color(t.text_dim)
                .size(12.0),
        );
    } else {
        widgets::section_label(ui, tl!("Layers (top first)"));
        ui.add_space(2.0);
        // As wide as the colour dropdowns above (their width leaves the scrollbar room).
        let w = (ui.available_width() - 32.0).max(120.0);
        egui::Frame::NONE.fill(t.field).corner_radius(t.radius_sm).inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
            ui.set_width(w);
            for l in layers.iter().take(12) {
                ui.horizontal(|ui| {
                    let dim = l["visible"] == json!(false);
                    let (r, _) = ui.allocate_exact_size(vec2(6.0, 6.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 3.0, if dim { t.text_faint } else { t.accent });
                    let name = RichText::new(library_ui::str_of(&l["name"])).color(if dim { t.text_faint } else { t.text }).size(12.5);
                    ui.add(egui::Label::new(name).truncate());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(tl!(library_ui::str_of(&l["kind"]))).color(t.text_faint).size(11.0));
                    });
                });
            }
            if layers.len() > 12 {
                ui.label(RichText::new(crate::i18n::fmt(tl!("…and {n} more"), &[("n", &(layers.len() - 12).to_string())])).color(t.text_faint).size(11.0));
            }
        });
    }
    let bypassed = look["bypassed"].as_array().map_or(0, Vec::len);
    if bypassed > 0 {
        ui.add_space(6.0);
        ui.label(
            RichText::new(crate::i18n::trn(crate::i18n::current(), bypassed as u64, "{n} photo bypasses the look", "{n} photos bypass the look"))
                .color(t.text_dim)
                .size(12.0),
        );
    }
    if let Some(broken) = look["broken"].as_str() {
        ui.add_space(6.0);
        ui.label(RichText::new(broken).color(t.warning).size(11.5));
    }
    ui.add_space(10.0);
    let can_clear = !layers.is_empty();
    if ui.add_enabled_ui(can_clear, |ui| widgets::secondary_button(ui, tl!("Clear"), 0.0)).inner.on_hover_text(tl!("Remove every layer of the album look")).clicked() {
        open_clear(app, album);
    }
}

/// The photo level: Use album look (several photos: set for all; mixed shows unchecked).
pub fn photos_section(app: &mut PhotocraftApp, ui: &mut egui::Ui, info: &Value, ids: &[u64]) {
    let t = Tokens::get(ui.ctx());
    let photos: Vec<&Value> = ids.iter().filter_map(|id| library_ui::albums(info).iter().flat_map(|a| a["photos"].as_array().into_iter().flatten()).find(|p| p["id"].as_u64() == Some(*id))).collect();
    let Some(first) = photos.first() else { return };
    let album = first["album"].as_u64().and_then(|a| library_ui::album(info, a));
    heading(ui);
    let mut on = photos.iter().all(|p| p["albumLook"] != json!(false));
    if widgets::checkbox(ui, &mut on, tl!("Use album look")).on_hover_text(tl!("Off: the album look is bypassed for this photo")).changed() {
        for id in ids {
            let r = app.run("photo.setAlbumLook", json!({"id": id, "enabled": on}));
            report(app, r);
        }
    }
    if let Some(a) = album {
        let n = a["look"]["count"].as_u64().unwrap_or(0);
        let enabled = a["look"]["enabled"].as_bool().unwrap_or(true);
        let line = if n == 0 {
            crate::i18n::fmt(tl!("{album} has no album look yet."), &[("album", library_ui::str_of(&a["name"]))])
        } else if !enabled {
            crate::i18n::fmt(tl!("The album look of {album} is turned off."), &[("album", library_ui::str_of(&a["name"]))])
        } else {
            crate::i18n::fmt(
                crate::i18n::trn(crate::i18n::current(), n, "Album look of {album}: {n} layer", "Album look of {album}: {n} layers").as_str(),
                &[("album", library_ui::str_of(&a["name"]))],
            )
        };
        ui.add_space(4.0);
        ui.label(RichText::new(line).color(t.text_faint).size(11.5));
    }
}

#[cfg(test)]
#[path = "album_look_ui_tests.rs"]
mod tests;
