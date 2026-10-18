use super::*;
const APPEND: i32 = libc::O_WRONLY | libc::O_APPEND;

fn model() -> Model {
    Model::new("tester".into())
}
fn history(m: &Model) -> String {
    String::from_utf8(
        m.logs
            .read(Node::Log(0).ino(), 0, m.channels[0].history)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn cursors_and_snapshots_are_per_channel() {
    let mut m = Model::new("tester".into());
    m.remote = Some(Remote::default());
    m.configure_channels(&["#one".into(), "#two".into()])
        .unwrap();
    let old = m.open(Node::Log(1), libc::O_RDONLY).unwrap();
    m.receive("#one", 30, "2026-10-18T12:00:00Z", "alice", "one")
        .unwrap();
    m.receive("#two", 2, "2026-10-18T12:00:00Z", "bob", "two")
        .unwrap();
    m.receive("#two", 2, "2026-10-18T12:00:00Z", "bob", "two")
        .unwrap();
    assert_eq!(m.messages, 2);
    assert_eq!(m.channels[0].cursor, 30);
    assert_eq!(m.channels[1].cursor, 2);
    assert_eq!(m.read(Node::Log(1), old, 0, 4096).unwrap(), b"");
    assert_eq!(m.read(Node::Log(0), old, 0, 4096), Err(libc::EBADF));
    m.receive("#two", 31, "2026-10-18T12:00:01Z", "bob", "third")
        .unwrap();
    assert_eq!(m.channels[1].cursor, 31);
    assert_eq!(m.messages, 3);
}

#[test]
fn directory_and_topic_snapshots_survive_channel_changes() {
    let mut m = model();
    let old = m.open_dir(Node::Channels).unwrap();
    let original = m.read_dir(Node::Channels, old).unwrap().to_vec();
    let i = m.ensure_channel("#Research").unwrap();
    let log = m.lookup(Node::Channels, "research.log").unwrap();
    assert_eq!(log, Node::Log(i));
    assert_eq!(Node::from_ino(log.ino()).unwrap(), log);
    assert_eq!(m.read_dir(Node::Channels, old).unwrap(), original);
    let new = m.open_dir(Node::Channels).unwrap();
    assert!(
        m.read_dir(Node::Channels, new)
            .unwrap()
            .contains(&(log, "research.log".into()))
    );
    let topic = m.open(Node::Topic(i), libc::O_RDONLY).unwrap();
    m.channels[i].topic = "changed".into();
    assert_eq!(m.read(Node::Topic(i), topic, 0, 4096).unwrap(), b"\n");
    m.release(Node::Channels, old).unwrap();
    assert!(matches!(m.read_dir(Node::Channels, old), Err(libc::EBADF)));
}

#[test]
fn remote_queue_is_atomic_bounded_and_reports_disconnect_losses() {
    let mut m = model();
    m.remote = Some(Remote::default());
    let w = m.open(Node::Log(0), APPEND).unwrap();
    assert_eq!(
        m.write(Node::Log(0), w, b"offline\n", APPEND),
        Err(libc::ENOTCONN)
    );
    m.remote.as_mut().unwrap().ready = true;
    m.channels[0].ready = true;
    assert_eq!(
        m.write(Node::Log(0), w, &b"x\n".repeat(MAX_OUTBOUND + 1), APPEND),
        Err(libc::EAGAIN)
    );
    assert!(m.remote.as_ref().unwrap().queue.is_empty());
    m.write(Node::Log(0), w, b"first\nsecond\n", APPEND)
        .unwrap();
    assert!(m.channels[0].history == 0);
    let remote = m.remote.as_mut().unwrap();
    remote.inflight = remote.queue.pop_front();
    m.network_failed("test disconnect".into());
    let remote = m.remote.as_ref().unwrap();
    assert_eq!((remote.canceled, remote.uncertain), (1, 1));
    assert!(remote.queue.is_empty());
    assert!(remote.inflight.is_none());
}

#[test]
fn replay_is_deduplicated_without_changing_existing_snapshots() {
    let mut m = model();
    m.remote = Some(Remote::default());
    let old = m.open(Node::Log(0), libc::O_RDONLY).unwrap();
    m.receive("#general", 1, "2026-10-18T12:00:00Z", "alice", "one")
        .unwrap();
    m.receive("#general", 1, "2026-10-18T12:00:00Z", "alice", "one")
        .unwrap();
    m.receive("#general", 3, "2026-10-18T12:00:01Z", "bob", "two")
        .unwrap();
    assert_eq!(m.messages, 2);
    assert_eq!(m.channels[0].cursor, 3);
    assert_eq!(m.read(Node::Log(0), old, 0, 4096).unwrap(), b"");
}

#[test]
fn handles_assemble_independently_and_snapshots_remain_fixed() {
    let mut m = model();
    let a = m.open(Node::Log(0), APPEND).unwrap();
    let b = m.open(Node::Log(0), APPEND).unwrap();
    let reader = m.open(Node::Log(0), libc::O_RDONLY).unwrap();
    m.write(Node::Log(0), a, b"hello ", APPEND).unwrap();
    m.write(Node::Log(0), b, b"other\n", APPEND).unwrap();
    m.write(Node::Log(0), a, b"world\none\ntwo\n", APPEND)
        .unwrap();
    assert_eq!(m.messages, 4);
    assert!(history(&m).contains("tester: hello world\n"));
    assert!(history(&m).find("other\n") < history(&m).find("hello world\n"));
    assert_eq!(m.read(Node::Log(0), reader, 0, 999).unwrap(), b"");
    let fresh = m.open(Node::Log(0), libc::O_RDONLY).unwrap();
    assert_eq!(
        m.read(Node::Log(0), fresh, 3, 7).unwrap(),
        &history(&m).as_bytes()[3..10]
    );
    assert_eq!(m.read(Node::Log(0), fresh, u64::MAX, 7).unwrap(), b"");
}

#[test]
fn rejected_batch_is_atomic_and_partial_can_be_retried() {
    let mut m = model();
    let w = m.open(Node::Log(0), APPEND).unwrap();
    m.write(Node::Log(0), w, b"prefix ", APPEND).unwrap();
    assert_eq!(
        m.write(Node::Log(0), w, b"ok\ninvalid\0\n", APPEND),
        Err(libc::EINVAL)
    );
    assert!(m.channels[0].history == 0);
    m.write(Node::Log(0), w, b"retry\n", APPEND).unwrap();
    assert!(history(&m).ends_with("prefix retry\n"));
}

#[test]
fn unicode_crlf_and_byte_limits() {
    let mut m = model();
    let w = m.open(Node::Log(0), APPEND).unwrap();
    m.write(Node::Log(0), w, &[0xc3], APPEND).unwrap();
    m.write(Node::Log(0), w, &[0xa9, b'\r'], APPEND).unwrap();
    m.write(Node::Log(0), w, b"\n\n\r\n", APPEND).unwrap();
    assert_eq!(m.messages, 1);
    assert!(history(&m).ends_with("é\n"));
    let max = "é".repeat(MAX_LINE / 2);
    m.write(Node::Log(0), w, max.as_bytes(), APPEND).unwrap();
    assert_eq!(
        m.write(Node::Log(0), w, b"x\n", APPEND),
        Err(libc::EMSGSIZE)
    );
    m.write(Node::Log(0), w, b"\r\n", APPEND).unwrap();
    assert_eq!(m.write(Node::Log(0), w, &[0xff], APPEND), Err(libc::EILSEQ));
    assert_eq!(
        m.write(Node::Log(0), w, &[0xc3, b'\n'], APPEND),
        Err(libc::EILSEQ)
    );
}

#[test]
fn duplicate_flush_does_not_send_or_destroy_partial() {
    let mut m = model();
    let w = m.open(Node::Log(0), APPEND).unwrap();
    m.write(Node::Log(0), w, b"part", APPEND).unwrap();
    assert_eq!(m.flush(Node::Log(0), w), Err(libc::EINVAL));
    assert_eq!(m.flush(Node::Log(0), w), Err(libc::EINVAL));
    m.write(Node::Log(0), w, b"ial\n", APPEND).unwrap();
    m.flush(Node::Log(0), w).unwrap();
    m.flush(Node::Log(0), w).unwrap();
    assert_eq!(m.messages, 1);
    m.write(Node::Log(0), w, b"lost", APPEND).unwrap();
    m.release(Node::Log(0), w).unwrap();
    assert_eq!(m.discarded_partials, 1);
    assert_eq!(m.messages, 1);
    assert_eq!(m.flush(Node::Log(0), w), Err(libc::EBADF));
}

#[test]
fn permissions_and_handle_limits() {
    let mut m = model();
    assert_eq!(
        m.open(Node::Log(0), libc::O_TRUNC | APPEND),
        Err(libc::EPERM)
    );
    assert_eq!(m.open(Node::Log(0), libc::O_WRONLY), Err(libc::EPERM));
    assert_eq!(
        m.open(Node::Log(0), libc::O_RDWR | libc::O_APPEND),
        Err(libc::EOPNOTSUPP)
    );
    assert_eq!(m.open(Node::Status, APPEND), Err(libc::EACCES));
    let w = m.open(Node::Log(0), APPEND).unwrap();
    assert_eq!(
        m.write(Node::Log(0), w, b"x\n", libc::O_WRONLY),
        Err(libc::EPERM)
    );
    assert_eq!(m.write(Node::Status, w, b"x\n", APPEND), Err(libc::EBADF));
    for _ in 1..MAX_HANDLES {
        m.open(Node::Log(0), libc::O_RDONLY).unwrap();
    }
    assert_eq!(m.open(Node::Log(0), libc::O_RDONLY), Err(libc::EMFILE));
    m.release(Node::Log(0), w).unwrap();
    m.open(Node::Log(0), libc::O_RDONLY).unwrap();
}
