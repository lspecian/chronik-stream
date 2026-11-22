pub mod autoscaler_controller;
pub mod cluster_controller;
pub mod standalone_controller;
pub mod topic_controller;
pub mod user_controller;

/// Log a controller stream error. A reconcile queued for an object that has
/// since been deleted (`ObjectNotFound`) is routine after every delete, not an
/// error.
pub(crate) fn log_controller_error<E: std::fmt::Debug, Q: std::fmt::Debug>(
    kind: &str,
    error: &kube::runtime::controller::Error<E, Q>,
) {
    match error {
        kube::runtime::controller::Error::ObjectNotFound(obj) => {
            tracing::debug!("{kind} {} no longer exists; nothing to reconcile", obj.name);
        }
        other => tracing::error!("{kind} reconciliation error: {other:?}"),
    }
}
