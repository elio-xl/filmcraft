//! Bounded, actual candidate differences, including entities changed as a side effect.
use crate::Result;
use crate::media_analysis::source::check_cancelled;
use filmcraft_project::{Caption, ItemKind, Marker, Project, Sequence, TrackItem, Transition};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) struct Budget {
    rows: usize,
    bytes: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self { rows: 256, bytes: 224 * 1024 }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Entity<'a> {
    Clip(u64, &'a TrackItem),
    Caption(u64, &'a Caption),
    Transition(u64, &'a Transition),
    Marker(&'a Marker),
}
type Key = (u64, &'static str, u64);
fn entities<'a>(sequence: u64, q: &'a Sequence, out: &mut BTreeMap<Key, Entity<'a>>) {
    for t in q.all_tracks() {
        for c in &t.items {
            out.insert((sequence, "clip", c.id.0), Entity::Clip(t.id.0, c));
        }
        for c in &t.transitions {
            out.insert((sequence, "transition", c.id.0), Entity::Transition(t.id.0, c));
        }
    }
    for t in &q.caption_tracks {
        for c in &t.captions {
            out.insert((sequence, "caption", c.id.0), Entity::Caption(t.id.0, c));
        }
    }
    for m in &q.markers {
        out.insert((sequence, "marker", m.id.0), Entity::Marker(m));
    }
}
fn text(v: &str) -> String {
    v.chars().take(256).collect()
}
impl Entity<'_> {
    fn state(self) -> Value {
        match self {
            Self::Clip(track, c) => json!({"track":track,"item":c.item.0,
                "start":c.start.0.to_string(),"duration":c.duration.0.to_string(),"end":c.end().0.to_string(),
                "source_in":c.source_in.0.to_string(),"speed":c.speed,"reverse":c.reverse,
                "link":c.link,"group":c.group,"effect_count":c.effects.len()}),
            Self::Caption(track, c) => json!({"track":track,
                "start":c.start.0.to_string(),"duration":c.duration.0.to_string(),"end":c.end().0.to_string(),
                "text":text(&c.text),"text_truncated":c.text.chars().nth(256).is_some(),
                "speaker":c.speaker.as_deref().map(text),"speaker_truncated":c.speaker.as_deref().is_some_and(|s|s.chars().nth(256).is_some())}),
            Self::Transition(track, c) => json!({"track":track,
                "start":c.start.0.to_string(),"duration":c.duration.0.to_string(),"end":c.end().0.to_string(),
                "from":c.from.map(|id|id.0),"to":c.to.map(|id|id.0)}),
            Self::Marker(m) => json!({"start":m.start.0.to_string(),"duration":m.duration.0.to_string(),
                "end":(m.start+m.duration).0.to_string(),"name":text(&m.name),"name_truncated":m.name.chars().nth(256).is_some()}),
        }
    }
    // Summaries intentionally omit bulky effects/keyframes. Flag their changes explicitly even
    // when the short before/after timing summaries are equal (e.g. a shared graphic peer).
    fn content_changed(self, after: Self) -> bool {
        match (self, after) {
            (Self::Clip(_, a), Self::Clip(_, b)) => a.effects != b.effects || a.graphic != b.graphic || a.markers != b.markers,
            (Self::Caption(_, a), Self::Caption(_, b)) => a.text != b.text || a.speaker != b.speaker,
            (Self::Transition(_, a), Self::Transition(_, b)) => a.effect != b.effect,
            (Self::Marker(a), Self::Marker(b)) => a.name != b.name || a.comment != b.comment,
            _ => true,
        }
    }
}
pub(super) struct Diff {
    pub value: Value,
    pub affected: Vec<u64>,
    pub affected_count: usize,
}
pub(super) fn between(before: &Project, after: &Project, budget: &mut Budget) -> Result<Diff> {
    let mut old = BTreeMap::new();
    let mut new = BTreeMap::new();
    for item in before.sequences() {
        if let ItemKind::Sequence(q) = &item.kind
            && after.sequence(item.id) != Some(q)
        {
            check_cancelled()?;
            entities(item.id.0, q, &mut old);
        }
    }
    for item in after.sequences() {
        if let ItemKind::Sequence(q) = &item.kind
            && before.sequence(item.id) != Some(q)
        {
            check_cancelled()?;
            entities(item.id.0, q, &mut new);
        }
    }
    let mut rows = Vec::new();
    let mut affected = Vec::new();
    let mut count = 0usize;
    let mut affected_count = 0usize;
    // Deleted/updated entities first, followed by creations. Keys include sequence + entity kind
    // so shared instances in other sequences and captions are not collapsed into a target ID.
    for key in old.keys().chain(new.keys().filter(|key| !old.contains_key(key))) {
        check_cancelled()?;
        let a = old.get(key).copied();
        let b = new.get(key).copied();
        if a == b {
            continue;
        }
        count = count.saturating_add(1);
        let is_clip = matches!(key.1, "clip" | "caption");
        if is_clip {
            affected_count = affected_count.saturating_add(1);
        }
        if budget.rows == 0 || budget.bytes == 0 {
            continue;
        }
        let row = json!({"sequence":key.0,"kind":key.1,"id":key.2,
            "change":if a.is_none(){"created"}else if b.is_none(){"deleted"}else{"modified"},
            "before":a.map(Entity::state),"after":b.map(Entity::state),
            "content_changed":match (a,b) {(Some(a),Some(b))=>a.content_changed(b),_=>true}});
        let bytes = crate::media_analysis::cache::serialized_size(&row)?.saturating_add(1);
        if bytes > budget.bytes {
            budget.bytes = 0;
            continue;
        }
        budget.rows -= 1;
        budget.bytes -= bytes;
        if is_clip {
            affected.push(key.2);
        }
        rows.push(row);
    }
    Ok(Diff {
        value: json!({"entity_count":count,"omitted_entity_count":count.saturating_sub(rows.len()),
        "summary_only":true,"entities":rows}),
        affected,
        affected_count,
    })
}
