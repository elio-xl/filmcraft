//! Commit only a worker-produced candidate whose live references still match.
use super::validation::{Candidate, Snapshot};
use crate::media_analysis::source::{AnalysisInput, AnalysisSource, SourceIdentity, failure, io_failure};
use crate::{Result, Services, Session};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) struct Guard {
    services: Arc<dyn Services>,
    stamps: Vec<(String, String)>,
}
impl Guard {
    pub fn capture(input: &AnalysisInput) -> Result<Self> {
        let mut paths = Vec::new();
        match &input.identity {
            SourceIdentity::File { path } => paths.push(path.clone()),
            SourceIdentity::Generator { .. } => {}
            SourceIdentity::Sequence { first, .. } => {
                let directory = std::path::Path::new(first).parent().and_then(|p| p.to_str()).filter(|p| !p.is_empty()).unwrap_or(".").to_owned();
                paths.push(directory);
                paths.extend(crate::media_analysis::source::sequence_inventory(&*input.services, first)?.into_iter().flatten());
            }
        }
        if paths.len() > 256 {
            return Err(failure("RESOURCE_LIMIT", "apply permits at most 256 source paths including sequence directories", false));
        }
        let stamps = paths
            .into_iter()
            .map(|path| {
                let stamp = stamp(&*input.services, &path)?;
                Ok((path, stamp))
            })
            .collect::<Result<_>>()?;
        Ok(Self { services: input.services.clone(), stamps })
    }
    pub fn len(&self) -> usize {
        self.stamps.len()
    }
    pub fn check(&self) -> Result<()> {
        for (path, expected) in &self.stamps {
            if stamp(&*self.services, path)? != *expected {
                return Err(failure("SOURCE_CHANGED", "source identity or metadata changed; inspect and rebuild the plan", true));
            }
        }
        Ok(())
    }
}
fn stamp(services: &dyn Services, path: &str) -> Result<String> {
    let token = services.source_stamp(path).map_err(io_failure)?;
    if token.is_empty() || token.len() > 1024 {
        return Err(failure("CAPABILITY_UNAVAILABLE", "host source stamp is empty or oversized", false));
    }
    Ok(token)
}
pub(super) struct Ready {
    pub snapshot: Snapshot,
    pub candidate: Candidate,
    pub guards: Vec<Guard>,
    // Retain the actual opened/verified source handles until commit or discard.
    pub sources: Vec<AnalysisSource>,
}
pub(super) fn commit(s: &mut Session, ready: Ready) -> Result<Value> {
    let Ready { snapshot, candidate, guards, sources } = ready;
    if snapshot.revision != s.revision {
        return Err(failure("REVISION_MISMATCH", "user edits superseded this plan; read the document again", false));
    }
    if !Arc::ptr_eq(&snapshot.project, &s.project) {
        return Err(failure("PROJECT_CHANGED", "project was replaced or closed", false));
    }
    if sources.iter().any(|source| !Arc::ptr_eq(&source.services, &s.services)) {
        return Err(failure("SOURCE_CHANGED", "source host changed", false));
    }
    if s.trim_play.active()
        || s.state.active_sequence != snapshot.state.active_sequence
        || s.state.linked_selection != snapshot.state.linked_selection
        || s.state.ripple_sequence_markers != snapshot.state.ripple_sequence_markers
    {
        return Err(failure("CONTEXT_CHANGED", "active sequence or editing policy changed", false));
    }
    let selection = candidate.state.selection != snapshot.state.selection;
    let transitions = candidate.state.transition_selection != snapshot.state.transition_selection;
    if (selection && s.state.selection != snapshot.state.selection) || (transitions && s.state.transition_selection != snapshot.state.transition_selection) {
        return Err(failure("CONTEXT_CHANGED", "user selection changed while the plan was running", false));
    }
    let after = s.revision.checked_add(1).ok_or_else(|| failure("RESOURCE_LIMIT", "revision space exhausted", false))?;
    for guard in &guards {
        guard.check()?;
    }
    let mut report = candidate.report;
    report["committed"] = json!(true);
    report["revision_before"] = json!(s.revision.to_string());
    report["revision_after"] = json!(after.to_string());
    report["undo_label"] = json!("Apply Editing Plan");
    if crate::media_analysis::cache::serialized_size(&report)? > 1024 * 1024 {
        return Err(failure("RESOURCE_LIMIT", "commit report exceeds 1 MiB", false));
    }
    s.edit("Apply Editing Plan", move |p, state| {
        *p = candidate.project;
        if selection {
            state.selection = candidate.state.selection;
        }
        if transitions {
            state.transition_selection = candidate.state.transition_selection;
        }
        Ok(())
    })?;
    Ok(report)
}
