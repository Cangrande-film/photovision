//! `photocraft-project`: PhotoVision projects as pure data.
//!
//! A project (`MyShoot.pvproj`, pretty JSON, versioned) holds albums of photos. Each level
//! (project, album, photo) carries a [`ColorOverride`] whose unset fields inherit from the level
//! above; [`Project::resolve_color`] resolves a photo's Input → Photo → Output pipeline.
//!
//! * Photos are **referenced** in place (an absolute path) or **managed**: copied into the
//!   project's media folder ([`media_dir`]: `MyShoot Media/<Album>/`) and stored relative to the
//!   project file's folder with `/` separators.
//! * Edits are saved as a sidecar next to the original ([`sidecar_path`]: `IMG_0001.jpg.pvision`, the native `.pcraft` bundle format under PhotoVision's extension);
//!   originals are never modified.
//! * Thumbnails are cached in [`thumb_cache_dir`] (`MyShoot.pvcache/thumbs/`).
//!
//! No I/O happens here (the engine reads and writes files); everything builds for wasm.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod color;

use serde::{Deserialize, Serialize};

pub use color::{ColorOverride, ColorPipeline, InputSpace, SpaceId};

/// Current project file format version.
pub const FORMAT_VERSION: u32 = 1;
/// Project file extension (without the dot).
pub const EXTENSION: &str = "pvproj";
/// Sidecar extension appended to the original's file name.
pub const SIDECAR_EXTENSION: &str = "pvision";
/// The sidecar extension of early (pre-release) projects, still read: `IMG_0001.jpg.pcraft`.
pub const LEGACY_SIDECAR_EXTENSION: &str = "pcraft";
/// Largest project file [`Project::from_json`] accepts.
pub const MAX_JSON_BYTES: usize = 256 << 20;
/// Limits that keep a hostile file from exhausting memory or the UI.
pub const MAX_ALBUMS: usize = 100_000;
pub const MAX_PHOTOS: usize = 2_000_000;
pub const MAX_NAME_CHARS: usize = 255;
pub const MAX_PATH_CHARS: usize = 4096;

/// What went wrong with a project operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectError {
    NoAlbum(u64),
    NoPhoto(u64),
    /// Invalid input (names, paths, JSON) with an actionable message.
    Invalid(String),
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectError::NoAlbum(id) => write!(f, "no album with id {id}"),
            ProjectError::NoPhoto(id) => write!(f, "no photo with id {id}"),
            ProjectError::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ProjectError {}

pub type Result<T> = std::result::Result<T, ProjectError>;

fn invalid(msg: impl Into<String>) -> ProjectError {
    ProjectError::Invalid(msg.into())
}

/// A PhotoVision project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub color: ColorOverride,
    #[serde(default)]
    pub albums: Vec<Album>,
    /// Next id to hand out (albums and photos share one id space).
    #[serde(default)]
    pub next_id: u64,
}

/// An album: an ordered list of photos.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub color: ColorOverride,
    #[serde(default)]
    pub photos: Vec<Photo>,
}

/// A photo in an album.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Photo {
    pub id: u64,
    /// Managed: relative to the project file's folder, `/`-separated. Referenced: absolute.
    pub path: String,
    /// Copied into the project's media folder (vs referenced in place).
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub color: ColorOverride,
    /// When the photo was added (RFC 3339 UTC), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<String>,
}

/// Outcome of adding one path in [`Project::add_photos`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Added {
    /// A new photo with this id.
    New(u64),
    /// The album already has this path (the existing photo's id).
    Duplicate(u64),
}

/// The level a colour setting applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Project,
    Album(u64),
    Photo(u64),
}

impl Project {
    pub fn new(name: &str) -> Project {
        let name = name.trim();
        Project {
            version: FORMAT_VERSION,
            name: if name.is_empty() { "Untitled".into() } else { name.into() },
            color: ColorOverride::default(),
            albums: Vec::new(),
            next_id: 1,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id.saturating_add(1);
        id
    }

    pub fn album(&self, id: u64) -> Result<&Album> {
        self.albums.iter().find(|a| a.id == id).ok_or(ProjectError::NoAlbum(id))
    }

    pub fn album_mut(&mut self, id: u64) -> Result<&mut Album> {
        self.albums.iter_mut().find(|a| a.id == id).ok_or(ProjectError::NoAlbum(id))
    }

    /// A photo and the album it is in.
    pub fn find_photo(&self, id: u64) -> Result<(&Album, &Photo)> {
        self.albums.iter().find_map(|a| a.photos.iter().find(|p| p.id == id).map(|p| (a, p))).ok_or(ProjectError::NoPhoto(id))
    }

    pub fn photo_mut(&mut self, id: u64) -> Result<&mut Photo> {
        self.albums.iter_mut().find_map(|a| a.photos.iter_mut().find(|p| p.id == id)).ok_or(ProjectError::NoPhoto(id))
    }

    pub fn photo_count(&self) -> usize {
        self.albums.iter().map(|a| a.photos.len()).sum()
    }

    fn check_album_name(&self, name: &str, except: Option<u64>) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("album name is empty"));
        }
        if name.chars().count() > MAX_NAME_CHARS {
            return Err(invalid(format!("album name is longer than {MAX_NAME_CHARS} characters")));
        }
        if self.albums.iter().any(|a| Some(a.id) != except && a.name.to_lowercase() == name.to_lowercase()) {
            return Err(invalid(format!("an album named \"{name}\" already exists")));
        }
        Ok(name.to_string())
    }

    /// Adds an empty album; returns its id. Names are unique (case-insensitive).
    pub fn add_album(&mut self, name: &str) -> Result<u64> {
        let name = self.check_album_name(name, None)?;
        if self.albums.len() >= MAX_ALBUMS {
            return Err(invalid(format!("a project holds at most {MAX_ALBUMS} albums")));
        }
        let id = self.alloc_id();
        self.albums.push(Album { id, name, color: ColorOverride::default(), photos: Vec::new() });
        Ok(id)
    }

    pub fn rename_album(&mut self, id: u64, name: &str) -> Result<()> {
        let name = self.check_album_name(name, Some(id))?;
        self.album_mut(id)?.name = name;
        Ok(())
    }

    /// Removes an album from the project (its files are left alone).
    pub fn delete_album(&mut self, id: u64) -> Result<Album> {
        let i = self.albums.iter().position(|a| a.id == id).ok_or(ProjectError::NoAlbum(id))?;
        Ok(self.albums.remove(i))
    }

    /// Adds photos to an album. `paths` are stored as given (callers pass absolute paths for
    /// referenced photos and project-relative ones for managed copies, see
    /// [`relative_to_project`]); a path the album already has is reported as a duplicate.
    pub fn add_photos(&mut self, album: u64, paths: &[String], managed: bool, added: Option<&str>) -> Result<Vec<Added>> {
        self.album(album)?;
        let mut out = Vec::with_capacity(paths.len().min(4096));
        for path in paths {
            validate_photo_path(path, managed)?;
            let key = path_key(path);
            if let Some(p) = self.album(album)?.photos.iter().find(|p| path_key(&p.path) == key) {
                out.push(Added::Duplicate(p.id));
                continue;
            }
            if self.photo_count() >= MAX_PHOTOS {
                return Err(invalid(format!("a project holds at most {MAX_PHOTOS} photos")));
            }
            let id = self.alloc_id();
            let photo = Photo { id, path: path.clone(), managed, color: ColorOverride::default(), added: added.map(str::to_string) };
            self.album_mut(album)?.photos.push(photo);
            out.push(Added::New(id));
        }
        Ok(out)
    }

    /// Removes a photo from its album (the file and its sidecar are left alone).
    pub fn remove_photo(&mut self, id: u64) -> Result<Photo> {
        for a in &mut self.albums {
            if let Some(i) = a.photos.iter().position(|p| p.id == id) {
                return Ok(a.photos.remove(i));
            }
        }
        Err(ProjectError::NoPhoto(id))
    }

    /// Points a photo at a new file (a moved original). Stored as given, like [`Self::add_photos`].
    pub fn relink(&mut self, id: u64, path: &str, managed: bool) -> Result<()> {
        validate_photo_path(path, managed)?;
        let p = self.photo_mut(id)?;
        p.path = path.to_string();
        p.managed = managed;
        Ok(())
    }

    /// The colour settings of a level.
    pub fn color_of(&self, level: Level) -> Result<&ColorOverride> {
        match level {
            Level::Project => Ok(&self.color),
            Level::Album(id) => Ok(&self.album(id)?.color),
            Level::Photo(id) => Ok(&self.find_photo(id)?.1.color),
        }
    }

    pub fn color_mut(&mut self, level: Level) -> Result<&mut ColorOverride> {
        match level {
            Level::Project => Ok(&mut self.color),
            Level::Album(id) => Ok(&mut self.album_mut(id)?.color),
            Level::Photo(id) => Ok(&mut self.photo_mut(id)?.color),
        }
    }

    /// A level's effective pipeline: its own settings over those of the levels above, over `base`.
    pub fn resolve_level(&self, level: Level, base: &ColorPipeline) -> Result<ColorPipeline> {
        Ok(match level {
            Level::Project => color::resolve(&[&self.color], base),
            Level::Album(id) => color::resolve(&[&self.album(id)?.color, &self.color], base),
            Level::Photo(id) => {
                let (a, p) = self.find_photo(id)?;
                color::resolve(&[&p.color, &a.color, &self.color], base)
            }
        })
    }

    /// A photo's effective pipeline (photo over album over project over `base`).
    pub fn resolve_color(&self, photo: u64, base: &ColorPipeline) -> Result<ColorPipeline> {
        self.resolve_level(Level::Photo(photo), base)
    }

    /// Pretty JSON.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| invalid(format!("cannot serialize the project: {e}")))
    }

    /// Parses and validates a project file: version, unique ids, sizes, and managed paths that
    /// stay inside the project folder. `next_id` is repaired if it is behind the ids in use.
    pub fn from_json(text: &str) -> Result<Project> {
        if text.len() > MAX_JSON_BYTES {
            return Err(invalid(format!("project file is larger than {} MiB", MAX_JSON_BYTES >> 20)));
        }
        let mut p: Project = serde_json::from_str(text).map_err(|e| invalid(format!("not a PhotoVision project file: {e}")))?;
        p.validate()?;
        let max = p.albums.iter().flat_map(|a| std::iter::once(a.id).chain(a.photos.iter().map(|p| p.id))).max().unwrap_or(0);
        if p.next_id <= max {
            p.next_id = max.saturating_add(1);
        }
        Ok(p)
    }

    fn validate(&self) -> Result<()> {
        if self.version == 0 || self.version > FORMAT_VERSION {
            return Err(invalid(format!(
                "project format version {} is not supported (this build reads up to {FORMAT_VERSION}); update PhotoVision",
                self.version
            )));
        }
        if self.name.chars().count() > MAX_NAME_CHARS {
            return Err(invalid("project name is too long"));
        }
        if self.albums.len() > MAX_ALBUMS || self.photo_count() > MAX_PHOTOS {
            return Err(invalid("project has too many albums or photos"));
        }
        let mut ids = std::collections::HashSet::new();
        for a in &self.albums {
            if a.id == 0 || !ids.insert(a.id) {
                return Err(invalid(format!("duplicate or zero id {}", a.id)));
            }
            if a.name.trim().is_empty() || a.name.chars().count() > MAX_NAME_CHARS {
                return Err(invalid(format!("album {} has an empty or too long name", a.id)));
            }
            for ph in &a.photos {
                if ph.id == 0 || !ids.insert(ph.id) {
                    return Err(invalid(format!("duplicate or zero id {}", ph.id)));
                }
                validate_photo_path(&ph.path, ph.managed)?;
            }
        }
        Ok(())
    }
}

fn path_key(p: &str) -> String {
    p.replace('\\', "/")
}

fn is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with('\\') || (b.len() >= 2 && b.first().is_some_and(u8::is_ascii_alphabetic) && b.get(1) == Some(&b':'))
}

fn validate_photo_path(path: &str, managed: bool) -> Result<()> {
    if path.trim().is_empty() {
        return Err(invalid("photo path is empty"));
    }
    if path.chars().count() > MAX_PATH_CHARS || path.contains('\0') {
        return Err(invalid("photo path is too long or contains NUL"));
    }
    if managed && (is_absolute(path) || path.split(['/', '\\']).any(|c| c == "..")) {
        return Err(invalid(format!("managed photo path `{path}` must be relative to the project folder and stay inside it")));
    }
    Ok(())
}

// ------------------------------------------------------------------ paths

/// The folder part of `path` (empty for a bare file name).
pub fn dir_of(path: &str) -> &str {
    match path.rfind(['/', '\\']) {
        Some(0) => &path[..1],
        Some(i) => &path[..i],
        None => "",
    }
}

/// The file name part of `path`.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// File name without its last extension (`MyShoot.pvproj` → `MyShoot`).
pub fn file_stem(path: &str) -> &str {
    let n = file_name(path);
    match n.rfind('.') {
        Some(i) if i > 0 => &n[..i],
        _ => n,
    }
}

/// `dir` + `name` with one separator: a backslash when `dir` uses only backslashes (Windows),
/// else `/` (or none when `dir` already ends with one).
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else if dir.ends_with('/') || dir.ends_with('\\') {
        format!("{dir}{name}")
    } else if dir.contains('\\') && !dir.contains('/') {
        format!("{dir}\\{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The edit sidecar of an original: `IMG_0001.jpg` → `IMG_0001.jpg.pvision`.
pub fn sidecar_path(original: &str) -> String {
    format!("{original}.{SIDECAR_EXTENSION}")
}

/// The legacy sidecar of an original (`IMG_0001.jpg.pcraft`), read when there is no `.pvision`.
pub fn legacy_sidecar_path(original: &str) -> String {
    format!("{original}.{LEGACY_SIDECAR_EXTENSION}")
}

/// Is `path` an edit sidecar (`<name>.<ext>.pvision`, or a legacy `<name>.<ext>.pcraft`)?
pub fn is_sidecar(path: &str) -> bool {
    let n = file_name(path).to_ascii_lowercase();
    [SIDECAR_EXTENSION, LEGACY_SIDECAR_EXTENSION]
        .iter()
        .any(|ext| n.strip_suffix(ext).and_then(|b| b.strip_suffix('.')).is_some_and(|base| base.rfind('.').is_some_and(|i| i > 0 && i + 1 < base.len())))
}

/// A file-name-safe version of `name` (also avoids Windows device names).
pub fn sanitize(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.' | '(' | ')') { c } else { '_' }).collect();
    let s = s.trim().trim_matches('.').trim().to_string();
    let s: String = s.chars().take(MAX_NAME_CHARS).collect();
    if s.is_empty() {
        return "Album".into();
    }
    let upper = s.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    let reserved = matches!(base, "CON" | "PRN" | "AUX" | "NUL")
        || ((base.starts_with("COM") || base.starts_with("LPT")) && base.len() == 4 && base.as_bytes().get(3).is_some_and(u8::is_ascii_digit));
    if reserved { format!("_{s}") } else { s }
}

/// Folder of the project's own files (`<dir>/MyShoot Media`).
pub fn media_root(project_path: &str) -> String {
    join(dir_of(project_path), &format!("{} Media", sanitize(file_stem(project_path))))
}

/// Where copies imported into an album go: `<project dir>/<ProjectStem> Media/<sanitized album>`.
pub fn media_dir(project_path: &str, album_name: &str) -> String {
    join(&media_root(project_path), &sanitize(album_name))
}

/// Thumbnail cache: `<project dir>/<ProjectStem>.pvcache/thumbs`.
pub fn thumb_cache_dir(project_path: &str) -> String {
    join(&join(dir_of(project_path), &format!("{}.pvcache", sanitize(file_stem(project_path)))), "thumbs")
}

/// `file` relative to the project file's folder with `/` separators, if it is inside it.
pub fn relative_to_project(project_path: &str, file: &str) -> Option<String> {
    let dir = path_key(dir_of(project_path));
    let file = path_key(file);
    let dir = dir.trim_end_matches('/');
    let rest = if dir.is_empty() { Some(file.as_str()) } else { file.strip_prefix(dir)?.strip_prefix('/') }?;
    if rest.is_empty() || is_absolute(rest) || rest.split('/').any(|c| c == "..") { None } else { Some(rest.to_string()) }
}

/// The file a photo refers to: managed paths are resolved against the project file's folder.
pub fn photo_file(project_path: &str, photo: &Photo) -> String {
    if photo.managed {
        let dir = dir_of(project_path);
        let sep = if dir.contains('\\') && !dir.contains('/') { "\\" } else { "/" };
        join(dir, &photo.path.replace('/', sep))
    } else {
        photo.path.clone()
    }
}

/// `name` made unique against `taken` by adding " (2)", " (3)", … before the extension.
pub fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 2..10_000u32 {
        let candidate = format!("{stem} ({n}){ext}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{stem} ({}){ext}", u64::MAX)
}

/// Unix seconds as an RFC 3339 UTC timestamp (`2026-10-07T12:34:56Z`).
pub fn rfc3339_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil from days (Howard Hinnant's algorithm), on i64 to keep it simple.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests;
