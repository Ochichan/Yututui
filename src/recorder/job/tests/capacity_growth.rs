use super::*;

#[test]
fn automatic_admission_recounts_growth_after_close_ack() {
    let dir = temp_dir("capacity-growth-after-ack");
    let temp_root = dir.join("temp");
    let final_dir = dir.join("final");
    let first_source = temp_root.join("first.mkv");
    let second_source = temp_root.join("second.mkv");
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(&first_source, b"1234").unwrap();
    std::fs::write(&second_source, b"8").unwrap();

    let barrier = crate::recorder::barrier::CommandBarrier::pending();
    let signal = barrier.signal();
    let first = durable::accept_with_limits(
        automatic(with_barrier(
            save_job(140, first_source.clone(), final_dir.clone(), "First"),
            barrier,
        )),
        10,
        7,
    )
    .unwrap();

    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&first_source)
        .unwrap();
    writer.write_all(b"567").unwrap();
    writer.sync_all().unwrap();
    drop(writer);
    signal.succeed();

    let blocked = match durable::accept_with_limits(
        automatic(save_job(
            141,
            second_source.clone(),
            final_dir.clone(),
            "Second",
        )),
        10,
        7,
    ) {
        Err(event) => event,
        Ok(_) => panic!("the next automatic source must see the acknowledged source's live size"),
    };
    assert!(matches!(
        blocked,
        RecorderEvent::CapacityBlocked {
            id: 141,
            pending_count: 2,
            pending_bytes: 8,
        }
    ));
    assert_eq!(std::fs::read(&first_source).unwrap(), b"1234567");
    assert_eq!(std::fs::read(&second_source).unwrap(), b"8");

    let saved = saved_path(run_accepted(first));
    assert_eq!(std::fs::read(saved).unwrap(), b"1234567");
    let second = durable::accept_with_limits(
        automatic(save_job(141, second_source, final_dir, "Second")),
        10,
        7,
    )
    .unwrap();
    assert!(matches!(run_accepted(second), RecorderEvent::Saved { .. }));
    let _ = std::fs::remove_dir_all(dir);
}
