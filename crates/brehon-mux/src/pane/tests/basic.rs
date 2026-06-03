use crate::pane::{Pane, PaneKind};

#[test]
fn test_pane_kind_as_str() {
    assert_eq!(PaneKind::Worker.as_str(), "worker");
    assert_eq!(PaneKind::Supervisor.as_str(), "supervisor");
    assert_eq!(PaneKind::Director.as_str(), "director");
    assert_eq!(PaneKind::Shell.as_str(), "shell");
    assert_eq!(PaneKind::Reviewer.as_str(), "reviewer");
    assert_eq!(PaneKind::Advisor.as_str(), "advisor");
}

#[test]
fn test_director_pane() {
    let pane = Pane::director("test-director", 24, 80).expect("create pane");
    assert_eq!(pane.id(), "test-director");
    assert_eq!(pane.kind(), &PaneKind::Director);
    assert_eq!(pane.title(), "Director");
    assert!(!pane.has_exited());
}

#[test]
fn test_pane_feed_and_dump() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    pane.feed(b"Hello, World!").expect("feed");
    let content = pane.dump_viewport().expect("dump viewport");
    assert!(content.contains("Hello, World!"));
}

#[test]
fn test_pane_ansi_colors() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    pane.feed(b"\x1b[31mRed\x1b[0m Normal").expect("feed");
    let content = pane.dump_viewport().expect("dump viewport");
    assert!(content.contains("Red"));
    assert!(content.contains("Normal"));
}

#[test]
fn test_pane_scroll() {
    let mut pane = Pane::director("test", 5, 80).expect("create pane");

    for i in 0..20 {
        pane.feed(format!("Line {i}\r\n").as_bytes())
            .expect("feed line");
    }

    assert!(pane.scroll(-5).is_ok());
    assert!(pane.scroll(3).is_ok());
    assert!(pane.scroll_to_top().is_ok());
    assert!(pane.scroll_to_bottom().is_ok());
}

#[test]
fn test_scrollback_info() {
    let pane = Pane::director("test", 24, 80).expect("create pane");
    let info = pane.scrollback_info();

    assert_eq!(info.viewport_offset, 0);
    assert_eq!(info.viewport_rows, 24);
    assert!(info.total_scrollback >= info.viewport_rows as u32);
}

#[test]
fn test_get_full_snapshot() {
    let mut pane = Pane::director("test", 5, 10).expect("create pane");
    for i in 0..5 {
        pane.feed(format!("Line {i}\r\n").as_bytes())
            .expect("feed line");
    }

    let snapshot = pane.get_full_snapshot().expect("snapshot");
    assert_eq!(snapshot.rows, 5);
    assert_eq!(snapshot.cols, 10);
    assert_eq!(snapshot.cells.len(), 50);
}

#[test]
fn test_create_snapshot_no_cache() {
    let mut pane = Pane::director("test", 5, 10).expect("create pane");
    for i in 0..5 {
        pane.feed(format!("Row {i}\r\n").as_bytes())
            .expect("feed row");
    }

    let (snapshot, cache_rows, cache_start) = pane
        .create_snapshot_with_cache(0)
        .expect("snapshot with cache");
    assert_eq!(snapshot.rows, 5);
    assert!(cache_rows.is_empty());
    assert!(cache_start.is_none());
}

#[test]
fn test_create_snapshot_with_cache() {
    let mut pane = Pane::director("test", 5, 80).expect("create pane");

    for i in 0..30 {
        pane.feed(format!("Line {i}\r\n").as_bytes())
            .expect("feed line");
    }

    pane.scroll(-10).expect("scroll up");

    let (snapshot, cache_rows, cache_start) = pane
        .create_snapshot_with_cache(20)
        .expect("snapshot with cache");
    assert_eq!(snapshot.rows, 5);
    assert!(cache_start.is_some());

    for row in &cache_rows {
        assert!(row.screen_row < pane.scrollback_lines());
    }
}
