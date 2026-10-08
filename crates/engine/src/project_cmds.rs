//! Projects, albums and photos (PhotoVision's Library): `project.*`, `album.*`, `photo.*`.
//!
//! The data model is `photocraft_project` (pure data, JSON); this module adds the file system and
//! the documents. One project is open at a time ([`Session::project`]). A photo opens through its
//! resolved colour pipeline (photo over album over project settings); its edits are saved as a
//! sidecar next to the original (`IMG_0001.jpg.pvision`, the native bundle format under
//! PhotoVision's extension; the original is never written), and an
//! open photo remembers which photo it is ([`crate::DocState::project_photo`]), so File › Save
//! writes the sidecar.
//!
//! File-system commands are unavailable on the web (their `enabled` predicates say so and the
//! file helpers return an error there).

use std::collections::{BTreeMap, HashSet};

use photocraft_cms::ColorSpace;
use photocraft_doc::Document;
use photocraft_project::{self as pj, Added, Level, Project};
use serde_json::{Value, json};

use crate::color_cmds::mode_space;
use crate::album_look::{self, AlbumLook};
use crate::color_pipeline::{self as cp, ActivePipeline, ColorOverride, ColorPipeline, InputSpace};
use crate::commands::CommandSpec;
use crate::file_cmds::{self, native};
use crate::{EngineError, Result, Session};

/// The open project.
#[derive(Clone, Debug)]
pub struct ProjectState {
    pub project: Project,
    /// The project file (absolute when it came from the file system).
    pub path: String,
    /// Changed since it was last saved.
    pub dirty: bool,
    /// Album looks by album id, loaded on `project.open` and written on `project.save` and
    /// `photo.save` (see [`crate::album_look`]). An album without an entry has an empty look.
    pub looks: BTreeMap<u64, AlbumLook>,
}

impl ProjectState {
    /// A project state that is not dirty and has no album looks loaded yet.
    pub fn new(project: Project, path: String) -> ProjectState {
        ProjectState { project, path, dirty: false, looks: BTreeMap::new() }
    }

    /// The file a photo refers to (managed paths resolved against the project folder).
    pub fn photo_file(&self, photo: u64) -> Result<String> {
        let (_, p) = self.project.find_photo(photo).map_err(perr)?;
        Ok(pj::photo_file(&self.path, p))
    }

    /// The photo's edit sidecar (`<original>.pcraft`).
    pub fn sidecar(&self, photo: u64) -> Result<String> {
        self.photo_file(photo).map(|f| pj::sidecar_path(&f))
    }

    /// The photo's effective colour pipeline.
    pub fn resolved(&self, photo: u64) -> Result<ColorPipeline> {
        self.project.resolve_color(photo, &ColorPipeline::default()).map_err(perr)
    }
}

impl Session {
    /// The document index showing project photo `photo`, if it is open.
    pub fn photo_document(&self, photo: u64) -> Option<usize> {
        self.documents().iter().position(|d| d.project_photo == Some(photo))
    }

    /// The sidecar File › Save writes for the active document, when it is a project photo.
    pub fn active_photo_sidecar(&self) -> Option<String> {
        let photo = self.active()?.project_photo?;
        self.project.as_ref()?.sidecar(photo).ok()
    }

    /// Forget project photos that are no longer in the open project (closed project, removed photo).
    fn unlink_stale_photos(&mut self) {
        let keep: HashSet<u64> = self.project.iter().flat_map(|p| p.project.albums.iter().flat_map(|a| a.photos.iter().map(|p| p.id))).collect();
        for d in self.docs.iter_mut() {
            if d.project_photo.is_some_and(|id| !keep.contains(&id)) {
                d.project_photo = None;
                d.album_look = None;
            }
        }
    }
}

// ------------------------------------------------------------------ helpers

pub(crate) fn perr(e: pj::ProjectError) -> EngineError {
    EngineError::Other(e.to_string())
}

pub(crate) fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn no_project() -> EngineError {
    EngineError::Other("no project open (project.new or project.open)".into())
}

pub(crate) fn state(s: &Session) -> Result<&ProjectState> {
    s.project.as_ref().ok_or_else(no_project)
}

pub(crate) fn state_mut(s: &mut Session) -> Result<&mut ProjectState> {
    s.project.as_mut().ok_or_else(no_project)
}

pub(crate) fn id_param(p: &Value, key: &str, cmd: &str) -> Result<u64> {
    match p.get(key) {
        Some(v) => v
            .as_u64()
            .or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0 && f.fract() == 0.0 && *f < 9.0e15).map(|f| f as u64))
            .ok_or_else(|| bad(cmd, format!("`{key}` must be a non-negative integer id, got {v}"))),
        None => Err(bad(cmd, format!("missing \"{key}\""))),
    }
}

fn str_param<'a>(p: &'a Value, key: &str, cmd: &str) -> Result<&'a str> {
    match p.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s),
        Some(Value::String(_)) => Err(bad(cmd, format!("\"{key}\" is empty"))),
        Some(v) => Err(bad(cmd, format!("\"{key}\" must be a string, got {v}"))),
        None => Err(bad(cmd, format!("missing \"{key}\""))),
    }
}

fn bool_param(p: &Value, key: &str, cmd: &str) -> Result<bool> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(bad(cmd, format!("`{key}` must be a bool, got {v}"))),
    }
}

pub(crate) fn is_rgb(doc: &Document) -> bool {
    mode_space(doc.mode) == ColorSpace::Rgb
}

// ------------------------------------------------------------------ file system (native only)

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod fs {
    use crate::{EngineError, Result};
    use std::path::Path;

    pub fn is_file(p: &str) -> bool {
        Path::new(p).is_file()
    }
    pub fn exists(p: &str) -> bool {
        Path::new(p).exists()
    }
    pub fn size(p: &str) -> Option<u64> {
        std::fs::metadata(p).ok().map(|m| m.len())
    }
    /// Modification time (nanoseconds since the epoch) and size.
    pub fn stamp(p: &str) -> Option<(u128, u64)> {
        let m = std::fs::metadata(p).ok()?;
        let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
        Some((t, m.len()))
    }
    pub fn create_dir_all(p: &str) -> Result<()> {
        std::fs::create_dir_all(p).map_err(|e| EngineError::Other(format!("cannot create folder {p}: {e}")))
    }
    pub fn copy(from: &str, to: &str) -> Result<()> {
        if let Err(e) = std::fs::copy(from, to) {
            let _ = std::fs::remove_file(to);
            return Err(EngineError::Other(format!("cannot copy {from} to {to}: {e}")));
        }
        Ok(())
    }
    pub fn absolute(p: &str) -> String {
        std::path::absolute(p).map(|a| a.to_string_lossy().into_owned()).unwrap_or_else(|_| p.to_string())
    }
    pub fn now_secs() -> Option<u64> {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
    }
    pub fn remove_file(p: &str) -> Result<()> {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(EngineError::Other(format!("cannot delete {p}: {e}"))),
            _ => Ok(()),
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) mod fs {
    use crate::{EngineError, Result};

    fn web(p: &str) -> EngineError {
        EngineError::Other(format!("{p}: projects need a file system; not available on the web"))
    }
    pub fn is_file(_: &str) -> bool {
        false
    }
    pub fn exists(_: &str) -> bool {
        false
    }
    pub fn size(_: &str) -> Option<u64> {
        None
    }
    pub fn stamp(_: &str) -> Option<(u128, u64)> {
        None
    }
    pub fn create_dir_all(p: &str) -> Result<()> {
        Err(web(p))
    }
    pub fn copy(from: &str, _: &str) -> Result<()> {
        Err(web(from))
    }
    pub fn absolute(p: &str) -> String {
        p.to_string()
    }
    pub fn now_secs() -> Option<u64> {
        None
    }
    pub fn remove_file(p: &str) -> Result<()> {
        Err(web(p))
    }
}

fn read_project(path: &str) -> Result<Project> {
    if fs::size(path).is_some_and(|n| n > pj::MAX_JSON_BYTES as u64) {
        return Err(EngineError::Other(format!("{path}: project file is too large")));
    }
    let bytes = file_cmds::read_file(path)?;
    let text = String::from_utf8(bytes).map_err(|_| EngineError::Other(format!("{path}: not a PhotoVision project file (not UTF-8 text)")))?;
    Project::from_json(&text).map_err(|e| EngineError::Other(format!("{path}: {e}")))
}

fn write_project(st: &ProjectState) -> Result<()> {
    let text = st.project.to_json().map_err(perr)?;
    file_cmds::write_file(&st.path, text.as_bytes())
}

// ------------------------------------------------------------------ predicates

pub(crate) fn has_project(s: &Session) -> std::result::Result<(), String> {
    native(s)?;
    s.project.as_ref().map(|_| ()).ok_or_else(|| "no project open".into())
}

fn has_project_photo(s: &Session) -> std::result::Result<(), String> {
    has_project(s)?;
    let d = s.active().ok_or("no document open")?;
    d.project_photo.map(|_| ()).ok_or_else(|| "the active document is not a project photo".into())
}

// ------------------------------------------------------------------ info

fn photo_json(st: &ProjectState, s: &Session, a: &pj::Album, p: &pj::Photo) -> Value {
    let file = pj::photo_file(&st.path, p);
    let sidecar = pj::sidecar_path(&file);
    let resolved = st.project.resolve_color(p.id, &ColorPipeline::default()).ok();
    json!({
        "id": p.id,
        "album": a.id,
        "path": p.path,
        "file": file,
        "name": pj::file_name(&file),
        "managed": p.managed,
        "added": p.added,
        "color": p.color,
        "resolvedColor": resolved,
        "sidecar": sidecar,
        "albumLook": p.album_look,
        "hasSidecar": fs::is_file(&sidecar),
        "exists": fs::is_file(&file),
        "document": s.photo_document(p.id),
    })
}

fn info_json(s: &Session) -> Result<Value> {
    let st = state(s)?;
    let base = ColorPipeline::default();
    let albums: Vec<Value> = st
        .project
        .albums
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "color": a.color,
                "resolvedColor": st.project.resolve_level(Level::Album(a.id), &base).ok(),
                "mediaDir": pj::media_dir(&st.path, &a.name),
                "look": album_look::summary(st, a),
                "photos": a.photos.iter().map(|p| photo_json(st, s, a, p)).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "path": st.path,
        "name": st.project.name,
        "dirty": st.dirty,
        "version": st.project.version,
        "color": st.project.color,
        "resolvedColor": st.project.resolve_level(Level::Project, &base).ok(),
        "mediaRoot": pj::media_root(&st.path),
        "thumbCache": pj::thumb_cache_dir(&st.path),
        "albums": albums,
        "spaces": cp::spaces_json(),
    }))
}

// ------------------------------------------------------------------ project.*

fn project_path_param(p: &Value, cmd: &str) -> Result<String> {
    let path = str_param(p, "path", cmd)?;
    if path.contains('\0') {
        return Err(bad(cmd, "path contains NUL"));
    }
    let path = if file_cmds::extension(path).as_deref() == Some(pj::EXTENSION) { path.to_string() } else { format!("{path}.{}", pj::EXTENSION) };
    Ok(fs::absolute(&path))
}

fn check_discard(s: &Session, p: &Value, cmd: &str) -> Result<()> {
    if s.project.as_ref().is_some_and(|st| st.dirty) && !bool_param(p, "discard", cmd)? {
        return Err(EngineError::Other("the open project has unsaved changes: save it (project.save) or pass \"discard\": true".into()));
    }
    Ok(())
}

fn set_project(s: &mut Session, st: ProjectState) {
    s.project = Some(st);
    s.unlink_stale_photos();
}

fn project_new(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "project.new";
    let path = project_path_param(p, cmd)?;
    let overwrite = bool_param(p, "overwrite", cmd)?;
    check_discard(s, p, cmd)?;
    if fs::exists(&path) && !overwrite {
        return Err(EngineError::Other(format!("{path} already exists: open it (project.open) or pass \"overwrite\": true")));
    }
    let name = match p.get("name") {
        None | Some(Value::Null) => pj::file_stem(&path).to_string(),
        Some(Value::String(n)) => n.clone(),
        Some(v) => return Err(bad(cmd, format!("`name` must be a string, got {v}"))),
    };
    let st = ProjectState::new(Project::new(&name), path);
    write_project(&st)?;
    set_project(s, st);
    info_json(s)
}

fn project_open(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "project.open";
    let path = fs::absolute(str_param(p, "path", cmd)?);
    check_discard(s, p, cmd)?;
    let project = read_project(&path)?;
    let mut st = ProjectState::new(project, path);
    let warnings = album_look::load_all(&mut st);
    set_project(s, st);
    let mut info = info_json(s)?;
    info["lookWarnings"] = json!(warnings);
    Ok(info)
}

fn project_save(s: &mut Session, _: &Value) -> Result<Value> {
    let st = state_mut(s)?;
    album_look::write_looks(st, None)?;
    write_project(st)?;
    st.dirty = false;
    Ok(json!({"path": st.path}))
}

fn project_close(s: &mut Session, p: &Value) -> Result<Value> {
    state(s)?;
    check_discard(s, p, "project.close")?;
    let st = s.project.take().ok_or_else(no_project)?;
    s.unlink_stale_photos();
    Ok(json!({"closed": st.path, "discarded": st.dirty}))
}

fn project_info(s: &mut Session, _: &Value) -> Result<Value> {
    info_json(s)
}

// ------------------------------------------------------------------ project.setColor

const FIELDS: [&str; 5] = ["input", "working", "output", "intent", "bpc"];

fn level_param(p: &Value, cmd: &str) -> Result<Level> {
    match str_param(p, "level", cmd)? {
        "project" => Ok(Level::Project),
        "album" => Ok(Level::Album(id_param(p, "id", cmd)?)),
        "photo" => Ok(Level::Photo(id_param(p, "id", cmd)?)),
        other => Err(bad(cmd, format!("unknown level `{other}` (project|album|photo)"))),
    }
}

/// Re-applies a project photo's resolved pipeline to its open document: a new working space
/// converts it (one undoable step), output/intent/BPC only change the viewer and exports, and a
/// new input space can't be applied to edited pixels (reported as `needsRebuild`).
fn refresh_document(s: &mut Session, index: usize, photo: u64) -> Result<Value> {
    let resolved = state(s)?.resolved(photo)?;
    let Some(id) = s.documents().get(index).map(|d| d.doc.id) else { return Ok(Value::Null) };
    let Some(cur) = s.color.pipeline(id).cloned() else {
        return Ok(json!({"document": index, "photo": photo, "skipped": "the document has no colour pipeline (not RGB)"}));
    };
    let mut next = resolved;
    let needs_rebuild = next.input != cur.pipeline.input;
    if needs_rebuild {
        next.input = cur.pipeline.input.clone();
    }
    let ap = ActivePipeline::new(next)?;
    let mut converted = false;
    if ap.pipeline.working != cur.pipeline.working {
        let prev = s.active_index();
        s.set_active(index);
        let r = s.edit("Convert to Photo Space", |doc, _| cp::apply_to_document(doc, None, &ap));
        if let Some(i) = prev {
            s.set_active(i);
        }
        converted = r?;
    }
    s.color.set_pipeline(id, Some(ap));
    Ok(json!({"document": index, "photo": photo, "converted": converted, "needsRebuild": needs_rebuild}))
}

fn set_color(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "project.setColor";
    state(s)?;
    let level = level_param(p, cmd)?;
    let field = str_param(p, "field", cmd)?;
    if !FIELDS.contains(&field) {
        return Err(bad(cmd, format!("unknown field `{field}` (input|working|output|intent|bpc)")));
    }
    let value = p.get("value").ok_or_else(|| bad(cmd, "missing \"value\" (null = inherit)"))?;
    let parsed = ColorOverride::from_params(&json!({ field: value })).map_err(|m| bad(cmd, m))?;
    // Custom profile files must be readable RGB profiles.
    for space in
        [parsed.working.as_ref(), parsed.output.as_ref(), parsed.input.as_ref().and_then(|i| if let InputSpace::Space(s) = i { Some(s) } else { None })]
            .into_iter()
            .flatten()
    {
        cp::profile(space)?;
    }
    let st = state_mut(s)?;
    let o = st.project.color_mut(level).map_err(perr)?;
    match field {
        "input" => o.input = parsed.input,
        "working" => o.working = parsed.working,
        "output" => o.output = parsed.output,
        "intent" => o.intent = parsed.intent,
        _ => o.bpc = parsed.bpc,
    }
    st.dirty = true;
    let affected: Vec<(usize, u64)> = s
        .documents()
        .iter()
        .enumerate()
        .filter_map(|(i, d)| d.project_photo.map(|ph| (i, ph)))
        .filter(|(_, ph)| match level {
            Level::Project => true,
            Level::Album(a) => s.project.as_ref().and_then(|st| st.project.find_photo(*ph).ok()).is_some_and(|(al, _)| al.id == a),
            Level::Photo(id) => *ph == id,
        })
        .collect();
    let mut docs = Vec::new();
    for (i, ph) in affected {
        docs.push(refresh_document(s, i, ph)?);
    }
    let needs_rebuild = docs.iter().any(|d| d["needsRebuild"] == json!(true));
    let st = state(s)?;
    let color = st.project.color_of(level).map_err(perr)?;
    Ok(json!({"field": field, "color": color, "documents": docs, "needsRebuild": needs_rebuild}))
}

// ------------------------------------------------------------------ album.*

fn clear_open(s: &mut Session, photos: &[u64]) {
    for d in s.docs.iter_mut() {
        if d.project_photo.is_some_and(|id| photos.contains(&id)) {
            d.project_photo = None;
            d.album_look = None;
        }
    }
}

fn album_new(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_param(p, "name", "album.new")?.to_string();
    let st = state_mut(s)?;
    let id = st.project.add_album(&name).map_err(perr)?;
    st.dirty = true;
    Ok(json!({"id": id, "name": name.trim()}))
}

fn album_rename(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "album.rename";
    let id = id_param(p, "id", cmd)?;
    let name = str_param(p, "name", cmd)?.to_string();
    let st = state_mut(s)?;
    st.project.rename_album(id, &name).map_err(perr)?;
    st.dirty = true;
    // Open photos name their look group after the album.
    album_look::refresh_album_docs(s, id, None);
    Ok(json!({"id": id, "name": name.trim()}))
}

fn album_delete(s: &mut Session, p: &Value) -> Result<Value> {
    let id = id_param(p, "id", "album.delete")?;
    let st = state_mut(s)?;
    let a = st.project.delete_album(id).map_err(perr)?;
    st.looks.remove(&id);
    st.dirty = true;
    let ids: Vec<u64> = a.photos.iter().map(|p| p.id).collect();
    clear_open(s, &ids);
    Ok(json!({"deleted": id, "photos": ids.len()}))
}

/// At most this many paths per `album.import` call.
const MAX_IMPORT: usize = 100_000;

fn importable(path: &str) -> std::result::Result<(), &'static str> {
    // Edits (sidecars and other native bundles), not photos.
    if pj::is_sidecar(path) || file_cmds::extension(path).is_some_and(|x| photocraft_io::is_native_extension(&x)) {
        return Err("sidecar");
    }
    if !file_cmds::extension(path).is_some_and(|x| file_cmds::OPENABLE.contains(&x.as_str())) {
        return Err("not an image type PhotoVision opens");
    }
    if !fs::is_file(path) {
        return Err("file not found");
    }
    Ok(())
}

fn album_import(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "album.import";
    let album = id_param(p, "album", cmd)?;
    let copy = match str_param(p, "mode", cmd)? {
        "reference" => false,
        "copy" => true,
        other => return Err(bad(cmd, format!("unknown mode `{other}` (reference|copy)"))),
    };
    let paths = p.get("paths").and_then(Value::as_array).ok_or_else(|| bad(cmd, "\"paths\" must be an array of file paths"))?;
    if paths.len() > MAX_IMPORT {
        return Err(bad(cmd, format!("at most {MAX_IMPORT} paths per import")));
    }
    let st = state(s)?;
    let album_name = st.project.album(album).map_err(perr)?.name.clone();
    let project_path = st.path.clone();
    let dir = pj::media_dir(&project_path, &album_name);
    let added = fs::now_secs().map(pj::rfc3339_utc);
    let mut results = Vec::with_capacity(paths.len().min(4096));
    let (mut imported, mut skipped) = (0usize, 0usize);
    for v in paths {
        let Some(src) = v.as_str() else {
            results.push(json!({"path": v, "status": "skipped", "reason": "not a string"}));
            skipped += 1;
            continue;
        };
        let r = import_one(s, album, src, copy, &dir, &project_path, added.as_deref());
        match r {
            Ok((id, stored)) => {
                imported += 1;
                results.push(json!({"path": src, "status": "imported", "id": id, "stored": stored}));
            }
            Err(reason) => {
                skipped += 1;
                results.push(json!({"path": src, "status": "skipped", "reason": reason}));
            }
        }
    }
    if imported > 0 {
        state_mut(s)?.dirty = true;
    }
    Ok(json!({"album": album, "mode": if copy { "copy" } else { "reference" }, "imported": imported, "skipped": skipped, "results": results}))
}

/// Imports one file; `Err` is the reason it was skipped.
fn import_one(
    s: &mut Session,
    album: u64,
    src: &str,
    copy: bool,
    dir: &str,
    project_path: &str,
    added: Option<&str>,
) -> std::result::Result<(u64, String), String> {
    importable(src).map_err(str::to_string)?;
    let stored = if copy {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let name = pj::file_name(src);
        // Re-importing the same file (same name and size, already in the album) is a duplicate.
        let existing = pj::join(dir, name);
        let st = state(s).map_err(|e| e.to_string())?;
        if let Some(rel) = pj::relative_to_project(project_path, &existing)
            && fs::size(&existing) == fs::size(src)
            && let Some(ph) = st.project.album(album).ok().and_then(|a| a.photos.iter().find(|p| p.managed && p.path == rel))
        {
            return Err(format!("already copied into the album (photo {})", ph.id));
        }
        let unique = pj::unique_name(name, |n| fs::exists(&pj::join(dir, n)));
        let dest = pj::join(dir, &unique);
        let rel = pj::relative_to_project(project_path, &dest).ok_or_else(|| format!("{dest} is outside the project folder"))?;
        fs::copy(src, &dest).map_err(|e| e.to_string())?;
        rel
    } else {
        fs::absolute(src)
    };
    let st = state_mut(s).map_err(|e| e.to_string())?;
    match st.project.add_photos(album, std::slice::from_ref(&stored), copy, added).map_err(|e| e.to_string())?.first() {
        Some(Added::New(id)) => Ok((*id, stored)),
        Some(Added::Duplicate(id)) => Err(format!("already in the album (photo {id})")),
        None => Err("not added".into()),
    }
}

// ------------------------------------------------------------------ photo.*

fn photo_remove(s: &mut Session, p: &Value) -> Result<Value> {
    let id = id_param(p, "id", "photo.remove")?;
    let st = state_mut(s)?;
    st.project.remove_photo(id).map_err(perr)?;
    st.dirty = true;
    clear_open(s, &[id]);
    Ok(json!({"removed": id}))
}

fn photo_relink(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "photo.relink";
    let id = id_param(p, "id", cmd)?;
    let path = str_param(p, "path", cmd)?;
    state(s)?.project.find_photo(id).map_err(perr)?;
    importable(path).map_err(|why| EngineError::Other(format!("{path}: {why}")))?;
    let abs = fs::absolute(path);
    let st = state_mut(s)?;
    st.project.relink(id, &abs, false).map_err(perr)?;
    st.dirty = true;
    Ok(json!({"id": id, "path": abs}))
}

/// A photo decoded for opening or export: the sidecar when it exists, else the original.
struct Loaded {
    doc: Document,
    from_sidecar: bool,
    original: String,
    sidecar: String,
    warnings: Vec<String>,
}

fn decode(path: &str) -> Result<(Document, Vec<String>)> {
    let bytes = file_cmds::read_file(path)?;
    let r = photocraft_io::import(pj::file_name(path), &bytes).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    Ok((r.document, r.warnings))
}

/// The sidecar to read for `original`: `.pvision`, else a legacy `.pcraft` one, if either exists.
fn existing_sidecar(original: &str) -> Option<String> {
    [pj::sidecar_path(original), pj::legacy_sidecar_path(original)].into_iter().find(|p| fs::is_file(p))
}

fn load_photo(st: &ProjectState, photo: u64) -> Result<Loaded> {
    let original = st.photo_file(photo)?;
    let sidecar = pj::sidecar_path(&original);
    let found = existing_sidecar(&original);
    let from_sidecar = found.is_some();
    let (mut doc, warnings) = if let Some(found) = &found {
        decode(found)?
    } else if fs::is_file(&original) {
        decode(&original)?
    } else {
        return Err(EngineError::Other(format!("{original} is missing: relink the photo (photo.relink)")));
    };
    doc.name = pj::file_name(&original).to_string();
    Ok(Loaded { doc, from_sidecar, original, sidecar, warnings })
}

/// Brings a loaded photo into its pipeline's working space. A sidecar is already interpreted
/// (its embedded profile is the working space it was saved in), so only the working space applies.
fn apply_pipeline(l: &mut Loaded, pipeline: &ColorPipeline) -> Result<(ActivePipeline, bool)> {
    let ap = ActivePipeline::new(pipeline.clone())?;
    let input = if l.from_sidecar { None } else { cp::input_profile(&pipeline.input)? };
    let converted = cp::apply_to_document(&mut l.doc, input.as_deref(), &ap)?;
    Ok((ap, converted))
}

fn photo_open(s: &mut Session, p: &Value) -> Result<Value> {
    let id = id_param(p, "id", "photo.open")?;
    state(s)?.project.find_photo(id).map_err(perr)?;
    if let Some(i) = s.photo_document(id) {
        s.set_active(i);
        return Ok(json!({"document": i, "photo": id, "alreadyOpen": true}));
    }
    let st = state(s)?;
    let pipeline = st.resolved(id)?;
    let mut l = load_photo(st, id)?;
    let look = album_look::inject_for_open(st, id, &mut l.doc);
    let (i, color) = if is_rgb(&l.doc) {
        let (ap, converted) = apply_pipeline(&mut l, &pipeline)?;
        let report = cp::report(&ap, &l.doc, converted);
        let i = s.add_document(l.doc, Some(l.sidecar.clone()));
        if let Some(doc_id) = s.documents().get(i).map(|d| d.doc.id) {
            s.color.set_pipeline(doc_id, Some(ap));
        }
        (i, report)
    } else {
        let (i, mut r) = s.open_document(l.doc, Some(l.sidecar.clone()));
        r["pipeline"] = json!("skipped");
        r["reason"] = json!("colour pipelines apply to RGB documents");
        (i, r)
    };
    if let Some(d) = s.docs.get_mut(i) {
        d.project_photo = Some(id);
        album_look::link_opened(d, look);
    }
    Ok(json!({
        "document": i,
        "photo": id,
        "fromSidecar": l.from_sidecar,
        "sidecar": l.sidecar,
        "original": l.original,
        "pipeline": pipeline,
        "color": color,
        "warnings": l.warnings,
    }))
}

/// Writes the active photo's sidecar without its album look (the look lives in the album's
/// `.pvlook`, written here too when it changed).
fn photo_save(s: &mut Session, _: &Value) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let photo = d.project_photo.ok_or_else(|| EngineError::Other("the active document is not a project photo".into()))?;
    let sidecar = state(s)?.sidecar(photo)?;
    let doc = album_look::without_look(d);
    let warnings = file_cmds::save_doc(&doc, &sidecar, None)?;
    let look = save_photo_look(s)?;
    let st = s.active_mut().ok_or(EngineError::NoDocument)?;
    st.path = Some(sidecar.clone());
    st.saved_revision = st.revision;
    Ok(json!({"path": sidecar, "photo": photo, "warnings": warnings, "look": look}))
}

/// After the active project photo's sidecar was written (`photo.save`, or the shell's File ›
/// Save): writes its album's look file if it changed. Returns the look file written, if any.
pub fn save_photo_look(s: &mut Session) -> Result<Option<String>> {
    let Some(album) = s.active().and_then(|d| d.album_look.as_ref()).map(|l| l.album) else { return Ok(None) };
    let st = state_mut(s)?;
    album_look::write_looks(st, Some(album))
}

// ------------------------------------------------------------------ album.export

fn export_ext(format: &str) -> Option<&'static str> {
    match format.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => Some("jpg"),
        "png" => Some("png"),
        "tiff" | "tif" => Some("tif"),
        "webp" => Some("webp"),
        _ => None,
    }
}

fn export_one(st: &ProjectState, photo: u64, out: &str, quality: Option<f64>) -> Result<Vec<String>> {
    let pipeline = st.resolved(photo)?;
    let mut l = load_photo(st, photo)?;
    let target = if is_rgb(&l.doc) { Some(apply_pipeline(&mut l, &pipeline)?.0.export_target()) } else { None };
    // The album look applies after the photo's own edits, in its working space.
    album_look::inject_for_export(st, photo, &mut l.doc);
    let (bytes, warnings) = file_cmds::encode_to(&l.doc, out, quality, target)?;
    file_cmds::write_file(out, &bytes)?;
    Ok(warnings)
}

/// A validated `album.export`, detached from the session: [`ExportPlan::run`] needs no
/// [`Session`], so a UI can run it on a worker thread while the user keeps working.
#[derive(Clone, Debug)]
pub struct ExportPlan {
    state: ProjectState,
    album: u64,
    photos: Vec<u64>,
    folder: String,
    ext: &'static str,
    overwrite: bool,
    quality: Option<f64>,
}

impl ExportPlan {
    /// Number of photos it will export.
    pub fn len(&self) -> usize {
        self.photos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.photos.is_empty()
    }

    /// Exports every photo (the `album.export` result).
    pub fn run(self) -> Result<Value> {
        let st = &self.state;
        fs::create_dir_all(&self.folder)?;
        let mut taken: HashSet<String> = HashSet::new();
        let mut results = Vec::with_capacity(self.photos.len());
        let (mut exported, mut skipped, mut failed) = (0usize, 0usize, 0usize);
        for &id in &self.photos {
            let original = st.photo_file(id)?;
            let name = pj::unique_name(&format!("{}.{}", pj::file_stem(&original), self.ext), |n| taken.contains(&n.to_lowercase()));
            taken.insert(name.to_lowercase());
            let out = pj::join(&self.folder, &name);
            if fs::exists(&out) && !self.overwrite {
                skipped += 1;
                results.push(json!({"photo": id, "path": out, "status": "skipped", "reason": "file exists (pass \"overwrite\": true)"}));
                continue;
            }
            match export_one(st, id, &out, self.quality) {
                Ok(warnings) => {
                    exported += 1;
                    results.push(json!({"photo": id, "path": out, "status": "exported", "warnings": warnings}));
                }
                Err(e) => {
                    failed += 1;
                    results.push(json!({"photo": id, "path": out, "status": "failed", "reason": e.to_string()}));
                }
            }
        }
        Ok(json!({"album": self.album, "folder": self.folder, "exported": exported, "skipped": skipped, "failed": failed, "results": results}))
    }
}

/// Validates `album.export` params against the open project (see [`ExportPlan`]).
pub fn export_plan(s: &Session, p: &Value) -> Result<ExportPlan> {
    let cmd = "album.export";
    let album = id_param(p, "album", cmd)?;
    let folder = str_param(p, "folder", cmd)?.to_string();
    let format = str_param(p, "format", cmd)?;
    let ext = export_ext(format).ok_or_else(|| bad(cmd, format!("unknown format `{format}` (jpeg|png|tiff|webp)")))?;
    let overwrite = bool_param(p, "overwrite", cmd)?;
    let quality = match p.get("quality") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_f64().filter(|q| q.is_finite()).ok_or_else(|| bad(cmd, "`quality` must be a number (0–12)"))?),
    };
    let st = state(s)?;
    let photos: Vec<u64> = st.project.album(album).map_err(perr)?.photos.iter().map(|p| p.id).collect();
    Ok(ExportPlan { state: st.clone(), album, photos, folder, ext, overwrite, quality })
}

/// Exports every photo of an album, flattened, in its resolved Output space. Runs synchronously
/// and leaves the open documents alone (each photo is decoded on its own).
fn album_export(s: &mut Session, p: &Value) -> Result<Value> {
    export_plan(s, p)?.run()
}

// ------------------------------------------------------------------ photo.rebuild

/// Re-reads an open photo's original through its resolved pipeline (a changed Input space can't
/// be applied to pixels already decoded) and replaces the bottom layer's pixels with it, keeping
/// every layer above. One undoable step. Refuses when the edit changed the canvas size or mode
/// (the layers above would no longer line up) or the bottom layer is not a pixel layer.
fn photo_rebuild(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "photo.rebuild";
    let id = id_param(p, "id", cmd)?;
    let st = state(s)?;
    st.project.find_photo(id).map_err(perr)?;
    let index =
        s.photo_document(id).ok_or_else(|| EngineError::Other(format!("photo {id} is not open (photo.open applies the current settings when it opens)")))?;
    let pipeline = st.resolved(id)?;
    let original = st.photo_file(id)?;
    if !fs::is_file(&original) {
        return Err(EngineError::Other(format!("{original} is missing: relink the photo (photo.relink)")));
    }
    let (mut fresh, warnings) = decode(&original)?;
    if !is_rgb(&fresh) {
        return Err(EngineError::Other("colour pipelines apply to RGB photos; this original is not RGB".into()));
    }
    let ap = ActivePipeline::new(pipeline)?;
    let input = cp::input_profile(&ap.pipeline.input)?;
    cp::apply_to_document(&mut fresh, input.as_deref(), &ap)?;
    let d = s.documents().get(index).ok_or(EngineError::NoDocument)?;
    let doc_id = d.doc.id;
    let (cur, new) = (d.doc.size, fresh.size);
    if (cur.width, cur.height) != (new.width, new.height) {
        return Err(EngineError::Other(format!(
            "the original is {}×{} px but the open photo is {}×{} px (cropped or resized): rebuilding would misalign its layers; close it without saving and open it again",
            new.width, new.height, cur.width, cur.height
        )));
    }
    if d.doc.mode != fresh.mode {
        return Err(EngineError::Other(format!("the open photo is {:?}, the original {:?}: convert it back to RGB first", d.doc.mode, fresh.mode)));
    }
    if !matches!(d.doc.layers.first().map(|l| &l.content), Some(photocraft_doc::LayerContent::Raster(_))) {
        return Err(EngineError::Other("the bottom layer of the open photo is not a pixel layer, so there is nothing to rebuild".into()));
    }
    if fresh.layers.len() != 1 {
        return Err(EngineError::Other(format!("the original has {} layers; rebuilding needs a flat original", fresh.layers.len())));
    }
    let Some(content) = fresh.layers.pop().map(|l| l.content) else {
        return Err(EngineError::Other("the original has no pixels".into()));
    };
    let (depth, icc) = (fresh.depth, fresh.icc_profile.clone());
    let prev = s.active_index();
    s.set_active(index);
    let r = s.edit("Rebuild from Original", |doc, _| {
        if doc.depth != depth {
            crate::image_cmds::set_depth(doc, depth);
        }
        doc.icc_profile = icc;
        let bottom = doc.layers.first_mut().ok_or_else(|| EngineError::Other("the photo has no layers".into()))?;
        bottom.content = content;
        Ok(())
    });
    if let Some(i) = prev {
        s.set_active(i);
    }
    r?;
    let pipeline = ap.pipeline.clone();
    s.color.set_pipeline(doc_id, Some(ap));
    Ok(json!({"document": index, "photo": id, "pipeline": pipeline, "warnings": warnings}))
}

// ------------------------------------------------------------------ photo.thumbnail

const RAW_EXTENSIONS: &[&str] = &["dng", "cr2", "cr3", "nef", "nrw", "arw", "pef", "orf", "raf", "rw2", "srw"];

/// Bump when thumbnails render differently, so cached ones regenerate (v2: converted to sRGB).
const THUMB_VERSION: u32 = 2;

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// A small document to thumbnail: the sidecar's stored thumbnail (in the sidecar's profile;
/// else the sidecar), a raw's embedded preview (else the developed raw), or the decoded file.
fn thumb_source(source: &str, sidecar: bool) -> Result<Document> {
    let bytes = file_cmds::read_file(source)?;
    if sidecar {
        if let Some(png) = photocraft_format::read_thumbnail(&bytes).ok().flatten()
            && let Ok(r) = photocraft_io::import("thumbnail.png", &png)
        {
            let mut doc = r.document;
            // The stored thumbnail is the composite in the document's own profile (a linear
            // ACEScg sidecar's looks dark as plain sRGB): tag it so it converts for display.
            doc.icc_profile = photocraft_format::read_icc_profile(&bytes).ok().flatten().map(std::sync::Arc::new);
            return Ok(doc);
        }
    } else if file_cmds::extension(source).is_some_and(|x| RAW_EXTENSIONS.contains(&x.as_str()))
        && let Some(pv) = photocraft_raw::embedded_preview(&bytes)
        && let Ok(r) = photocraft_io::import("preview.jpg", pv.jpeg)
    {
        return Ok(r.document);
    }
    photocraft_io::import(pj::file_name(source), &bytes).map(|r| r.document).map_err(|e| EngineError::Other(format!("{source}: {e}")))
}

/// The thumbnail in sRGB (what the Library grid shows and other apps assume of a PNG): the
/// composite converted from the document's profile, so wide-gamut and linear photos look right.
fn display_thumbnail(doc: &Document, max: u32) -> photocraft_raster::Rgba8Image {
    let mut buf = photocraft_compose::thumbnail_buffer(doc, max);
    let src = crate::color_cmds::composite_profile(doc);
    let srgb = photocraft_cms::Builtin::Srgb.profile();
    if !src.same_colors(srgb)
        && let Ok(t) = photocraft_cms::Transform::new(&src, srgb, photocraft_cms::Intent::RelativeColorimetric, true)
    {
        t.apply(buf.px.as_flattened_mut(), 4);
    }
    buf.to_rgba8()
}

fn encode_png(img: &photocraft_raster::Rgba8Image) -> Result<Vec<u8>> {
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
    let image = Image::from_raw(img.width, img.height, ChannelLayout::Rgba, SampleType::U8, img.pixels.clone())
        .map_err(|e| EngineError::Other(format!("thumbnail: {e}")))?;
    photocraft_codecs::encode(&image, Format::Png, &EncodeOptions::default()).map_err(|e| EngineError::Other(format!("thumbnail: {e}")))
}

/// Where a photo's thumbnail is (or will be) cached, and how to render it. Planning is cheap
/// (a file stat); [`ThumbPlan::render`] and [`ThumbPlan::load`] decode the photo and need no
/// [`Session`], so a UI runs them on a worker thread.
#[derive(Clone, Debug)]
pub struct ThumbPlan {
    pub photo: u64,
    /// The cached PNG (sRGB).
    pub path: String,
    /// The PNG was already there when planned.
    pub cached: bool,
    source: String,
    from_sidecar: bool,
    max: u32,
    /// The photo's Input space when it overrides the file's own profile (originals only).
    input: Option<std::sync::Arc<photocraft_cms::Profile>>,
    /// The album look to apply (its group) and the photo's pipeline, whose working space the
    /// look is applied in; `None` when no look applies to the photo.
    look: Option<(photocraft_doc::Layer, ColorPipeline)>,
}

impl ThumbPlan {
    /// Writes the PNG unless it is cached.
    pub fn render(&self) -> Result<()> {
        if fs::is_file(&self.path) {
            return Ok(());
        }
        let mut doc = thumb_source(&self.source, self.from_sidecar)?;
        if let Some(p) = &self.input
            && is_rgb(&doc)
        {
            doc.icc_profile = Some(p.to_bytes());
        }
        if let Some((group, pipeline)) = &self.look {
            doc = album_look::thumbnail_with_look(&doc, self.max, group, pipeline)?;
        }
        let png = encode_png(&display_thumbnail(&doc, self.max))?;
        if let Some(dir) = self.path.rfind(['/', '\\']).and_then(|i| self.path.get(..i)) {
            fs::create_dir_all(dir)?;
        }
        file_cmds::write_file(&self.path, &png)
    }

    /// Renders the PNG if needed and decodes it (straight-alpha sRGB RGBA8).
    pub fn load(&self) -> Result<photocraft_raster::Rgba8Image> {
        self.render()?;
        let bytes = file_cmds::read_file(&self.path)?;
        let r = photocraft_io::import("thumbnail.png", &bytes).map_err(|e| EngineError::Other(format!("{}: {e}", self.path)))?;
        let d = &r.document;
        let longest = d.size.width.max(d.size.height).max(1);
        Ok(photocraft_compose::thumbnail(d, longest))
    }
}

/// Plans photo `id`'s thumbnail at most `max` pixels on its longer side (see [`ThumbPlan`]).
pub fn thumbnail_plan(s: &Session, id: u64, max: u32) -> Result<ThumbPlan> {
    let max = max.clamp(16, 2048);
    let st = state(s)?;
    let original = st.photo_file(id)?;
    let sidecar = existing_sidecar(&original);
    let from_sidecar = sidecar.is_some();
    let input = match (&st.resolved(id)?.input, from_sidecar) {
        (InputSpace::Space(sp), false) => cp::profile(sp).ok(),
        _ => None,
    };
    let source = sidecar.unwrap_or(original);
    let (mtime, size) = fs::stamp(&source).ok_or_else(|| EngineError::Other(format!("{source} is missing: relink the photo (photo.relink)")))?;
    let input_key = input.as_ref().map_or(0, |p| p.content_hash());
    // The album look is part of the picture: a changed look gives a new cache key.
    let look = album_look::thumbnail_look(st, id);
    let look_key = look.as_ref().map_or(0, |(g, _)| album_look::group_hash(g));
    let key = fnv1a(format!("v{THUMB_VERSION}\n{source}\n{mtime}\n{size}\n{max}\n{input_key}\n{look_key:x}").as_bytes());
    let dir = pj::thumb_cache_dir(&st.path);
    let path = pj::join(&dir, &format!("{key:016x}.png"));
    let cached = fs::is_file(&path);
    Ok(ThumbPlan { photo: id, path, cached, source, from_sidecar, max, input, look })
}

fn photo_thumbnail(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "photo.thumbnail";
    let id = id_param(p, "id", cmd)?;
    let max = match p.get("maxSide") {
        None | Some(Value::Null) => 256,
        Some(v) => v.as_f64().filter(|f| f.is_finite()).ok_or_else(|| bad(cmd, "`maxSide` must be a number"))?.clamp(16.0, 2048.0) as u32,
    };
    let plan = thumbnail_plan(s, id, max)?;
    plan.render()?;
    Ok(json!({"photo": id, "path": plan.path, "cached": plan.cached}))
}

// ------------------------------------------------------------------ specs

macro_rules! spec {
    ($id:literal, $label:literal, $params:literal, $en:expr, $run:expr, $journal:expr) => {
        CommandSpec { id: $id, label: $label, menu: &[], shortcut: None, params: $params, enabled: $en, run: $run, journal: $journal }
    };
}

/// Project, album and photo commands. No menu items yet: the Library UI (phase 4) drives them.
pub fn specs() -> Vec<CommandSpec> {
    vec![
        spec!(
            "project.new",
            "New Project",
            r#"{"path":"<file>.pvproj","name"?:string,"overwrite":bool=false,"discard":bool=false} → project.info (writes the file; refuses an existing file without overwrite, and an unsaved open project without discard)"#,
            native,
            project_new,
            true
        ),
        spec!("project.open", "Open Project", r#"{"path":"<file>.pvproj","discard":bool=false} → project.info"#, native, project_open, true),
        spec!("project.save", "Save Project", r#"{} → {"path"} (atomic write)"#, has_project, project_save, true),
        spec!(
            "project.close",
            "Close Project",
            r#"{"discard":bool=false} → {"closed":path,"discarded":bool} (refuses unsaved changes without discard; open photos stay open)"#,
            has_project,
            project_close,
            true
        ),
        spec!(
            "project.info",
            "Project Info",
            r#"{} → {"path","name","dirty","version","color":{…},"resolvedColor":{…},"mediaRoot","thumbCache","spaces":{…},"albums":[{"id","name","color","resolvedColor","mediaDir","photos":[{"id","album","path","file","name","managed","added","color","resolvedColor","sidecar","hasSidecar","exists","document":index|null}]}]}"#,
            has_project,
            project_info,
            false
        ),
        spec!(
            "project.setColor",
            "Set Project Color",
            r#"{"level":"project|album|photo","id"?:id,"field":"input|working|output|intent|bpc","value":<space>|"auto"|intent|bool|null} (null = inherit) → {"field","color":{…},"documents":[{"document","photo","converted","needsRebuild"}],"needsRebuild":bool}; open photos follow: a new working space converts them, output/intent/BPC change the viewer and exports, a new input space needs a rebuild from the original"#,
            has_project,
            set_color,
            true
        ),
        spec!("album.new", "New Album", r#"{"name":string} → {"id","name"}"#, has_project, album_new, true),
        spec!("album.rename", "Rename Album", r#"{"id":id,"name":string} → {"id","name"}"#, has_project, album_rename, true),
        spec!("album.delete", "Delete Album", r#"{"id":id} → {"deleted":id,"photos":n} (files are not deleted)"#, has_project, album_delete, true),
        spec!(
            "album.import",
            "Import Photos",
            r#"{"album":id,"paths":[path…],"mode":"reference|copy"} → {"album","mode","imported":n,"skipped":n,"results":[{"path","status":"imported|skipped","id"?,"stored"?,"reason"?}]} (copy: into "<Project> Media/<Album>/", never overwriting)"#,
            has_project,
            album_import,
            true
        ),
        spec!(
            "album.export",
            "Export Album",
            r#"{"album":id,"folder":path,"format":"jpeg|png|tiff|webp","quality"?:0-12,"overwrite":bool=false} → {"album","folder","exported","skipped","failed","results":[{"photo","path","status":"exported|skipped|failed","reason"?,"warnings"?}]} (each photo flattened in its resolved Output space)"#,
            has_project,
            album_export,
            true
        ),
        spec!("photo.remove", "Remove Photo", r#"{"id":id} → {"removed":id} (files are not deleted)"#, has_project, photo_remove, true),
        spec!("photo.relink", "Relink Photo", r#"{"id":id,"path":path} → {"id","path"}"#, has_project, photo_relink, true),
        spec!(
            "photo.open",
            "Open Photo",
            r#"{"id":id} → {"document":index,"photo","fromSidecar","sidecar","original","pipeline":{…},"color":report,"warnings"} (or {"document","photo","alreadyOpen":true})"#,
            has_project,
            photo_open,
            true
        ),
        spec!(
            "photo.save",
            "Save Photo",
            r#"{} → {"path":sidecar,"photo","warnings"} (active document must be a project photo; writes <original>.pvision, the native bundle format)"#,
            has_project_photo,
            photo_save,
            true
        ),
        spec!(
            "photo.rebuild",
            "Rebuild from Original",
            r#"{"id":id} → {"document","photo","pipeline":{…},"warnings"} (an open photo: re-reads the original through its resolved pipeline, e.g. after an Input space change, and replaces the bottom layer's pixels, keeping the layers above; one undo step; refuses when the canvas size or mode changed)"#,
            has_project,
            photo_rebuild,
            true
        ),
        spec!(
            "photo.thumbnail",
            "Photo Thumbnail",
            r#"{"id":id,"maxSide":16-2048=256} → {"photo","path":png,"cached":bool} (an sRGB PNG in the project's .pvcache)"#,
            has_project,
            photo_thumbnail,
            false
        ),
    ]
}

#[cfg(test)]
#[path = "project_cmds_tests.rs"]
mod tests;
