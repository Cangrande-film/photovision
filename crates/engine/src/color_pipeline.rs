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

use photocraft_cms::{Builtin, ColorSpace, Curve, Intent, Profile};
use photocraft_color::SampleType;
use photocraft_doc::Document;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

pub use photocraft_io::ExportTarget;

use crate::color_cmds::{convert_document, document_profile, mode_space};
use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

// ------------------------------------------------------------------ space ids

/// A colour space: a built-in profile id or `icc:<path>` (a custom profile file). Serialized as
/// that string. Built-in ids are canonical (aliases such as `ACEScg` are normalized).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SpaceId(String);

/// Prefix of custom profile files in a [`SpaceId`].
pub const ICC_PREFIX: &str = "icc:";

impl SpaceId {
    /// Parses a built-in id/alias or `icc:<path>`.
    pub fn parse(s: &str) -> std::result::Result<SpaceId, String> {
        if let Some(path) = s.strip_prefix(ICC_PREFIX) {
            if path.trim().is_empty() {
                return Err("`icc:` needs a profile path (icc:/path/to/profile.icc)".into());
            }
            return Ok(SpaceId(s.to_string()));
        }
        match Builtin::from_id(s) {
            Some(b) if b.profile().color_space == ColorSpace::Rgb => Ok(SpaceId(b.id().to_string())),
            Some(b) => Err(format!("`{}` is not an RGB space", b.id())),
            None => Err(format!("unknown colour space `{s}` (a built-in id such as {}, or icc:<path>)", ids_hint())),
        }
    }

    pub fn builtin(b: Builtin) -> SpaceId {
        SpaceId(b.id().to_string())
    }

    pub fn srgb() -> SpaceId {
        SpaceId::builtin(Builtin::Srgb)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_builtin(&self) -> Option<Builtin> {
        if self.icc_path().is_some() { None } else { Builtin::from_id(&self.0) }
    }

    /// The profile file of an `icc:` space.
    pub fn icc_path(&self) -> Option<&str> {
        self.0.strip_prefix(ICC_PREFIX)
    }

    /// Human-readable name (the built-in label, or the file name of a custom profile).
    pub fn label(&self) -> String {
        match (self.as_builtin(), self.icc_path()) {
            (Some(b), _) => b.label().to_string(),
            (None, Some(p)) => p.rsplit(['/', '\\']).next().unwrap_or(p).to_string(),
            _ => self.0.clone(),
        }
    }
}

impl Default for SpaceId {
    fn default() -> Self {
        SpaceId::srgb()
    }
}

impl std::fmt::Display for SpaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SpaceId {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SpaceId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        SpaceId::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn ids_hint() -> String {
    working_spaces().iter().map(|(id, _)| *id).collect::<Vec<_>>().join(", ")
}

/// How opened pixels are interpreted. Serialized as `"auto"` or a [`SpaceId`] string.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum InputSpace {
    /// The embedded ICC profile, else sRGB.
    #[default]
    Auto,
    /// Assign this space (ignore the embedded profile).
    Space(SpaceId),
}

impl InputSpace {
    pub fn parse(s: &str) -> std::result::Result<InputSpace, String> {
        if s.eq_ignore_ascii_case("auto") { Ok(InputSpace::Auto) } else { SpaceId::parse(s).map(InputSpace::Space) }
    }
    pub fn as_str(&self) -> &str {
        match self {
            InputSpace::Auto => "auto",
            InputSpace::Space(s) => s.as_str(),
        }
    }
}

impl Serialize for InputSpace {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for InputSpace {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        InputSpace::parse(&s).map_err(serde::de::Error::custom)
    }
}

mod intent_str {
    use super::*;
    pub fn serialize<S: Serializer>(i: &Intent, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(i.id())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Intent, D::Error> {
        let s = String::deserialize(d)?;
        Intent::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown intent `{s}` (perceptual|relative|saturation|absolute)")))
    }
}

mod opt_intent_str {
    use super::*;
    pub fn serialize<S: Serializer>(i: &Option<Intent>, s: S) -> std::result::Result<S::Ok, S::Error> {
        match i {
            Some(i) => s.serialize_some(i.id()),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Intent>, D::Error> {
        intent_str::deserialize(d).map(Some)
    }
}

// ------------------------------------------------------------------ pipeline

/// A resolved Input → Working → Output pipeline. JSON:
/// `{"input":"auto","working":"srgb","output":"srgb","intent":"relative","bpc":true}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ColorPipeline {
    pub input: InputSpace,
    pub working: SpaceId,
    pub output: SpaceId,
    /// Rendering intent of the input → working and working → output conversions.
    #[serde(with = "intent_str")]
    pub intent: Intent,
    /// Black point compensation.
    pub bpc: bool,
}

impl Default for ColorPipeline {
    fn default() -> Self {
        ColorPipeline { input: InputSpace::Auto, working: SpaceId::srgb(), output: SpaceId::srgb(), intent: Intent::RelativeColorimetric, bpc: true }
    }
}

/// One level's settings (project, album or photo): every field optional (`None` = inherit).
/// Serialized camelCase with unset fields left out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ColorOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputSpace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working: Option<SpaceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<SpaceId>,
    #[serde(skip_serializing_if = "Option::is_none", with = "opt_intent_str")]
    pub intent: Option<Intent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bpc: Option<bool>,
}

impl ColorOverride {
    pub fn is_empty(&self) -> bool {
        *self == ColorOverride::default()
    }

    /// Reads the pipeline keys (`input`, `working`, `output`, `intent`, `bpc`) of a command's
    /// params, ignoring other keys; `null` means "not set". Wrong types and unknown spaces fail.
    pub fn from_params(p: &Value) -> std::result::Result<ColorOverride, String> {
        let Some(obj) = p.as_object() else { return Err("expected an object of pipeline fields".into()) };
        let mut o = ColorOverride::default();
        let text = |k: &str| -> std::result::Result<Option<&str>, String> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.as_str())),
                Some(other) => Err(format!("`{k}` must be a string, got {other}")),
            }
        };
        if let Some(s) = text("input")? {
            o.input = Some(InputSpace::parse(s)?);
        }
        if let Some(s) = text("working")? {
            o.working = Some(SpaceId::parse(s)?);
        }
        if let Some(s) = text("output")? {
            o.output = Some(SpaceId::parse(s)?);
        }
        if let Some(s) = text("intent")? {
            o.intent = Some(Intent::parse(s).ok_or_else(|| format!("unknown intent `{s}` (perceptual|relative|saturation|absolute)"))?);
        }
        match obj.get("bpc") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(b)) => o.bpc = Some(*b),
            Some(other) => return Err(format!("`bpc` must be a bool, got {other}")),
        }
        Ok(o)
    }
}

/// Resolves a pipeline from override levels, most specific first (`[photo, album, project]`):
/// each field comes from the first level that sets it, else from `base`.
pub fn resolve(levels: &[&ColorOverride], base: &ColorPipeline) -> ColorPipeline {
    fn pick<T: Clone>(levels: &[&ColorOverride], get: impl Fn(&ColorOverride) -> Option<&T>, base: &T) -> T {
        levels.iter().find_map(|l| get(l)).unwrap_or(base).clone()
    }
    ColorPipeline {
        input: pick(levels, |l| l.input.as_ref(), &base.input),
        working: pick(levels, |l| l.working.as_ref(), &base.working),
        output: pick(levels, |l| l.output.as_ref(), &base.output),
        intent: pick(levels, |l| l.intent.as_ref(), &base.intent),
        bpc: pick(levels, |l| l.bpc.as_ref(), &base.bpc),
    }
}

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

const DISPLAY_SPACES: [Builtin; 10] = [
    Builtin::Srgb,
    Builtin::Rec709Bt1886,
    Builtin::Rec709Oetf,
    Builtin::DisplayP3,
    Builtin::P3D65,
    Builtin::DciP3,
    Builtin::Rec2020,
    Builtin::Rec2020G24,
    Builtin::AdobeRgbCompat,
    Builtin::ProPhotoCompat,
];

const LINEAR_SPACES: [Builtin; 4] = [Builtin::LinearSrgb, Builtin::LinearRec2020, Builtin::LinearP3D65, Builtin::AcesCg];

fn listed(spaces: &[Builtin]) -> Vec<(&'static str, &'static str)> {
    spaces.iter().map(|b| (b.id(), b.label())).collect()
}

/// Input spaces offered in pickers: `(id, English label)` (plus `auto`, which UIs add).
pub fn input_spaces() -> Vec<(&'static str, &'static str)> {
    listed(&DISPLAY_SPACES)
}

/// Working ("Photo") spaces: the display spaces and the linear ones.
pub fn working_spaces() -> Vec<(&'static str, &'static str)> {
    let mut v = listed(&DISPLAY_SPACES);
    v.extend(listed(&LINEAR_SPACES));
    v
}

/// Output spaces: the display spaces.
pub fn output_spaces() -> Vec<(&'static str, &'static str)> {
    listed(&DISPLAY_SPACES)
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

fn input_profile(input: &InputSpace) -> Result<Option<Arc<Profile>>> {
    match input {
        InputSpace::Auto => Ok(None),
        InputSpace::Space(s) => profile(s).map(Some),
    }
}

fn report(ap: &ActivePipeline, doc: &Document, converted: bool) -> Value {
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

fn spaces_json() -> Value {
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
