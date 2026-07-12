use super::*;

#[cfg(unix)]
#[tokio::test]
async fn dropping_bound_server_before_start_releases_lease_or_recovery_failure_endpoint() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let mut suffix = [0_u8; 8];
    getrandom::fill(&mut suffix).unwrap();
    let suffix = suffix
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // macOS sockaddr_un has a very small path budget, while its process temp directory can be
    // deeply nested. Keep this endpoint deliberately short and place it inside a private root so
    // the test exercises cleanup rather than failing before bind.
    let root = std::path::Path::new("/tmp").join(format!("ytt-sc-{}-{suffix}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        std::fs::symlink_metadata(&root)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let endpoint = root.join("owner.sock").to_string_lossy().into_owned();
    let listener = bind(&endpoint).unwrap();
    assert!(std::path::Path::new(&endpoint).exists());
    let server = RemoteServer {
        listener: Some(listener),
        token: Arc::from("test"),
        endpoint: endpoint.clone(),
        owns_instance_file: true,
        mode: InstanceMode::StandaloneTui,
        capabilities: Vec::new(),
    };

    drop(server);

    assert!(
        !std::path::Path::new(&endpoint).exists(),
        "an early lease/recovery return must not strand the just-bound endpoint"
    );
    std::fs::remove_dir(&root).unwrap();
}
