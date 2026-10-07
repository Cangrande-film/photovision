//! Projects, albums and photos (PhotoVision's Library): `project.*`, `album.*`, `photo.*`.
//!
//! The data model is `photocraft_project` (pure data, JSON); this module adds the file system and
//! the documents. One project is open at a time ([`Session::project`]). A photo opens through its
//! resolved colour pipeline (photo over album over project settings); its edits are saved as a
//! sidecar next to the original (`IMG_0001.jpg.pcraft`, the original is never written), and an
//! open photo remembers which photo it is ([`crate::DocState::project_photo`]), so File › Save
//! writes the sidecar.
//!
//! File-system commands are unavailable on the web (their `enabled` predicates say so and the
//! file helpers return an error there).

use std::collections::HashSet;

use photocraft_cms::ColorSpace;
use photocraft_doc::Document;
use photocraft_project::{self as pj, Added, Level, Project};
use serde_json::{Value, json};

use crate::color_cmds::mode_space;
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
}

impl ProjectState {
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
            }
        }
    }
}

// ------------------------------------------------------------------ helpers

fn perr(e: pj::ProjectError) -> EngineError {
    EngineError::Other(e.to_string())
}

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn no_project() -> EngineError {
    EngineError::Other("no project open (project.new or project.open)".into())
}

fn state(s: &Session) -> Result<&ProjectState> {
    s.project.as_ref().ok_or_else(no_project)
}

fn state_mut(s: &mut Session) -> Result<&mut ProjectState> {
    s.project.as_mut().ok_or_else(no_project)
}

fn id_param(p: &Value, key: &str, cmd: &str) -> Result<u64> {
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

fn is_rgb(doc: &Document) -> bool {
    mode_space(doc.mode) == ColorSpace::Rgb
}

// ------------------------------------------------------------------ file system (native only)

#[cfg(not(target_arch = "wasm32"))]
mod fs {
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
}

#[cfg(target_arch = "wasm32")]
mod fs {
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

fn has_project(s: &Session) -> std::result::Result<(), String> {
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
    let st = ProjectState { project: Project::new(&name), path, dirty: false };
    write_project(&st)?;
    set_project(s, st);
    info_json(s)
}

fn project_open(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "project.open";
    let path = fs::absolute(str_param(p, "path", cmd)?);
    check_discard(s, p, cmd)?;
    let project = read_project(&path)?;
    set_project(s, ProjectState { project, path, dirty: false });
    info_json(s)
}

fn project_save(s: &mut Session, _: &Value) -> Result<Value> {
    let st = state_mut(s)?;
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
    Ok(json!({"id": id, "name": name.trim()}))
}

fn album_delete(s: &mut Session, p: &Value) -> Result<Value> {
    let id = id_param(p, "id", "album.delete")?;
    let st = state_mut(s)?;
    let a = st.project.delete_album(id).map_err(perr)?;
    st.dirty = true;
    let ids: Vec<u64> = a.photos.iter().map(|p| p.id).collect();
    clear_open(s, &ids);
    Ok(json!({"deleted": id, "photos": ids.len()}))
}

/// At most this many paths per `album.import` call.
const MAX_IMPORT: usize = 100_000;

fn importable(path: &str) -> std::result::Result<(), &'static str> {
    if pj::is_sidecar(path) {
        return Err("an edit sidecar, not a photo");
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

fn load_photo(st: &ProjectState, photo: u64) -> Result<Loaded> {
    let original = st.photo_file(photo)?;
    let sidecar = pj::sidecar_path(&original);
    let from_sidecar = fs::is_file(&sidecar);
    let (mut doc, warnings) = if from_sidecar {
        decode(&sidecar)?
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

fn photo_save(s: &mut Session, _: &Value) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let photo = d.project_photo.ok_or_else(|| EngineError::Other("the active document is not a project photo".into()))?;
    let sidecar = state(s)?.sidecar(photo)?;
    let warnings = file_cmds::save_doc(&d.doc, &sidecar, None)?;
    let st = s.active_mut().ok_or(EngineError::NoDocument)?;
    st.path = Some(sidecar.clone());
    st.saved_revision = st.revision;
    Ok(json!({"path": sidecar, "photo": photo, "warnings": warnings}))
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
    let (bytes, warnings) = file_cmds::encode_to(&l.doc, out, quality, target)?;
    file_cmds::write_file(out, &bytes)?;
    Ok(warnings)
}

/// Exports every photo of an album, flattened, in its resolved Output space. Runs synchronously
/// and leaves the open documents alone (each photo is decoded on its own).
fn album_export(s: &mut Session, p: &Value) -> Result<Value> {
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
    fs::create_dir_all(&folder)?;
    let mut taken: HashSet<String> = HashSet::new();
    let mut results = Vec::with_capacity(photos.len());
    let (mut exported, mut skipped, mut failed) = (0usize, 0usize, 0usize);
    for id in photos {
        let original = st.photo_file(id)?;
        let name = pj::unique_name(&format!("{}.{ext}", pj::file_stem(&original)), |n| taken.contains(&n.to_lowercase()));
        taken.insert(name.to_lowercase());
        let out = pj::join(&folder, &name);
        if fs::exists(&out) && !overwrite {
            skipped += 1;
            results.push(json!({"photo": id, "path": out, "status": "skipped", "reason": "file exists (pass \"overwrite\": true)"}));
            continue;
        }
        match export_one(st, id, &out, quality) {
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
    Ok(json!({"album": album, "folder": folder, "exported": exported, "skipped": skipped, "failed": failed, "results": results}))
}

// ------------------------------------------------------------------ photo.thumbnail

const RAW_EXTENSIONS: &[&str] = &["dng", "cr2", "cr3", "nef", "nrw", "arw", "pef", "orf", "raf", "rw2", "srw"];

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// A small document to thumbnail: the sidecar's stored thumbnail (else the sidecar), a raw's
/// embedded preview (else the developed raw), or the decoded file.
fn thumb_source(source: &str, sidecar: bool) -> Result<Document> {
    let bytes = file_cmds::read_file(source)?;
    let small = if sidecar {
        photocraft_format::read_thumbnail(&bytes).ok().flatten().map(|png| ("thumbnail.png", png))
    } else if file_cmds::extension(source).is_some_and(|x| RAW_EXTENSIONS.contains(&x.as_str())) {
        photocraft_raw::embedded_preview(&bytes).map(|pv| ("preview.jpg", pv.jpeg.to_vec()))
    } else {
        None
    };
    if let Some((name, b)) = small
        && let Ok(r) = photocraft_io::import(name, &b)
    {
        return Ok(r.document);
    }
    photocraft_io::import(pj::file_name(source), &bytes).map(|r| r.document).map_err(|e| EngineError::Other(format!("{source}: {e}")))
}

fn encode_png(img: &photocraft_raster::Rgba8Image) -> Result<Vec<u8>> {
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
    let image = Image::from_raw(img.width, img.height, ChannelLayout::Rgba, SampleType::U8, img.pixels.clone())
        .map_err(|e| EngineError::Other(format!("thumbnail: {e}")))?;
    photocraft_codecs::encode(&image, Format::Png, &EncodeOptions::default()).map_err(|e| EngineError::Other(format!("thumbnail: {e}")))
}

fn photo_thumbnail(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "photo.thumbnail";
    let id = id_param(p, "id", cmd)?;
    let max = match p.get("maxSide") {
        None | Some(Value::Null) => 256,
        Some(v) => v.as_f64().filter(|f| f.is_finite()).ok_or_else(|| bad(cmd, "`maxSide` must be a number"))?.clamp(16.0, 2048.0) as u32,
    };
    let st = state(s)?;
    let original = st.photo_file(id)?;
    let sidecar = pj::sidecar_path(&original);
    let from_sidecar = fs::is_file(&sidecar);
    let source = if from_sidecar { sidecar } else { original };
    let (mtime, size) = fs::stamp(&source).ok_or_else(|| EngineError::Other(format!("{source} is missing: relink the photo (photo.relink)")))?;
    let key = fnv1a(format!("{source}\n{mtime}\n{size}\n{max}").as_bytes());
    let dir = pj::thumb_cache_dir(&st.path);
    let out = pj::join(&dir, &format!("{key:016x}.png"));
    if fs::is_file(&out) {
        return Ok(json!({"photo": id, "path": out, "cached": true}));
    }
    let doc = thumb_source(&source, from_sidecar)?;
    let png = encode_png(&photocraft_compose::thumbnail(&doc, max))?;
    fs::create_dir_all(&dir)?;
    file_cmds::write_file(&out, &png)?;
    Ok(json!({"photo": id, "path": out, "cached": false}))
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
            r#"{} → {"path":sidecar,"photo","warnings"} (active document must be a project photo; writes <original>.pcraft)"#,
            has_project_photo,
            photo_save,
            true
        ),
        spec!(
            "photo.thumbnail",
            "Photo Thumbnail",
            r#"{"id":id,"maxSide":16-2048=256} → {"photo","path":png,"cached":bool}"#,
            has_project,
            photo_thumbnail,
            false
        ),
    ]
}

#[cfg(test)]
#[path = "project_cmds_tests.rs"]
mod tests;
