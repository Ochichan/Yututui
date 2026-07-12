use super::*;

#[tokio::test]
async fn connection_children_are_empty_after_abort_and_join() {
    let mut connections = JoinSet::new();
    connections.spawn(std::future::pending::<()>());
    connections.spawn(std::future::pending::<()>());
    assert_eq!(connections.len(), 2);

    abort_and_join_connection_tasks(&mut connections).await;

    assert!(connections.is_empty());
}

#[tokio::test]
async fn instance_guard_drop_latches_hub_and_aborts_accept_fallback() {
    let hub = test_hub();
    let serve_task = tokio::spawn(std::future::pending::<()>());
    let abort_handle = serve_task.abort_handle();
    let endpoint_lease = Arc::new(EndpointLease::new(
        session_socket_tests::test_endpoint("guard-abort-fallback"),
        #[cfg(unix)]
        None,
    ));
    endpoint_lease.finish_publication(None);
    let guard = InstanceGuard {
        endpoint_lease,
        hub: Arc::clone(&hub),
        serve_task: Some(serve_task),
    };

    drop(guard);

    timeout(Duration::from_millis(200), async {
        while !abort_handle.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("guard drop must abort its accept task");
    assert!(hub.is_shutting_down());
}
