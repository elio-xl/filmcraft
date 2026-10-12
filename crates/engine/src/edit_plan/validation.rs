//! Strict, bounded plan parsing and pure project editing. No host services or Session execution.
use crate::media_analysis::source::{check_cancelled, failure};
use crate::{EditorState, EngineError, Result};
use filmcraft_edit::{self as edit, Edge, EditCtx, TrimMode};
use filmcraft_project::{ClipId, ItemId, ItemKind, Project, Sequence, TrackId};
use filmcraft_time::Tick;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const ALLOWED: &[&str] = &["timeline.trim", "timeline.move", "edit.clear", "captions.setText", "graphics.set"];
pub fn allowed_command(id: &str) -> bool {
    ALLOWED.contains(&id)
}
pub(super) struct SourceVersion {
    pub item: ItemId,
    pub version: String,
}
pub(super) struct Plan {
    pub apply: bool,
    #[cfg(test)]
    pub panic_after_step: Option<usize>,
    pub request_id: String,
    pub expected_revision: u64,
    pub sources: Vec<SourceVersion>,
    pub steps: Vec<Step>,
    pub canonical: Value,
}
pub(super) struct Step {
    pub id: String,
    pub reason: String,
    op: Operation,
}
enum Operation {
    Trim { clip: ClipId, edge: Edge, mode: TrimMode, delta: Tick },
    Move { moves: Vec<(ClipId, TrackId, Tick)>, linked: Option<bool>, insert: bool, copy: bool },
    Clear { clips: Vec<ClipId> },
    Caption { caption: ClipId, text: Option<String>, speaker: Option<Option<String>> },
    Graphic { clip: ClipId, layer: Value, time: Tick, props: Map<String, Value> },
}
fn invalid(message: impl AsRef<str>) -> EngineError {
    failure("PLAN_INVALID", message, false)
}
fn object<'a>(v: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>> {
    let f = v.as_object().ok_or_else(|| invalid("expected an object"))?;
    if f.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid("unknown field"));
    }
    Ok(f)
}
fn field<'a>(f: &'a Map<String, Value>, k: &str) -> Result<&'a Value> {
    f.get(k).ok_or_else(|| invalid(format!("missing explicit {k}")))
}
fn id(v: &Value) -> Result<u64> {
    v.as_u64().filter(|n| *n > 0).ok_or_else(|| invalid("target IDs must be positive integers"))
}
fn string(v: &Value, max: usize, empty: bool) -> Result<String> {
    let t = v
        .as_str()
        .filter(|s| (empty || !s.trim().is_empty()) && s.len() <= max * 4 && s.chars().count() <= max)
        .ok_or_else(|| invalid("missing, invalid or oversized string"))?;
    Ok(t.into())
}
fn boolean(f: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    f.get(key).map(|v| v.as_bool().ok_or_else(|| invalid(format!("{key} must be boolean")))).transpose().map(|v| v.unwrap_or(default))
}
fn unsigned(v: &Value) -> Result<u64> {
    let t = v
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 20 && s.bytes().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| invalid("expected unsigned decimal string"))?;
    t.parse().map_err(|_| invalid("integer exceeds u64"))
}
fn tick(v: &Value, signed: bool) -> Result<Tick> {
    let t = v.as_str().filter(|s| !s.is_empty() && s.len() <= 20).ok_or_else(|| invalid("time must be a decimal Tick string"))?;
    let digits = if signed { t.strip_prefix('-').unwrap_or(t) } else { t };
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(invalid("time must be a decimal Tick string"));
    }
    let n = t.parse::<i64>().map_err(|_| invalid("time overflows ticks"))?;
    if !(Tick::MIN.0..=Tick::MAX.0).contains(&n) || (!signed && n < 0) {
        return Err(invalid("time outside supported bounds"));
    }
    Ok(Tick(n))
}
fn budget(v: &Value) -> Result<()> {
    let mut todo = vec![(v, 0usize)];
    let mut nodes = 0usize;
    while let Some((v, depth)) = todo.pop() {
        nodes += 1;
        if nodes > 100000 || depth > 32 {
            return Err(invalid("plan is too large or deeply nested"));
        }
        match v {
            Value::Array(a) => {
                if a.len() > 10000 {
                    return Err(invalid("oversized array"));
                }
                todo.extend(a.iter().map(|v| (v, depth + 1)));
            }
            Value::Object(m) => {
                if m.len() > 10000 || m.keys().any(|k| k.len() > 65536) {
                    return Err(invalid("oversized object"));
                }
                todo.extend(m.values().map(|v| (v, depth + 1)));
            }
            Value::String(s) if s.len() > 1024 * 1024 => return Err(invalid("oversized string")),
            _ => {}
        }
    }
    if crate::media_analysis::cache::serialized_size(v)? > 1024 * 1024 {
        return Err(invalid("plan exceeds 1 MiB"));
    }
    Ok(())
}
pub(super) fn parse(p: &Value) -> Result<Plan> {
    budget(p)?;
    let f = object(p, &["request_id", "expected_revision", "source_versions", "steps"])?;
    let request_id = string(field(f, "request_id")?, 128, false)?;
    let expected_revision = unsigned(field(f, "expected_revision")?)?;
    let raw = field(f, "steps")?.as_array().filter(|a| !a.is_empty() && a.len() <= 100).ok_or_else(|| invalid("steps must contain 1..100 entries"))?;
    let mut steps = Vec::new();
    for (index, v) in raw.iter().enumerate() {
        steps.push(parse_step(v).map_err(|e| invalid(format!("step {index}: {e}")))?);
    }
    let entries =
        field(f, "source_versions")?.as_array().filter(|a| a.len() <= 256).ok_or_else(|| invalid("source_versions must be an array of at most256 items"))?;
    let mut sources = Vec::new();
    let mut seen = BTreeSet::new();
    for v in entries {
        let f = object(v, &["item", "source_version"])?;
        let item = ItemId(id(field(f, "item")?)?);
        if !seen.insert(item) {
            return Err(invalid("duplicate source version item"));
        }
        let version = string(field(f, "source_version")?, 71, false)?;
        if version.len() != 71 || !version.starts_with("sha256:") || !version.bytes().skip(7).all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(invalid("source_version must be an analysis SHA256 version"));
        }
        sources.push(SourceVersion { item, version });
    }
    let mut canonical = p.clone();
    if let Some(f) = canonical.as_object_mut() {
        f.remove("request_id");
    }
    Ok(Plan {
        apply: false,
        #[cfg(test)]
        panic_after_step: None,
        request_id,
        expected_revision,
        sources,
        steps,
        canonical,
    })
}
fn parse_step(v: &Value) -> Result<Step> {
    let f = object(v, &["id", "params", "reason"])?;
    let id = string(field(f, "id")?, 64, false)?;
    if !allowed_command(&id) {
        return Err(invalid(format!("command {id} has no isolated plan executor")));
    }
    let reason = string(field(f, "reason")?, 512, true)?;
    let p = field(f, "params")?;
    let op = match id.as_str() {
        "timeline.trim" => {
            let f = object(p, &["clip", "edge", "mode", "delta"])?;
            let clip = ClipId(self::id(field(f, "clip")?)?);
            let edge = match field(f, "edge")?.as_str() {
                Some("in") => Edge::In,
                Some("out") => Edge::Out,
                _ => return Err(invalid("edge must be in or out")),
            };
            let mode = match f.get("mode").and_then(Value::as_str) {
                None if !f.contains_key("mode") => TrimMode::Regular,
                Some("regular") => TrimMode::Regular,
                Some("ripple") => TrimMode::Ripple,
                _ => return Err(invalid("mode must be regular or ripple")),
            };
            let delta = tick(field(f, "delta")?, true)?;
            if delta == Tick::ZERO {
                return Err(invalid("trim delta must be nonzero"));
            }
            Operation::Trim { clip, edge, mode, delta }
        }
        "timeline.move" => {
            let f = object(p, &["moves", "linked", "insert", "copy"])?;
            let raw = field(f, "moves")?
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= 100)
                .ok_or_else(|| invalid("moves must contain 1..100 explicit targets"))?;
            let mut moves = Vec::new();
            let mut seen = BTreeSet::new();
            for m in raw {
                let m = object(m, &["clip", "track", "time"])?;
                let clip = ClipId(self::id(field(m, "clip")?)?);
                if !seen.insert(clip) {
                    return Err(invalid("duplicate moved clip"));
                }
                moves.push((clip, TrackId(self::id(field(m, "track")?)?), tick(field(m, "time")?, false)?));
            }
            let linked = f.get("linked").map(|v| v.as_bool().ok_or_else(|| invalid("linked must be boolean"))).transpose()?;
            Operation::Move { moves, linked, insert: boolean(f, "insert", false)?, copy: boolean(f, "copy", false)? }
        }
        "edit.clear" => {
            let f = object(p, &["clips"])?;
            let raw = field(f, "clips")?
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= 100)
                .ok_or_else(|| invalid("clips must contain 1..100 explicit targets"))?;
            let clips = raw.iter().map(|v| self::id(v).map(ClipId)).collect::<Result<Vec<_>>>()?;
            Operation::Clear { clips }
        }
        "captions.setText" => {
            let f = object(p, &["caption", "text", "speaker"])?;
            let caption = ClipId(self::id(field(f, "caption")?)?);
            let text = f.get("text").map(|v| string(v, 65536, true)).transpose()?;
            let speaker = f.get("speaker").map(|v| if v.is_null() { Ok(None) } else { string(v, 256, true).map(Some) }).transpose()?;
            if text.is_none() && speaker.is_none() {
                return Err(invalid("caption requires text or speaker"));
            }
            Operation::Caption { caption, text, speaker }
        }
        "graphics.set" => {
            let f = object(p, &["clip", "layer", "time", "props"])?;
            let clip = ClipId(self::id(field(f, "clip")?)?);
            let layer = field(f, "layer")?.clone();
            if let Some(n) = layer.as_u64() {
                if n > 10000 {
                    return Err(invalid("layer index exceeds budget"));
                }
            } else {
                string(&layer, 128, false)?;
            }
            let time = tick(field(f, "time")?, false)?;
            let props = field(f, "props")?
                .as_object()
                .filter(|p| !p.is_empty() && p.len() <= 128)
                .ok_or_else(|| invalid("props must be a bounded nonempty object"))?
                .clone();
            if props.get("enabled").is_some_and(|v| !v.is_boolean()) {
                return Err(invalid("enabled must be boolean"));
            }
            Operation::Graphic { clip, layer, time, props }
        }
        _ => return Err(invalid("command has no isolated executor")),
    };
    Ok(Step { id, reason, op })
}

pub(super) struct Snapshot {
    pub project: Arc<Project>,
    pub state: EditorState,
    pub revision: u64,
}
pub(super) struct Candidate {
    pub project: Project,
    pub state: EditorState,
    pub report: Value,
}
fn check_project(p: &Project) -> Result<()> {
    if p.items.len() > 100000 || p.next_id.checked_add(1_000_000_000).is_none() {
        return Err(invalid("project item/ID allocation budget exceeded"));
    }
    let mut count = 0usize;
    for item in p.sequences() {
        if let ItemKind::Sequence(q) = &item.kind {
            q.check().map_err(invalid)?;
            for track in q.all_tracks() {
                count = count.saturating_add(track.items.len());
                for clip in &track.items {
                    let scaled = clip.duration.0 as f64 * clip.speed.abs();
                    if !scaled.is_finite() || scaled < 0.0 || scaled > Tick::MAX.0 as f64 {
                        return Err(invalid("clip source span exceeds supported bounds"));
                    }
                    let duration = Tick(scaled.round() as i64);
                    if clip.source_in < Tick::ZERO
                        || clip.source_in > Tick::MAX
                        || !clip.speed.is_finite()
                        || clip.speed <= 0.0
                        || duration < Tick::ZERO
                        || duration > Tick::MAX
                        || clip.source_in.0.checked_add(duration.0).is_none_or(|n| n > Tick::MAX.0)
                    {
                        return Err(invalid("clip source timing is invalid or outside supported bounds"));
                    }
                }
            }
            count = count.saturating_add(q.caption_tracks.iter().map(|t| t.captions.len()).sum::<usize>());
            if count > 100000 {
                return Err(invalid("plan project exceeds 100000 timeline entities"));
            }
        }
    }
    Ok(())
}
fn targets(q: &Sequence, state: &EditorState, step: &Step) -> Result<Vec<ClipId>> {
    let clips = match &step.op {
        Operation::Trim { clip, .. } | Operation::Graphic { clip, .. } => vec![*clip],
        Operation::Move { moves, .. } => moves.iter().map(|m| m.0).collect(),
        Operation::Clear { clips } => clips.clone(),
        Operation::Caption { caption, .. } => {
            let (track, _) = q.find_caption(*caption).ok_or_else(|| invalid("caption does not exist"))?;
            if q.caption_track(track).is_none_or(|t| t.locked) {
                return Err(invalid("caption track is locked"));
            }
            return Ok(Vec::new());
        }
    };
    for id in &clips {
        let (track, _) = q.find_item(*id).ok_or_else(|| invalid(format!("clip {} does not exist", id.0)))?;
        if q.track(track).is_none_or(|t| t.locked) {
            return Err(invalid("target track is locked"));
        }
    }
    Ok(match &step.op {
        Operation::Trim { .. } | Operation::Clear { .. } => crate::commands::linked_clips(q, &clips, state.linked_selection),
        Operation::Move { moves, linked, .. } => {
            crate::commands::plan_clip_moves(q, moves, linked.unwrap_or(state.linked_selection))?.into_iter().map(|m| m.0).collect()
        }
        _ => clips,
    })
}
fn duration(p: &Project, verified: &BTreeMap<ItemId, Tick>, item: ItemId) -> Option<Tick> {
    let fallback = crate::project_media_duration(p, item);
    let Some((root, media, _)) = p.resolve_media(item) else {
        return fallback;
    };
    if matches!(media.info.kind, filmcraft_media::MediaKind::Still | filmcraft_media::MediaKind::Synthetic) {
        return fallback;
    }
    match (fallback, verified.get(&root)) {
        (Some(a), Some(b)) => Some(a.min(*b)),
        (None, Some(b)) => Some(*b),
        _ => fallback,
    }
}
pub(super) fn compute(snapshot: &Snapshot, plan: &Plan, verified: &BTreeMap<ItemId, Tick>) -> Result<Candidate> {
    check_project(&snapshot.project)?;
    let seq_id = snapshot.state.active_sequence.ok_or_else(|| invalid("no active sequence"))?;
    let mut project = (*snapshot.project).clone();
    let mut state = snapshot.state.clone();
    let mut reports = Vec::new();
    for (index, step) in plan.steps.iter().enumerate() {
        check_cancelled()?;
        let result = (|| {
            let q = project.sequence(seq_id).ok_or_else(|| invalid("active sequence no longer exists"))?;
            let affected = targets(q, &state, step)?;
            for clip in &affected {
                let (_, it) = q.find_item(*clip).ok_or_else(|| invalid("linked clip does not exist"))?;
                if let Some((root, _, _)) = project.resolve_media(it.item)
                    && !verified.contains_key(&root)
                {
                    return Err(invalid(format!("SOURCE_VERSION_REQUIRED for item {}", it.item.0)));
                }
            }
            let lookup = project.clone();
            let durations = |id| duration(&lookup, verified, id);
            let starts = |id| crate::media_start(&lookup, id);
            let mut next = project.next_id;
            {
                let q = project.sequence_mut(seq_id).ok_or_else(|| invalid("missing sequence"))?;
                let mut ctx =
                    EditCtx { next_id: &mut next, media_duration: &durations, media_start: &starts, min_duration: q.settings.frame_rate.frame_duration() };
                match &step.op {
                    Operation::Trim { clip, edge, mode, delta } => {
                        let before = q.find_item(*clip).map(|(_, i)| i.range());
                        let mut applied = crate::commands::clamp_trim_linked(q, &affected, *edge, *mode, *delta, &ctx)?;
                        if *mode == TrimMode::Ripple {
                            applied = edit::ripple_trim_group(q, &affected, *edge, applied, &mut ctx)?;
                            if state.ripple_sequence_markers
                                && let Some(range) = before
                            {
                                let (from, shift) = match edge {
                                    Edge::In if applied > Tick::ZERO => (range.start + applied, -applied),
                                    Edge::In => (range.start, -applied),
                                    Edge::Out => (range.end(), applied),
                                };
                                crate::sequence_tools::ripple_markers(&mut q.markers, from, shift);
                            }
                        } else {
                            for clip in &affected {
                                edit::trim(q, *clip, *edge, *mode, applied, &mut ctx)?;
                            }
                        }
                        if applied == Tick::ZERO {
                            return Err(invalid("trim has no available source or timeline handle"));
                        }
                    }
                    Operation::Move { moves, linked, insert, copy } => {
                        let moves = crate::commands::plan_clip_moves(q, moves, linked.unwrap_or(state.linked_selection))?;
                        if *copy {
                            state.selection = edit::copy_items(q, &moves, *insert, &mut ctx)?;
                            state.transition_selection.clear();
                        } else {
                            edit::move_items(q, &moves, *insert, &mut ctx)?;
                        }
                    }
                    Operation::Clear { .. } => {
                        edit::delete_items(q, &affected);
                        state.selection.clear();
                    }
                    Operation::Caption { caption, text, speaker } => {
                        edit::captions::set_caption(q, *caption, text.as_deref(), speaker.as_ref().map(|s| s.as_deref()))?
                    }
                    Operation::Graphic { clip, layer, time, props } => {
                        let (_, it) = q.find_item_mut(*clip).ok_or_else(|| invalid("graphic clip does not exist"))?;
                        if it.duration <= Tick::ZERO {
                            return Err(invalid("graphic duration must be positive"));
                        }
                        let indices = filmcraft_project::graphic::layer_indices(&it.effects);
                        let index = if let Some(n) = layer.as_u64() {
                            usize::try_from(n).ok()
                        } else {
                            layer.as_str().and_then(|name| {
                                indices.iter().enumerate().find_map(|(i, ei)| {
                                    it.effects.get(*ei).filter(|e| filmcraft_project::graphic::layer_display_name(e, i).eq_ignore_ascii_case(name)).map(|_| i)
                                })
                            })
                        }
                        .and_then(|i| indices.get(i).copied())
                        .ok_or_else(|| invalid("graphic layer does not exist"))?;
                        let mt = it.source_time_at((*time).clamp(it.start, it.end() - Tick(1)));
                        let effect = it.effects.get_mut(index).ok_or_else(|| invalid("graphic layer is missing"))?;
                        crate::graphics::apply_props(effect, props, mt)?;
                    }
                }
                q.check().map_err(invalid)?;
            }
            project.next_id = next;
            if matches!(step.op, Operation::Graphic { .. }) {
                crate::graphic_templates::sync_source_graphics_project(&mut project);
            }
            check_project(&project)?;
            Ok(json!({"index":index,"id":step.id,"reason":step.reason,"status":"valid",
                "affected_clips":affected.iter().take(1000).map(|id|id.0).collect::<Vec<_>>(),"affected_clip_count":affected.len()}))
        })();
        #[cfg(test)]
        if plan.panic_after_step == Some(index) {
            panic!("injected isolated edit panic");
        }
        reports.push(result.map_err(|e: EngineError| invalid(format!("step {index} ({}): {e}", step.id)))?);
    }
    let changed = project.items.iter().filter(|(id, item)| snapshot.project.items.get(id) != Some(*item)).map(|(id, _)| id.0).collect::<Vec<_>>();
    let report = json!({"schema_version":1,"status":"complete","committed":false,"expected_revision":snapshot.revision.to_string(),
        "steps":reports,"diff":{"changed_items":changed.iter().take(1000).copied().collect::<Vec<_>>(),"changed_item_count":changed.len()}});
    Ok(Candidate { project, state, report })
}
