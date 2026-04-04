use forge_core::audit::{AuditEvent, AuditQuery, AuditReader, AuditWriter};
use serde_json::Value;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

fn wait_for_events(reader: &AuditReader, min_count: usize) -> Vec<forge_core::audit::AuditRecord> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let events = reader
            .query_events(AuditQuery::default(), None)
            .expect("query failed");
        if events.len() >= min_count {
            return events;
        }
        assert!(Instant::now() < deadline, "timed out waiting for events");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn setup() -> (AuditWriter, AuditReader) {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_path_buf();
    // Keep file alive via the path; NamedTempFile is dropped but path persists on disk
    // until the returned objects are dropped.
    std::mem::forget(file);
    let writer = AuditWriter::new(&path).unwrap();
    let reader = AuditReader::open(&path).unwrap();
    (writer, reader)
}

#[test]
fn filter_by_server() {
    let (writer, reader) = setup();
    writer.log(AuditEvent::new("alpha", "tool1", &Value::Null, 0, 10, None, None));
    writer.log(AuditEvent::new("beta", "tool2", &Value::Null, 0, 10, None, None));
    drop(writer);

    let all = wait_for_events(&reader, 2);
    assert_eq!(all.len(), 2);

    let alpha = reader
        .query_events(
            AuditQuery { server: Some("alpha".to_owned()), ..Default::default() },
            None,
        )
        .unwrap();
    assert_eq!(alpha.len(), 1);
    assert_eq!(alpha[0].server, "alpha");
}

#[test]
fn filter_by_tool() {
    let (writer, reader) = setup();
    writer.log(AuditEvent::new("srv", "build", &Value::Null, 0, 5, None, None));
    writer.log(AuditEvent::new("srv", "test", &Value::Null, 0, 5, None, None));
    drop(writer);

    wait_for_events(&reader, 2);

    let build_only = reader
        .query_events(
            AuditQuery { tool: Some("build".to_owned()), ..Default::default() },
            None,
        )
        .unwrap();
    assert_eq!(build_only.len(), 1);
    assert_eq!(build_only[0].tool, "build");
}

#[test]
fn filter_errors_only() {
    let (writer, reader) = setup();
    writer.log(AuditEvent::new("srv", "ok", &Value::Null, 0, 5, None, None));
    writer.log(AuditEvent::new(
        "srv",
        "fail",
        &Value::Null,
        -1,
        5,
        Some("something broke".to_owned()),
        None,
    ));
    drop(writer);

    wait_for_events(&reader, 2);

    let errors = reader
        .query_events(AuditQuery { errors_only: true, ..Default::default() }, None)
        .unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].tool, "fail");
    assert!(errors[0].error.is_some());
}

#[test]
fn limit_is_respected() {
    let (writer, reader) = setup();
    for i in 0..10 {
        writer.log(AuditEvent::new(
            "srv",
            &format!("tool{}", i),
            &Value::Null,
            0,
            1,
            None,
            None,
        ));
    }
    drop(writer);

    wait_for_events(&reader, 10);

    let limited = reader
        .query_events(AuditQuery::default(), Some(3))
        .unwrap();
    assert_eq!(limited.len(), 3);
}

#[test]
fn results_ordered_newest_first() {
    let (writer, reader) = setup();
    writer.log(AuditEvent::new("srv", "first", &Value::Null, 0, 1, None, None));
    // Small sleep to ensure distinct timestamps.
    std::thread::sleep(Duration::from_millis(5));
    writer.log(AuditEvent::new("srv", "second", &Value::Null, 0, 1, None, None));
    drop(writer);

    let events = wait_for_events(&reader, 2);
    // ORDER BY ts DESC — newest first.
    assert_eq!(events[0].tool, "second");
    assert_eq!(events[1].tool, "first");
}
