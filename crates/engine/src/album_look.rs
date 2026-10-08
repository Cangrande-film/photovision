//! PhotoVision album looks: changes applied to every photo of an album at once, like DaVinci
//! Resolve's timeline grade (a grade after every clip's own grade, edited once, applied to all
//! clips), but as ordinary layers.
//!
//! * **Model.** An album's look is a group of adjustment and fill layers ([`AlbumLook`], held in
//!   [`ProjectState::looks`]), saved as a small native bundle `<Project> Looks/album-<id>.pvlook`
//!   that the album references ([`photocraft_project::Album::look`]). The album can switch it
//!   off ([`photocraft_project::Album::look_enabled`]) and each photo can bypass it
//!   ([`photocraft_project::Photo::album_look`]).
//! * **Documents.** An open project photo gets the look as a group at the top of its layers
//!   ("Album Look — <album>", after the photo's own layers: clip grade, then timeline grade),
//!   visible when the album's look is on and the photo doesn't bypass it. The document remembers
//!   it in [`DocState::album_look`]. The group holds real layers: select them, change them in
//!   Properties, add, delete or reorder adjustment layers inside it with the usual commands.
//! * **Sync.** After every command, a document whose group changed copies it into the album
//!   ([`sync`]) and every other open photo of the album gets the new content ([`refresh_doc`]).
//!   Layers a look can't hold (pixels, type, shapes, smart objects) are moved out of the group,
//!   just below it, and pixel masks are removed (photos differ in size), with a notice
//!   ([`Session::notices`]). Hiding the group (its eye) bypasses the look for that photo; deleting
//!   (or merging) it does too, with a notice, so the look itself is never lost.
//! * **Undo.** Undo in the document where the look was edited changes its group again, which syncs
//!   again, so undo works for the look. A document that *receives* the look gets it written into
//!   its current state and its undo/redo states too, without a history step there: undoing an
//!   unrelated edit in it never brings back an older look.
//! * **Saving.** A photo's sidecar is written without the group ([`without_look`]); the look is
//!   written to its `.pvlook` by `photo.save` / File › Save (that album only) and `project.save`
//!   (every changed look). Exports (`album.export`, Save a Copy, Export As) include the look, and
//!   Library thumbnails apply it (their cache key includes [`group_hash`]).

use std::sync::Arc;

use photocraft_doc::{ColorMode, Document, Layer, LayerContent, LayerId, SampleType, Size};
use photocraft_ops::LayerTarget;
use photocraft_project::{self as pj, Album};
use serde_json::{Value, json};

use crate::color_pipeline::{self as cp, ActivePipeline, ColorPipeline};
use crate::commands::CommandSpec;
use crate::file_cmds;
use crate::project_cmds::{self as pc, ProjectState, fs};
use crate::{DocState, EngineError, Result, Session};

/// Deepest group nesting inside a look.
pub const MAX_DEPTH: usize = 8;
/// Most layers directly inside a look.
pub const MAX_LAYERS: usize = 256;
/// Largest `.pvlook` file read.
const MAX_FILE_BYTES: u64 = 64 << 20;
/// Notices kept for the shell ([`Session::take_notices`]).
const MAX_NOTICES: usize = 20;

/// An album's look in memory.
#[derive(Clone, Debug)]
pub struct AlbumLook {
    /// The look's group: its children are the look's layers (bottom to top); its opacity and
    /// blend mode apply to the whole look. Its id and name are not used.
    pub group: Layer,
    /// Bumped on every change.
    pub revision: u64,
    /// Changed since it was last written to its `.pvlook`.
    pub dirty: bool,
    /// The `.pvlook` couldn't be read (the message). The file is left alone until the look changes.
    pub broken: Option<String>,
}

impl AlbumLook {
    pub fn empty() -> AlbumLook {
        AlbumLook { group: Layer::group("Album Look", Vec::new()), revision: 0, dirty: false, broken: None }
    }
    /// The look's layers, bottom to top.
    pub fn layers(&self) -> &[Layer] {
        self.group.children().unwrap_or(&[])
    }
    pub fn is_empty(&self) -> bool {
        self.layers().is_empty()
    }
}

/// An open project photo's album look group (see [`DocState::album_look`]).
#[derive(Clone, Debug)]
pub struct AlbumLookLink {
    /// The album whose look it shows.
    pub album: u64,
    /// The group layer (the same id while the document is open, also after a delete and undo).
    pub group: LayerId,
    /// The group as last synced; `None` while it is missing from the document (deleted, merged).
    synced: Option<Layer>,
    /// The document revision last compared.
    checked: u64,
}

/// The group's name in a document.
pub fn group_name(album: &str) -> String {
    format!("Album Look — {album}")
}

fn empty_group() -> Layer {
    AlbumLook::empty().group
}

fn template_of(st: &ProjectState, album: u64) -> Layer {
    st.looks.get(&album).map_or_else(empty_group, |l| l.group.clone())
}

fn look_is_empty(st: &ProjectState, album: u64) -> bool {
    st.looks.get(&album).is_none_or(AlbumLook::is_empty)
}

/// Does the album look show on `photo` (the album's look is on and the photo doesn't bypass it)?
fn shows(st: &ProjectState, photo: u64) -> Option<(&Album, bool)> {
    let (a, p) = st.project.find_photo(photo).ok()?;
    Some((a, a.look_enabled && p.album_look))
}

/// A fresh copy of the album's look for a document (new layer ids).
fn new_group(st: &ProjectState, album: &Album, visible: bool) -> Layer {
    let mut g = template_of(st, album.id).duplicate();
    g.name = group_name(&album.name);
    g.visible = visible;
    if let LayerContent::Group(grp) = &mut g.content {
        grp.expanded = true;
    }
    g
}

fn notice(s: &mut Session, msg: String) {
    s.notices.push(msg);
    let n = s.notices.len();
    if n > MAX_NOTICES {
        s.notices.drain(..n - MAX_NOTICES);
    }
}

fn photo_name(st: &ProjectState, photo: u64) -> String {
    st.photo_file(photo).map(|f| pj::file_name(&f).to_string()).unwrap_or_else(|_| format!("photo {photo}"))
}

// ------------------------------------------------------------------ what a look may hold

/// Can `l` be part of an album look? Adjustment and fill layers, and groups of them.
pub fn allowed(l: &Layer, depth: usize) -> bool {
    match &l.content {
        LayerContent::Adjustment(_) | LayerContent::Fill(_) => l.video.is_none(),
        LayerContent::Group(g) => depth < MAX_DEPTH && g.artboard.is_none() && g.children.iter().all(|c| allowed(c, depth + 1)),
        _ => false,
    }
}

/// Removes what doesn't carry over between photos of different sizes: pixel masks and fill
/// layers' rendered caches. Counts the masks removed.
fn strip(l: &mut Layer, masks: &mut usize, depth: usize) {
    if l.mask.take().is_some() {
        *masks += 1;
    }
    l.fill_cache = None;
    if depth < MAX_DEPTH
        && let Some(ch) = l.children_mut()
    {
        for c in ch {
            strip(c, masks, depth + 1);
        }
    }
}

fn kind_label(l: &Layer) -> String {
    match &l.content {
        LayerContent::Raster(_) => "pixel".into(),
        LayerContent::Adjustment(a) => a.label().to_string(),
        other => other.kind_name().to_lowercase(),
    }
}

/// Keeps what the look group may hold; returns the layers that must leave it (bottom to top)
/// and notes for the user.
pub(crate) fn sanitize(group: &mut Layer) -> (Vec<Layer>, Vec<String>) {
    let Some(children) = group.children_mut() else { return (Vec::new(), Vec::new()) };
    let all = std::mem::take(children);
    let (mut kept, mut out, mut notes, mut masks) = (Vec::new(), Vec::new(), Vec::new(), 0usize);
    for mut l in all {
        if kept.len() < MAX_LAYERS && allowed(&l, 1) {
            strip(&mut l, &mut masks, 1);
            kept.push(l);
        } else {
            notes.push(format!(
                "“{}” ({}) can't be part of an album look, which holds only adjustment and fill layers: it was moved out of the look into this photo",
                l.name,
                kind_label(&l)
            ));
            out.push(l);
        }
    }
    if masks > 0 {
        notes.push(format!(
            "{masks} pixel mask{} removed from the album look: photos differ in size, so a look can't hold pixel masks",
            if masks == 1 { " was" } else { "s were" }
        ));
    }
    *children = kept;
    (out, notes)
}

/// Inserts `layers` (bottom to top) directly below layer `id`, among its siblings.
fn insert_below(doc: &mut Document, id: LayerId, layers: Vec<Layer>) -> Result<()> {
    let path = doc.path_of(id).ok_or(EngineError::NoLayer(id))?;
    let (&last, parent) = path.split_last().ok_or(EngineError::NoLayer(id))?;
    let siblings = if parent.is_empty() {
        &mut doc.layers
    } else {
        doc.layer_at_mut(parent).and_then(Layer::children_mut).ok_or(EngineError::NoLayer(id))?
    };
    for (k, l) in layers.into_iter().enumerate() {
        let at = last.saturating_add(k).min(siblings.len());
        siblings.insert(at, l);
    }
    Ok(())
}

// ------------------------------------------------------------------ hashing

fn zero_ids(l: &mut Layer, depth: usize) {
    l.id = LayerId(0);
    l.psd_id = None;
    if depth < MAX_DEPTH
        && let Some(ch) = l.children_mut()
    {
        for c in ch {
            zero_ids(c, depth + 1);
        }
    }
}

/// A content hash of a look group (ids, its name and visibility left out); 0 for an empty look.
/// Stable across sessions, so cached thumbnails stay valid until the look changes.
pub fn group_hash(g: &Layer) -> u64 {
    if g.children().is_none_or(<[Layer]>::is_empty) {
        return 0;
    }
    let mut n = g.clone();
    zero_ids(&mut n, 0);
    n.name.clear();
    n.visible = true;
    n.locks = Default::default();
    if let LayerContent::Group(grp) = &mut n.content {
        grp.expanded = true;
    }
    pc::fnv1a(format!("{n:?}").as_bytes()).max(1)
}

fn same_content(a: &Layer, b: &Layer) -> bool {
    a.opacity == b.opacity && a.fill_opacity == b.fill_opacity && a.blend == b.blend && a.children() == b.children()
}

/// The look template stored for the album from a document's group.
fn template_from(g: &Layer) -> Layer {
    let mut t = g.clone();
    t.name = "Album Look".into();
    t.visible = true;
    t.mask = None;
    t
}

// ------------------------------------------------------------------ documents

/// `photo.open`: adds the album's look to the top of `doc`. Returns (album, group) for
/// [`link_opened`].
pub(crate) fn inject_for_open(st: &ProjectState, photo: u64, doc: &mut Document) -> Option<(u64, LayerId)> {
    let (album, visible) = shows(st, photo)?;
    let g = new_group(st, album, visible);
    let id = g.id;
    doc.layers.push(g);
    Some((album.id, id))
}

/// Remembers the injected group on the opened document and targets the photo's own top layer
/// (so new layers go below the look).
pub(crate) fn link_opened(d: &mut DocState, look: Option<(u64, LayerId)>) {
    let Some((album, group)) = look else { return };
    d.album_look = Some(AlbumLookLink { album, group, synced: d.doc.layer(group).cloned(), checked: d.revision });
    let n = d.doc.layers.len();
    if n >= 2
        && d.doc.layers.last().is_some_and(|l| l.id == group)
        && let Some(own) = d.doc.layers.get(n - 2).map(|l| l.id)
    {
        d.active_layer = Some(own);
        d.selected_layers = vec![own];
        d.layer_anchor = Some(own);
        d.history.set_current_layers(LayerTarget { active: Some(own), selected: vec![own] });
    }
}

/// `album.export`: adds the look on top when it shows on the photo.
pub(crate) fn inject_for_export(st: &ProjectState, photo: u64, doc: &mut Document) {
    if let Some((album, true)) = shows(st, photo)
        && !look_is_empty(st, album.id)
    {
        doc.layers.push(new_group(st, album, true));
    }
}

/// The look a photo's thumbnail applies and the photo's pipeline (`None`: no look shows on it).
pub(crate) fn thumbnail_look(st: &ProjectState, photo: u64) -> Option<(Layer, ColorPipeline)> {
    let (album, true) = shows(st, photo)? else { return None };
    if look_is_empty(st, album.id) {
        return None;
    }
    Some((template_of(st, album.id), st.resolved(photo).ok()?))
}

/// A small document showing `doc` (an original or a sidecar's stored thumbnail, in its own
/// profile) with the look applied in the photo's working space: the composite is reduced to
/// `max` pixels first, so a large original costs no more than its thumbnail.
pub(crate) fn thumbnail_with_look(doc: &Document, max: u32, group: &Layer, pipeline: &ColorPipeline) -> Result<Document> {
    let mut g = group.duplicate();
    g.visible = true;
    if !pc::is_rgb(doc) {
        let mut d = doc.clone();
        d.layers.push(g);
        return Ok(d);
    }
    let buf = photocraft_compose::thumbnail_buffer(doc, max);
    let (w, h) = (buf.rect.width().max(1), buf.rect.height().max(1));
    let mut small = Document::new(doc.name.clone(), Size::new(w, h), ColorMode::Rgb, SampleType::F32);
    small.icc_profile = doc.icc_profile.clone();
    let px = file_cmds::buffer_surface(&buf, small.pixel_format());
    small.layers.push(Layer::new("Image", LayerContent::Raster(px)));
    let ap = ActivePipeline::new(pipeline.clone())?;
    cp::apply_to_document(&mut small, None, &ap)?;
    small.layers.push(g);
    Ok(small)
}

/// The document to write to a project photo's sidecar: without its album look group.
pub fn without_look(d: &DocState) -> Arc<Document> {
    match &d.album_look {
        Some(link) if d.doc.layer(link.group).is_some() => {
            let mut doc = (*d.doc).clone();
            doc.remove(link.group);
            Arc::new(doc)
        }
        _ => d.doc.clone(),
    }
}

/// File › Revert of a project photo: the group to put back on top of the re-read sidecar (with
/// the same id, so the document keeps its link), when the document has it now.
pub(crate) fn group_for_revert(s: &Session, index: usize) -> Option<Layer> {
    let d = s.docs.get(index)?;
    let link = d.album_look.as_ref()?;
    d.doc.layer(link.group)?;
    let st = s.project.as_ref()?;
    let (album, visible) = shows(st, d.project_photo?)?;
    let mut g = new_group(st, album, visible);
    g.id = link.group;
    Some(g)
}

/// Gives `new` (fresh ids) the ids of `old` by position, so a refreshed group keeps its layers'
/// identity (selection, panels) when only their settings changed.
fn reuse_ids(new: &mut [Layer], old: &[Layer], depth: usize) {
    for (n, o) in new.iter_mut().zip(old) {
        n.id = o.id;
        if depth < MAX_DEPTH
            && let (Some(nc), Some(oc)) = (n.children_mut(), o.children())
        {
            reuse_ids(nc, oc, depth + 1);
        }
    }
}

/// `cur` (a document's group) with the album's content and the given visibility.
fn rebuilt(cur: &Layer, template: &Layer, name: &str, visible: bool) -> Layer {
    let mut g = cur.clone();
    let mut kids: Vec<Layer> = template.children().unwrap_or(&[]).iter().map(Layer::duplicate).collect();
    reuse_ids(&mut kids, cur.children().unwrap_or(&[]), 1);
    if let Some(ch) = g.children_mut() {
        *ch = kids;
    }
    g.opacity = template.opacity;
    g.fill_opacity = template.fill_opacity;
    g.blend = template.blend;
    g.visible = visible;
    g.name = name.to_string();
    g
}

fn mark_checked(s: &mut Session, j: usize, synced: Option<Layer>) {
    if let Some(d) = s.docs.get_mut(j)
        && let Some(link) = d.album_look.as_mut()
    {
        link.synced = synced;
        link.checked = d.revision;
    }
}

/// What a document's look group should be: the album's content, its name and visibility.
struct Target {
    gid: LayerId,
    template: Layer,
    name: String,
    visible: bool,
    /// Put the group back where it is missing (`photo.setAlbumLook` on a photo whose group was
    /// deleted).
    reinject: bool,
}

impl Target {
    /// `doc` with its look group brought up to date (`None`: nothing to change). Each state is
    /// rebuilt from its own group, so its layer ids stay unique in it.
    fn patch(&self, doc: &Document) -> Option<Document> {
        let next = match doc.layer(self.gid) {
            Some(cur) if cur.is_group() => {
                // Undo states get the visibility too: it mirrors the album's and the photo's
                // flags, and an older one coming back on undo would change them again.
                let g = rebuilt(cur, &self.template, &self.name, self.visible);
                if &g == cur {
                    return None;
                }
                let mut d = doc.clone();
                *d.layer_mut(self.gid)? = g;
                d
            }
            None if self.reinject => {
                let mut g = self.template.duplicate();
                g.id = self.gid;
                g.name = self.name.clone();
                g.visible = self.visible;
                let mut d = doc.clone();
                d.layers.push(g);
                d
            }
            _ => return None,
        };
        Some(next)
    }
}

/// Brings document `j`'s look group up to date in its current state and its undo/redo states
/// (not a history step: the look is shared, see the module docs). A clean document stays clean:
/// its own edits (what its sidecar holds) didn't change.
fn write_group(s: &mut Session, j: usize, t: &Target) {
    let Some(d) = s.docs.get_mut(j) else { return };
    let Some(next) = t.patch(&d.doc) else {
        let synced = d.doc.layer(t.gid).cloned();
        mark_checked(s, j, synced);
        return;
    };
    d.history.rewrite_states(|old| t.patch(old));
    let clean = d.saved_revision == d.revision;
    d.doc = Arc::new(next);
    d.revision += 1;
    if clean {
        d.saved_revision = d.revision;
    }
    d.last_damage = None;
    if d.active_layer.is_none_or(|id| d.doc.layer(id).is_none()) {
        d.active_layer = Some(t.gid);
    }
    crate::fix_selection(d);
    let synced = d.doc.layer(t.gid).cloned();
    mark_checked(s, j, synced);
}

/// Brings open document `j` up to date with its album's look and the photo's flags. A document
/// whose group was removed (bypassed) is left alone unless `reinject`.
pub(crate) fn refresh_doc(s: &mut Session, j: usize, reinject: bool) {
    let Some(st) = s.project.as_ref() else { return };
    let Some(d) = s.docs.get(j) else { return };
    let (Some(photo), Some(link)) = (d.project_photo, d.album_look.as_ref()) else { return };
    let Some((album, visible)) = shows(st, photo) else { return };
    let t = Target { gid: link.group, template: template_of(st, album.id), name: group_name(&album.name), visible, reinject };
    write_group(s, j, &t);
}

/// Refreshes every open photo of `album` but `except`.
pub(crate) fn refresh_album_docs(s: &mut Session, album: u64, except: Option<usize>) {
    for j in 0..s.docs.len() {
        if Some(j) != except && s.docs.get(j).and_then(|d| d.album_look.as_ref()).is_some_and(|l| l.album == album) {
            refresh_doc(s, j, false);
        }
    }
}

/// After every command (see `jobs::after_command`): documents whose look group changed update
/// their album and the album's other open photos.
pub(crate) fn sync(s: &mut Session) {
    if s.project.is_none() {
        return;
    }
    for i in 0..s.docs.len() {
        let due = s.docs.get(i).is_some_and(|d| d.album_look.as_ref().is_some_and(|l| l.checked != d.revision));
        if due {
            sync_doc(s, i);
        }
    }
}

fn sync_doc(s: &mut Session, i: usize) {
    let Some(d) = s.docs.get(i) else { return };
    let (Some(photo), Some(link)) = (d.project_photo, d.album_look.clone()) else { return };
    let Some(st) = s.project.as_ref() else { return };
    let Some((album, _)) = shows(st, photo) else {
        if let Some(d) = s.docs.get_mut(i) {
            d.album_look = None;
        }
        return;
    };
    let (album_id, album_name, enabled) = (album.id, album.name.clone(), album.look_enabled);
    if album_id != link.album {
        if let Some(d) = s.docs.get_mut(i) {
            d.album_look = None;
        }
        return;
    }
    let name = photo_name(st, photo);
    let current = d.doc.layer(link.group).filter(|l| l.is_group()).cloned();
    let Some(mut g) = current else {
        // Deleted or merged: the photo bypasses the look from now on; the look itself stays.
        if link.synced.is_some() {
            set_photo_flag(s, photo, false);
            notice(
                s,
                format!(
                    "The album look no longer applies to {name}: its group was removed. Turn on “Use album look” (Project › Album Look) to bring it back; the look itself is unchanged."
                ),
            );
        }
        mark_checked(s, i, None);
        return;
    };
    let reappeared = link.synced.is_none();
    let eye = link.synced.as_ref().is_some_and(|p| p.visible != g.visible);
    let mut others = false;
    if reappeared || eye {
        // The eye (or undoing a removal) bypasses the look for this photo, or brings it back.
        set_photo_flag(s, photo, g.visible);
        if g.visible && !enabled {
            if let Some(st) = s.project.as_mut()
                && let Ok(a) = st.project.album_mut(album_id)
            {
                a.look_enabled = true;
                st.dirty = true;
            }
            notice(s, format!("The album look of {album_name} was turned back on (for every photo of the album)."));
            others = true;
        }
    }
    let changed = link.synced.as_ref().is_none_or(|p| !same_content(p, &g));
    if changed {
        let (out, notes) = sanitize(&mut g);
        if !out.is_empty() || !notes.is_empty() {
            // Not a history step: undo goes back to before the layers were dragged in.
            if let Some(d) = s.docs.get_mut(i) {
                let mut doc = (*d.doc).clone();
                if let Some(slot) = doc.layer_mut(link.group) {
                    *slot = g.clone();
                }
                if insert_below(&mut doc, link.group, out).is_ok() {
                    d.doc = Arc::new(doc);
                    d.revision += 1;
                    d.last_damage = None;
                    crate::fix_selection(d);
                }
            }
            for n in notes {
                notice(s, n);
            }
        }
        let differs = s.project.as_ref().is_some_and(|st| !same_content(&template_of(st, album_id), &g));
        if differs && let Some(st) = s.project.as_mut() {
            let look = st.looks.entry(album_id).or_insert_with(AlbumLook::empty);
            look.group = template_from(&g);
            look.revision = look.revision.wrapping_add(1);
            look.dirty = true;
            look.broken = None;
            st.dirty = true;
            others = true;
        }
    }
    mark_checked(s, i, Some(g));
    if others {
        refresh_album_docs(s, album_id, Some(i));
    }
}

fn set_photo_flag(s: &mut Session, photo: u64, on: bool) {
    if let Some(st) = s.project.as_mut()
        && let Ok(p) = st.project.photo_mut(photo)
        && p.album_look != on
    {
        p.album_look = on;
        st.dirty = true;
    }
}

impl Session {
    /// The album look group of open document `index`, when it has one.
    pub fn album_look_group(&self, index: usize) -> Option<LayerId> {
        let d = self.docs.get(index)?;
        let link = d.album_look.as_ref()?;
        d.doc.layer(link.group).map(|_| link.group)
    }

    /// The album whose look document `index` shows, when it is a project photo.
    pub fn album_look_album(&self, index: usize) -> Option<u64> {
        self.docs.get(index)?.album_look.as_ref().map(|l| l.album)
    }

    /// What saving document `index` to `path` writes: a project photo's sidecar is written
    /// without its album look group; anything else (Save As, a copy, an export) as it is.
    pub fn document_to_save(&self, index: usize, path: &str) -> Option<Arc<Document>> {
        let d = self.docs.get(index)?;
        let sidecar = d.project_photo.and_then(|p| self.project.as_ref()?.sidecar(p).ok());
        let norm = |p: &str| p.replace('\\', "/");
        Some(if sidecar.is_some_and(|s| norm(&s) == norm(path)) { without_look(d) } else { d.doc.clone() })
    }

    /// Takes the notices commands left for the user (album look changes the user should know
    /// about, such as layers moved out of a look); the shell shows them.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }
}

// ------------------------------------------------------------------ files

fn encode(look: &AlbumLook) -> Result<Vec<u8>> {
    let mut doc = Document::new("Album Look", Size::new(1, 1), ColorMode::Rgb, SampleType::U8);
    doc.layers.push(template_from(&look.group));
    photocraft_format::save_to_bytes(&doc, &Default::default()).map_err(|e| EngineError::Other(format!("album look: {e}")))
}

/// Reads a `.pvlook`: its first group (or its layers) as the look, sanitized.
pub(crate) fn decode(bytes: &[u8]) -> Result<(Layer, Vec<String>)> {
    let opts = photocraft_format::LoadOptions { max_manifest_bytes: 16 << 20, max_blob_bytes: 64 << 20, max_total_bytes: 256 << 20, preserve_ids: false };
    let doc = photocraft_format::load_from_bytes_with(bytes, &opts).map_err(|e| EngineError::Other(format!("not a PhotoVision album look: {e}")))?;
    let mut layers = doc.layers;
    let mut g = match layers.iter().position(Layer::is_group) {
        Some(k) => layers.swap_remove(k),
        None => Layer::group("Album Look", layers),
    };
    let (out, mut notes) = sanitize(&mut g);
    if !out.is_empty() {
        notes.push(format!("{} layer(s) that an album look can't hold were left out", out.len()));
    }
    Ok((template_from(&g), notes))
}

/// `project.open`: reads every album's `.pvlook`. Problems become warnings and an empty look
/// whose file is kept (see [`AlbumLook::broken`]).
pub(crate) fn load_all(st: &mut ProjectState) -> Vec<String> {
    let mut warnings = Vec::new();
    let refs: Vec<(u64, String, String)> =
        st.project.albums.iter().filter_map(|a| a.look.clone().map(|rel| (a.id, a.name.clone(), pj::project_file(&st.path, &rel)))).collect();
    for (album, name, file) in refs {
        let read = || -> Result<(Layer, Vec<String>)> {
            if fs::size(&file).is_some_and(|n| n > MAX_FILE_BYTES) {
                return Err(EngineError::Other("the file is too large".into()));
            }
            decode(&file_cmds::read_file(&file)?)
        };
        let look = match read() {
            Ok((group, notes)) => {
                warnings.extend(notes.into_iter().map(|n| format!("album look of {name}: {n}")));
                AlbumLook { group, revision: 1, dirty: false, broken: None }
            }
            Err(e) => {
                let msg = format!("the album look of {name} ({file}) couldn't be read, so it is empty for now: {e}");
                warnings.push(msg.clone());
                AlbumLook { broken: Some(msg), ..AlbumLook::empty() }
            }
        };
        st.looks.insert(album, look);
    }
    warnings
}

/// Writes changed looks (every album, or just `only`): a look with layers to its `.pvlook`, an
/// emptied one deletes its file. Returns the last file written.
pub(crate) fn write_looks(st: &mut ProjectState, only: Option<u64>) -> Result<Option<String>> {
    let ids: Vec<u64> = st.project.albums.iter().map(|a| a.id).filter(|id| only.is_none_or(|o| o == *id)).collect();
    let mut written = None;
    for id in ids {
        let Some(look) = st.looks.get(&id) else { continue };
        if !look.dirty {
            continue;
        }
        let current = st.project.album(id).map_err(pc::perr)?.look.clone();
        if look.is_empty() {
            if let Some(rel) = current {
                fs::remove_file(&pj::project_file(&st.path, &rel))?;
                st.project.album_mut(id).map_err(pc::perr)?.look = None;
                st.dirty = true;
            }
        } else {
            let rel = current.clone().unwrap_or_else(|| pj::look_rel_path(&st.path, id));
            pj::validate_look_path(&rel).map_err(pc::perr)?;
            let file = pj::project_file(&st.path, &rel);
            let bytes = encode(look)?;
            fs::create_dir_all(pj::dir_of(&file))?;
            file_cmds::write_file(&file, &bytes)?;
            if current.as_deref() != Some(rel.as_str()) {
                st.project.album_mut(id).map_err(pc::perr)?.look = Some(rel);
                st.dirty = true;
            }
            written = Some(file);
        }
        if let Some(look) = st.looks.get_mut(&id) {
            look.dirty = false;
            look.broken = None;
        }
    }
    Ok(written)
}

/// The album's look as `project.info` reports it.
pub(crate) fn summary(st: &ProjectState, album: &Album) -> Value {
    let look = st.looks.get(&album.id);
    let layers: Vec<Value> = look
        .map(|l| l.layers().iter().rev().take(MAX_LAYERS).map(|x| json!({"name": x.name, "kind": kind_label(x), "visible": x.visible})).collect())
        .unwrap_or_default();
    json!({
        "enabled": album.look_enabled,
        "count": layers.len(),
        "layers": layers,
        "hash": format!("{:016x}", look.map_or(0, |l| group_hash(&l.group))),
        "revision": look.map_or(0, |l| l.revision),
        "file": album.look,
        "broken": look.and_then(|l| l.broken.clone()),
        "bypassed": album.photos.iter().filter(|p| !p.album_look).map(|p| p.id).collect::<Vec<_>>(),
        "photos": album.photos.len(),
    })
}

// ------------------------------------------------------------------ commands

fn req_bool(p: &Value, key: &str, cmd: &str) -> Result<bool> {
    match p.get(key) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(pc::bad(cmd, format!("`{key}` must be true or false, got {v}"))),
        None => Err(pc::bad(cmd, format!("missing \"{key}\""))),
    }
}

fn look_info(s: &mut Session, p: &Value) -> Result<Value> {
    let id = pc::id_param(p, "album", "album.look.info")?;
    let st = pc::state(s)?;
    let a = st.project.album(id).map_err(pc::perr)?;
    let mut v = summary(st, a);
    v["album"] = json!(id);
    v["name"] = json!(a.name);
    let open: Vec<usize> = s.docs.iter().enumerate().filter(|(_, d)| d.album_look.as_ref().is_some_and(|l| l.album == id)).map(|(i, _)| i).collect();
    v["documents"] = json!(open);
    Ok(v)
}

fn look_set_enabled(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "album.look.setEnabled";
    let id = pc::id_param(p, "album", cmd)?;
    let enabled = req_bool(p, "enabled", cmd)?;
    let st = pc::state_mut(s)?;
    let a = st.project.album_mut(id).map_err(pc::perr)?;
    if a.look_enabled != enabled {
        a.look_enabled = enabled;
        st.dirty = true;
    }
    refresh_album_docs(s, id, None);
    Ok(json!({"album": id, "enabled": enabled}))
}

fn look_clear(s: &mut Session, p: &Value) -> Result<Value> {
    let id = pc::id_param(p, "album", "album.look.clear")?;
    let st = pc::state_mut(s)?;
    st.project.album(id).map_err(pc::perr)?;
    let look = st.looks.entry(id).or_insert_with(AlbumLook::empty);
    let n = look.layers().len();
    if n > 0 || look.broken.is_some() {
        look.group = empty_group();
        look.revision = look.revision.wrapping_add(1);
        look.dirty = true;
        look.broken = None;
        st.dirty = true;
    }
    refresh_album_docs(s, id, None);
    Ok(json!({"album": id, "cleared": n}))
}

fn photo_set_album_look(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "photo.setAlbumLook";
    let id = pc::id_param(p, "id", cmd)?;
    let enabled = req_bool(p, "enabled", cmd)?;
    pc::state(s)?.project.find_photo(id).map_err(pc::perr)?;
    set_photo_flag(s, id, enabled);
    let doc = s.photo_document(id);
    if let Some(j) = doc {
        refresh_doc(s, j, enabled);
    }
    Ok(json!({"id": id, "enabled": enabled, "document": doc}))
}

/// The active document's look group and its path, for the layer commands.
fn active_group(s: &Session) -> std::result::Result<(LayerId, Vec<usize>), String> {
    let d = s.active().ok_or("no document open")?;
    let link = d.album_look.as_ref().ok_or("the active document is not a project photo with an album look")?;
    let path = d.doc.path_of(link.group).ok_or("the album look is bypassed for this photo (its group was removed): turn on Use Album Look first")?;
    Ok((link.group, path))
}

/// Selected layers inside (`inside`) or outside the look group, topmost-only (a selected group's
/// selected children are left out), bottom to top.
fn selected_relative(s: &Session, inside: bool) -> std::result::Result<(LayerId, Vec<LayerId>), String> {
    let (gid, gpath) = active_group(s)?;
    let d = s.active().ok_or("no document open")?;
    let in_group = |p: &[usize]| p.len() > gpath.len() && p.starts_with(&gpath);
    // A group holding the look group can't move into it.
    let ancestor = |p: &[usize]| p.len() < gpath.len() && gpath.starts_with(p);
    let picked: Vec<(LayerId, Vec<usize>)> = d
        .selected_layers()
        .into_iter()
        .filter_map(|id| d.doc.path_of(id).map(|p| (id, p)))
        .filter(|(id, p)| *id != gid && !ancestor(p) && in_group(p) == inside)
        .collect();
    let top: Vec<LayerId> =
        picked.iter().filter(|(_, p)| !picked.iter().any(|(_, q)| q.len() < p.len() && p.starts_with(q))).map(|(id, _)| *id).collect();
    if top.is_empty() {
        return Err(if inside { "select layers inside the album look group".into() } else { "select the photo's adjustment layers to move into its album look".into() });
    }
    Ok((gid, top))
}

fn can_to_look(s: &Session) -> std::result::Result<(), String> {
    selected_relative(s, false).map(|_| ())
}

fn can_from_look(s: &Session) -> std::result::Result<(), String> {
    selected_relative(s, true).map(|_| ())
}

fn layer_to_album_look(s: &mut Session, _: &Value) -> Result<Value> {
    let (gid, ids) = selected_relative(s, false).map_err(EngineError::Other)?;
    let d = s.active().ok_or(EngineError::NoDocument)?;
    for id in &ids {
        let l = d.doc.layer(*id).ok_or(EngineError::NoLayer(*id))?;
        if !allowed(l, 1) {
            return Err(EngineError::Other(format!(
                "“{}” is a {} layer: only adjustment and fill layers (and groups of them) can join an album look",
                l.name,
                kind_label(l)
            )));
        }
    }
    let album = d.album_look.as_ref().map(|l| l.album);
    let (moved, masks) = s.edit("Move to Album Look", |doc, active| {
        let mut moved = Vec::with_capacity(ids.len());
        let mut masks = 0usize;
        for id in &ids {
            let mut l = doc.remove(*id).ok_or(EngineError::NoLayer(*id))?;
            strip(&mut l, &mut masks, 1);
            moved.push(l);
        }
        let n = moved.len();
        let kids = doc.layer_mut(gid).and_then(Layer::children_mut).ok_or(EngineError::NoLayer(gid))?;
        if kids.len().saturating_add(n) > MAX_LAYERS {
            return Err(EngineError::Other(format!("an album look holds at most {MAX_LAYERS} layers")));
        }
        kids.extend(moved);
        *active = ids.last().copied();
        Ok((n, masks))
    })?;
    if let Some(d) = s.active_mut() {
        d.selected_layers = ids.clone();
        crate::fix_selection(d);
    }
    let mut warnings = Vec::new();
    if masks > 0 {
        warnings.push(format!("{masks} pixel mask(s) removed: photos differ in size, so a look can't hold pixel masks"));
    }
    Ok(json!({"moved": moved, "album": album, "warnings": warnings}))
}

fn layer_from_album_look(s: &mut Session, _: &Value) -> Result<Value> {
    let (gid, ids) = selected_relative(s, true).map_err(EngineError::Other)?;
    let copied = s.edit("Copy from Album Look", |doc, active| {
        let copies: Vec<Layer> = ids.iter().filter_map(|id| doc.layer(*id)).map(Layer::duplicate).collect();
        let top = copies.last().map(|l| l.id);
        let n = copies.len();
        insert_below(doc, gid, copies)?;
        *active = top.or(*active);
        Ok(n)
    })?;
    Ok(json!({"copied": copied}))
}

macro_rules! spec {
    ($id:literal, $label:literal, $params:literal, $en:expr, $run:expr, $journal:expr) => {
        CommandSpec { id: $id, label: $label, menu: &[], shortcut: None, params: $params, enabled: $en, run: $run, journal: $journal }
    };
}

/// Album look commands (Project › Album Look and the Layers panel's context menu).
pub fn specs() -> Vec<CommandSpec> {
    vec![
        spec!(
            "album.look.info",
            "Album Look Info",
            r#"{"album":id} → {"album","name","enabled","count","layers":[{"name","kind","visible"}] (top first),"hash","revision","file","broken","bypassed":[photo ids],"photos":n,"documents":[open document indices]}"#,
            pc::has_project,
            look_info,
            false
        ),
        spec!(
            "album.look.setEnabled",
            "Enable Album Look",
            r#"{"album":id,"enabled":bool} → {"album","enabled"} (open photos of the album show or hide the look)"#,
            pc::has_project,
            look_set_enabled,
            true
        ),
        spec!(
            "album.look.clear",
            "Clear Album Look",
            r#"{"album":id} → {"album","cleared":n} (removes every layer of the look from the album and its open photos; the .pvlook is deleted on the next save)"#,
            pc::has_project,
            look_clear,
            true
        ),
        spec!(
            "photo.setAlbumLook",
            "Use Album Look",
            r#"{"id":photo,"enabled":bool} → {"id","enabled","document"} (false bypasses the album look for this photo, like disabling the timeline grade for one clip; true brings it back, also when its group was deleted)"#,
            pc::has_project,
            photo_set_album_look,
            true
        ),
        spec!(
            "layer.toAlbumLook",
            "Move to Album Look",
            r#"{} → {"moved":n,"album","warnings"} (the selected adjustment / fill layers of the active project photo move to the top of its album look and so apply to every photo of the album; one undo step)"#,
            can_to_look,
            layer_to_album_look,
            true
        ),
        spec!(
            "layer.fromAlbumLook",
            "Copy from Album Look",
            r#"{} → {"copied":n} (copies the selected layers of the album look group into this photo only, just below the group; the look is unchanged)"#,
            can_from_look,
            layer_from_album_look,
            true
        ),
    ]
}

#[cfg(test)]
#[path = "album_look_tests.rs"]
mod tests;
