//! Persisted-state changes: save, assign and publish an event.

use super::*;

/// The clone-mutate-save-assign tail: the durable file first, then the in-memory copy, so a
/// failed save leaves `state.persisted` as it was. The fsync stays under the caller's lock.
pub(crate) fn commit_persisted(
    daemon: &Daemon,
    state: &mut State,
    persisted: Persisted,
) -> Result<(), ApiError> {
    state.persist(&daemon.state_path, persisted).map_err(io_api)
}

/// `commit_persisted`, then publish the change under `revision`.
pub(crate) fn commit_publish(
    daemon: &Daemon,
    state: &mut State,
    persisted: Persisted,
    revision: u64,
    entity: &str,
    id: u64,
) -> Result<(), ApiError> {
    commit_persisted(daemon, state, persisted)?;
    publish(daemon, state, revision, entity, id);
    Ok(())
}

pub(crate) fn mutate<F>(daemon: &Arc<Daemon>, mutation: F) -> Result<Response, ApiError>
where
    F: FnOnce(&mut Persisted, u64) -> Result<(&'static str, u64), ApiError>,
{
    let mut state = lock(&daemon.state);
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let (entity, id) = mutation(&mut persisted, revision)?;
    if let Some(session) = persisted
        .sessions
        .iter_mut()
        .find(|session| session.id.0 == id)
    {
        session.revision = revision;
    }
    commit_persisted(daemon, &mut state, persisted)?;
    publish(daemon, &mut state, revision, entity, id);
    Ok(Response::Ack { revision })
}

pub(crate) fn mutate_runtime<F>(daemon: &Daemon, mutation: F) -> Result<Response, ApiError>
where
    F: FnOnce(&mut State) -> Result<(), ApiError>,
{
    let mut state = lock(&daemon.state);
    mutation(&mut state)?;
    Ok(Response::Ack {
        revision: state.revision,
    })
}

pub(crate) fn publish(daemon: &Daemon, state: &mut State, revision: u64, entity: &str, id: u64) {
    state.revision = revision;
    state.push_event(Event::Changed {
        revision,
        entity: entity.into(),
        id,
    });
    state.push_plugin_event(entity, revision, id);
    daemon.changed.notify_all();
    daemon.plugin_changed.notify_one();
}

pub(crate) fn bump(daemon: &Daemon, state: &mut State, entity: &str, id: u64) -> u64 {
    let revision = state.revision.saturating_add(1);
    publish(daemon, state, revision, entity, id);
    revision
}
