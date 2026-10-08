//! The Input → Photo (working) → Output colour pipeline, Resolve-style.
//!
//! * **Input** ([`InputSpace`]): how the file's pixels are interpreted. `auto` = the embedded ICC
//!   profile, else sRGB; or a fixed space that is *assigned* (overrides the embedded profile).
//! * **Working** (the "Photo" space): the document is converted into it when opened, so its
//!   `icc_profile` is the working profile and compositing, adjustments and saves work unchanged.
//!   Linear working spaces promote the document to 32-bit float ([`working_depth`]), which is what
//!   `photocraft_compose::adjust::Transfer::for_document` expects (integer depths gamma-encoded,
//!   float linear); choosing a linear working space *is* "linear blending".
//! * **Output**: what flat exports are converted to and tagged with
//!   (`photocraft_io::ExportOptions::target`), and what the canvas soft-proofs through
//!   (working → output → monitor). Layered saves (PSD/PSB, `.pcraft`) stay in the working space.
//!
//! Settings come in levels (project, album, photo). Each level is a [`ColorOverride`] whose
//! fields are all optional; [`resolve`] picks, per field, the most specific level that sets it,
//! falling back to a base [`ColorPipeline`] (the default reproduces today's behaviour: embedded
//! profile or sRGB, sRGB working and output, relative colorimetric with BPC).
//!
//! Spaces are [`SpaceId`]s: a built-in profile id (`photocraft_cms::Builtin::id`) or
//! `icc:<path>` for a custom profile file (native only). All pipeline spaces are RGB.
//!
//! The active document's pipeline lives in `ColorState` (per document, like the proof setup) so
//! the display path, which only sees the `Document`, can read it; it is view/session state and is
//! not saved in `.pcraft` files. Commands: `color.pipeline` (get) and `color.setPipeline` (set);
//! `file.openAs` takes `colorPipeline` to open through a pipeline.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use photocraft_cms::{Builtin, ColorSpace, Curve, Profile};
use photocraft_color::SampleType;
use photocraft_doc::Document;
use serde_json::{Value, json};

pub use photocraft_io::ExportTarget;

use crate::color_cmds::{convert_document, document_profile, mode_space};
use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

// ------------------------------------------------------------------ data types

// The pure-data types live in `photocraft-project` (projects store them per level); re-exported
// here so the engine API is unchanged.
pub use photocraft_project::color::{ColorOverride, ColorPipeline, ICC_PREFIX, InputSpace, SpaceId, input_spaces, output_spaces, resolve, working_spaces};

/// Does `space` store linear light? Custom profiles count when they are RGB matrix/TRC profiles
/// with identity curves (unreadable ones are not linear).
pub fn is_linear(space: &SpaceId) -> bool {
    if let Some(b) = space.as_builtin() {
        return b.is_linear();
    }
    profile(space).is_ok_and(|p| p.is_matrix_shaper() && p.trc.as_ref().is_some_and(|t| t.iter().all(Curve::is_identity)))
}

/// Bit depth of a document in `working`: linear spaces need 32-bit float, gamma-encoded spaces
/// keep the current depth.
pub fn working_depth(current: SampleType, working: &SpaceId) -> SampleType {
    if is_linear(working) { SampleType::F32 } else { current }
}

/// The profile of a space (built-ins are cached; `icc:` files are read each call, native only).
/// Fails with an actionable message when the file can't be read or isn't an RGB profile.
pub fn profile(space: &SpaceId) -> Result<Arc<Profile>> {
    if let Some(b) = space.as_builtin() {
        static CACHE: OnceLock<Mutex<HashMap<Builtin, Arc<Profile>>>> = OnceLock::new();
        let mut c = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        return Ok(c.entry(b).or_insert_with(|| Arc::new(b.profile().clone())).clone());
    }
    let path = space.icc_path().ok_or_else(|| EngineError::Other(format!("unknown colour space `{space}`")))?;
    let p = read_icc(path)?;
    if p.color_space != ColorSpace::Rgb {
        return Err(EngineError::Other(format!("`{path}` is a {:?} profile; pipeline spaces must be RGB", p.color_space)));
    }
    Ok(Arc::new(p))
}

#[cfg(not(target_arch = "wasm32"))]
fn read_icc(path: &str) -> Result<Profile> {
    let bytes = std::fs::read(path).map_err(|e| EngineError::Other(format!("cannot read colour profile `{path}`: {e}; pick a built-in space or fix the path")))?;
    Profile::parse(&bytes).map_err(|e| EngineError::Other(format!("`{path}` is not a usable ICC profile ({e}); pick a built-in space")))
}

#[cfg(target_arch = "wasm32")]
fn read_icc(path: &str) -> Result<Profile> {
    Err(EngineError::Other(format!("custom profile files (`{path}`) are not available in the browser; pick a built-in space")))
}

// ------------------------------------------------------------------ session state

/// A document's active pipeline with its working and output profiles resolved.
#[derive(Clone, Debug)]
pub struct ActivePipeline {
    pub pipeline: ColorPipeline,
    pub working: Arc<Profile>,
    pub output: Arc<Profile>,
}

impl ActivePipeline {
    pub fn new(pipeline: ColorPipeline) -> Result<ActivePipeline> {
        let working = profile(&pipeline.working)?;
        let output = profile(&pipeline.output)?;
        Ok(ActivePipeline { pipeline, working, output })
    }

    /// The flat-export target (`photocraft_io::ExportOptions::target`).
    pub fn export_target(&self) -> photocraft_io::ExportTarget {
        photocraft_io::ExportTarget { profile: self.output.clone(), intent: self.pipeline.intent, bpc: self.pipeline.bpc }
    }
}

/// Interprets `doc`'s pixels as `input` and converts them into `working` (promoting to 32-bit
/// float first when `working` is linear, so no precision is lost). RGB documents only. Returns
/// whether pixels were converted (false when the source already has the working colours; the
/// document is then just tagged with the working profile).
pub fn apply_to_document(doc: &mut Document, input: Option<&Profile>, ap: &ActivePipeline) -> Result<bool> {
    if mode_space(doc.mode) != ColorSpace::Rgb {
        return Err(EngineError::Other(format!("colour pipelines apply to RGB documents; this one is {:?}", doc.mode)));
    }
    if let Some(p) = input {
        doc.icc_profile = Some(p.to_bytes());
    }
    let depth = working_depth(doc.depth, &ap.pipeline.working);
    crate::image_cmds::set_depth(doc, depth);
    let src = document_profile(doc);
    if src.same_colors(&ap.working) {
        doc.icc_profile = Some(ap.working.to_bytes());
        return Ok(false);
    }
    convert_document(doc, &ap.working, ap.pipeline.intent, ap.pipeline.bpc)?;
    Ok(true)
}

pub(crate) fn input_profile(input: &InputSpace) -> Result<Option<Arc<Profile>>> {
    match input {
        InputSpace::Auto => Ok(None),
        InputSpace::Space(s) => profile(s).map(Some),
    }
}

pub(crate) fn report(ap: &ActivePipeline, doc: &Document, converted: bool) -> Value {
    json!({
        "action": "pipeline",
        "pipeline": ap.pipeline,
        "working": ap.working.description,
        "output": ap.output.description,
        "depth": format!("{:?}", doc.depth),
        "converted": converted,
    })
}

impl Session {
    /// Open a decoded file through a colour pipeline: interpret it in the input space, convert it
    /// to the working space (32-bit float for linear ones) and remember the pipeline for the
    /// viewer and exports. Non-RGB documents (gray, CMYK, Lab, …) are opened with the Color
    /// Settings policy instead (the pipeline spaces are RGB, and converting them would silently
    /// change the document's mode); the report then says `"pipeline": "skipped"`.
    pub fn open_document_with_pipeline(&mut self, mut doc: Document, path: Option<String>, pipeline: &ColorPipeline) -> Result<(usize, Value)> {
        if mode_space(doc.mode) != ColorSpace::Rgb {
            let (i, mut r) = self.open_document(doc, path);
            r["pipeline"] = json!("skipped");
            r["reason"] = json!("colour pipelines apply to RGB documents");
            return Ok((i, r));
        }
        let ap = ActivePipeline::new(pipeline.clone())?;
        let input = input_profile(&pipeline.input)?;
        let converted = apply_to_document(&mut doc, input.as_deref(), &ap)?;
        let r = report(&ap, &doc, converted);
        let id = doc.id;
        let i = self.add_document(doc, path);
        // add_document may have re-issued the id.
        let id = self.docs.get(i).map_or(id, |d| d.doc.id);
        self.color.set_pipeline(id, Some(ap));
        Ok((i, r))
    }

    /// The active pipeline of document `index`.
    pub fn doc_pipeline(&self, index: usize) -> Option<&ActivePipeline> {
        let id = self.documents().get(index)?.doc.id;
        self.color.pipeline(id)
    }

    /// Flat-export target of document `index` (its pipeline's output space), for
    /// `photocraft_io::ExportOptions::target`. `None` without a pipeline.
    pub fn export_target(&self, index: usize) -> Option<photocraft_io::ExportTarget> {
        self.doc_pipeline(index).map(ActivePipeline::export_target)
    }
}

// ------------------------------------------------------------------ commands

pub(crate) fn spaces_json() -> Value {
    let list = |v: Vec<(&str, &str)>| v.into_iter().map(|(id, label)| json!({"id": id, "label": label})).collect::<Vec<_>>();
    json!({"input": list(input_spaces()), "working": list(working_spaces()), "output": list(output_spaces())})
}

fn pipeline_json(s: &Session) -> Value {
    let active = s.active_index().and_then(|i| s.doc_pipeline(i));
    json!({
        "pipeline": active.map(|a| &a.pipeline),
        "working": active.map(|a| a.working.description.clone()),
        "output": active.map(|a| a.output.description.clone()),
        "spaces": spaces_json(),
    })
}

/// `color.pipeline`: the active document's pipeline (or null) and the spaces to choose from.
fn get_pipeline(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(pipeline_json(s))
}

/// `color.setPipeline`: change the active document's pipeline. Unset fields keep their value.
/// Without a pipeline yet, the whole pipeline is applied (interpret as input, convert to working)
/// as one undoable step. With one: a new working space re-converts the document (undoable); a
/// new output, intent or BPC only changes the viewer and exports; a new input is refused (the
/// original pixels are gone). `clear: true` drops the pipeline (the pixels stay as they are).
fn set_pipeline(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "color.setPipeline";
    let bad = |msg: String| EngineError::BadParams { cmd: cmd.into(), msg };
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let id = st.doc.id;
    match p.get("clear") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            s.color.set_pipeline(id, None);
            return Ok(pipeline_json(s));
        }
        Some(other) => return Err(bad(format!("`clear` must be a bool, got {other}"))),
    }
    let o = ColorOverride::from_params(p).map_err(bad)?;
    let current = s.color.pipeline(id).cloned();
    let base = current.as_ref().map(|a| a.pipeline.clone()).unwrap_or_default();
    let next = resolve(&[&o], &base);
    let ap = ActivePipeline::new(next.clone())?;
    match current {
        None => {
            let input = input_profile(&next.input)?;
            s.edit("Color Pipeline", |doc, _| apply_to_document(doc, input.as_deref(), &ap).map(|_| ()))?;
        }
        Some(cur) => {
            if next.input != cur.pipeline.input {
                return Err(EngineError::Other("input space can only be changed before editing; reopen the original".into()));
            }
            if next.working != cur.pipeline.working {
                s.edit("Convert to Photo Space", |doc, _| apply_to_document(doc, None, &ap).map(|_| ()))?;
            }
        }
    }
    s.color.set_pipeline(id, Some(ap));
    Ok(pipeline_json(s))
}

fn has_doc(s: &Session) -> std::result::Result<(), String> {
    match s.active() {
        None => Err("no document open".into()),
        Some(d) if mode_space(d.doc.mode) != ColorSpace::Rgb => Err("colour pipelines apply to RGB documents".into()),
        Some(_) => Ok(()),
    }
}

/// Colour pipeline command specs (no menu yet: the Library / Project Settings UI drives them).
pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "color.pipeline",
            label: "Color Pipeline",
            menu: &[],
            shortcut: None,
            params: "{} → {\"pipeline\":{\"input\":\"auto|<space>\",\"working\":\"<space>\",\"output\":\"<space>\",\"intent\":\"relative\",\"bpc\":true}|null,\"working\":desc,\"output\":desc,\"spaces\":{\"input\":[{id,label}],\"working\":[…],\"output\":[…]}}",
            enabled: |_| Ok(()),
            run: get_pipeline,
            journal: false,
        },
        CommandSpec {
            id: "color.setPipeline",
            label: "Set Color Pipeline",
            menu: &[],
            shortcut: None,
            params: r##"{"input":"auto|<space>"?,"working":"<space>"?,"output":"<space>"?,"intent":"perceptual|relative|saturation|absolute"?,"bpc":bool?,"clear":bool=false} (<space> = a built-in id such as srgb, rec709-bt1886, display-p3, rec2020, acescg, linear-srgb, or icc:<path>; unset fields keep their value; the input can only be set before the first pipeline is applied)"##,
            enabled: has_doc,
            run: set_pipeline,
            journal: true,
        },
    ]
}

#[cfg(test)]
#[path = "color_pipeline_tests.rs"]
mod tests;
