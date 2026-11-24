use super::rollout::codex_rollout;
use super::*;

pub(super) enum Admission<T = SourceFile> {
    Ready(T),
    Wait,
    Refused(String),
}

/// Finds the exact transcript file and attaches the source to the store. Every
/// refusal is decided before the store is written, so it cannot disturb the
/// other sources.
pub(super) fn admit(
    store: &Store,
    tracked: &mut Tracked,
    limits: &NodeLimits,
    boot: Uuid,
) -> Result<Admission> {
    if tracked.spec.native_session_key.is_empty() {
        let Some((day, key)) = codex_rollout(&tracked.spec.transcript, limits.discovery_entries)?
        else {
            return Ok(Admission::Wait);
        };
        tracked.source.native_session_key = Text::new(key.as_str())?;
        tracked.spec.native_session_key = key;
        tracked.spec.transcript = day;
    }
    let (spec, source) = (&tracked.spec, &tracked.source);
    if let Some(row) = store.source_optional(source.source_id)? {
        return resume_source(row, source);
    }
    if !matches!(source.binding.session, SessionIdentity::Remote { boot_id, .. } if boot_id == boot)
    {
        return Ok(Admission::Refused(
            "its session belongs to another boot of this node".into(),
        ));
    }
    let permit = match discover(spec, source, limits.discovery_entries)? {
        Admission::Ready(p) => p,
        Admission::Wait => return Ok(Admission::Wait),
        Admission::Refused(reason) => return Ok(Admission::Refused(reason)),
    };
    if store.source_count()? >= limits.max_sources {
        // Freed when a finished source is released.
        return Ok(Admission::Wait);
    }
    store.attach_file_engine(source.clone(), limits.tracked_identities, permit.clone())?;
    Ok(Admission::Ready(SourceFile::open(permit)?))
}

fn resume_source(row: SourceRow, source: &engine::Source) -> Result<Admission> {
    let Some(reader) = row.reader else {
        return Ok(Admission::Refused("stored source has no transcript".into()));
    };
    if serde_json::to_value(&row.source)? != serde_json::to_value(source)? {
        return Ok(Admission::Refused(
            "stored source differs from this launch".into(),
        ));
    }
    Ok(Admission::Ready(SourceFile::open(reader.permit)?))
}

fn discover(
    spec: &SourceSpec,
    source: &engine::Source,
    entries: usize,
) -> Result<Admission<aq_observation::source::SourcePermit>> {
    let (root, member) = match std::fs::symlink_metadata(&spec.transcript) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Admission::Wait),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_dir() => (spec.transcript.clone(), None),
        Ok(_) => (
            spec.transcript
                .parent()
                .context("transcript parent")?
                .to_path_buf(),
            Some(
                (spec.transcript.file_name().and_then(|name| name.to_str()))
                    .context("transcript file name")?
                    .to_owned(),
            ),
        ),
    };
    if std::fs::canonicalize(&root)? != root
        || std::fs::canonicalize(&spec.authorized_root)? != spec.authorized_root
    {
        return Ok(Admission::Refused(
            "transcript path leaves its authorized root through a link".into(),
        ));
    }
    let discovery = Discovery {
        source_id: source.source_id,
        root,
        native_session_key: source.native_session_key.clone(),
        format: serde_json::from_value(serde_json::to_value(&source.format)?)?,
        exact_member: member,
        maximum_entries: entries,
        maximum_candidates: DISCOVERY_CANDIDATES,
    };
    let resolution = match spec.owner_uid {
        Some(uid) => discovery.resolve_for_owner(uid)?,
        None => discovery.resolve()?,
    };
    let permit = match resolution {
        Resolution::Found(permit) => permit,
        Resolution::Missing | Resolution::NotReady => return Ok(Admission::Wait),
        other => return Ok(Admission::Refused(format!("transcript refused: {other:?}"))),
    };
    Ok(Admission::Ready(permit))
}
