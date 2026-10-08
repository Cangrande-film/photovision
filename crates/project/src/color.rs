//! Colour pipeline settings as plain data: the Input → Photo (working) → Output spaces, intent
//! and black point compensation, per level ([`ColorOverride`]: every field optional) and
//! resolved ([`ColorPipeline`]). Spaces are [`SpaceId`]s: a built-in profile id
//! (`photocraft_cms::Builtin::id`) or `icc:<path>` for a custom profile file. The engine
//! (`photocraft_engine::color_pipeline`) re-exports these types and adds the colour maths
//! (profiles, conversion, the active pipeline per document).

use photocraft_cms::{Builtin, ColorSpace, Intent};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

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
