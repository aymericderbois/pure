//! Reloading the open file when another program changes it on disk.
//!
//! Changes are made deterministic by writing the file and then pinning its
//! mtime to a distinct value, and the settle delay counts ticks rather than
//! wall-clock time, so no test sleeps.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use ratatui::{Terminal, backend::TestBackend};

use super::*;

/// An app watching a temp file; the file is removed on drop.
struct Watched {
    app: App,
    path: PathBuf,
}

impl Drop for Watched {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn temp_path(ext: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("pure-reload-{}-{id}.{ext}", std::process::id()))
}

/// Open `content` from a fresh temp file the way the binary does.
fn watched(ext: &str, content: &str) -> Watched {
    let path = temp_path(ext);
    external_write(&path, content);
    let loaded = open_document(&path).unwrap();
    let mut app = App::new(loaded.document, Some(path.clone()), loaded.format, None);
    app.set_interactive(false);
    app.set_disk_baseline(loaded.disk);
    Watched { app, path }
}

/// Rewrite the file as another program would, then pin a never-used mtime so
/// the change shows even where timestamps are coarse (or fixed, as in the Nix
/// store).
fn external_write(path: &Path, content: impl AsRef<[u8]>) {
    static NEXT_MTIME: AtomicU64 = AtomicU64::new(1_000);
    fs::write(path, content).unwrap();
    let secs = NEXT_MTIME.fetch_add(1, Ordering::Relaxed);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

fn md(text: &str) -> Document {
    parse_document(text.to_string(), DocumentFormat::Markdown).unwrap()
}

fn document(app: &App) -> &Document {
    app.display.editor().document()
}

fn status(app: &App) -> &str {
    app.status_message
        .as_ref()
        .map(|(message, _)| message.as_str())
        .unwrap_or("")
}

/// Two ticks: the first notices the change, the second finds it settled.
fn settle(app: &mut App) -> bool {
    assert!(!app.on_tick(), "a fresh change waits one tick to settle");
    app.on_tick()
}

/// Draw once so the editor knows its viewport (scrolling needs it).
fn draw(app: &mut App) {
    let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
}

/// Permission tests can't run as root, who reads any file.
#[cfg(unix)]
fn permissions_apply(path: &Path) -> bool {
    fs::read(path).is_err()
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn clean_document_reloads_when_the_file_changes() {
    let mut w = watched("md", "# Title\n\nHello\n");
    external_write(&w.path, "# Title\n\nWorld\n");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("# Title\n\nWorld\n"));
    assert!(!w.app.dirty);
    assert_eq!(
        status(&w.app),
        "File changed on disk — reloaded (Ctrl+Z to undo)"
    );
    assert!(!w.app.on_tick(), "the new version is the baseline now");
}

#[test]
fn first_poll_after_a_change_does_nothing() {
    let mut w = watched("md", "Hello\n");
    external_write(&w.path, "World\n");
    assert!(!w.app.on_tick());
    assert_eq!(*document(&w.app), md("Hello\n"));
}

#[test]
fn file_still_changing_is_read_once_it_settles() {
    let mut w = watched("md", "One\n");
    external_write(&w.path, "Two\n");
    assert!(!w.app.on_tick());
    external_write(&w.path, "Three\n");
    assert!(!w.app.on_tick(), "changed again: wait another tick");
    assert_eq!(*document(&w.app), md("One\n"));
    assert!(w.app.on_tick());
    assert_eq!(*document(&w.app), md("Three\n"));
}

#[test]
fn reload_is_one_undo_step_and_keeps_the_caret() {
    let mut w = watched("md", "First\n\nSecond\n\nThird\n");
    w.app
        .display
        .editor_mut()
        .set_cursor(DocumentPosition::new(2, 3));
    external_write(&w.path, "First!\n\nSecond\n\nThird\n");
    assert!(settle(&mut w.app));
    assert_eq!(w.app.display.editor().cursor(), DocumentPosition::new(2, 3));
    w.app.undo();
    assert_eq!(*document(&w.app), md("First\n\nSecond\n\nThird\n"));
    assert_eq!(w.app.display.editor().cursor(), DocumentPosition::new(2, 3));
    assert!(w.app.dirty, "undoing the reload diverges from the disk");
    assert!(!w.app.display.editor_mut().undo(), "a single undo step");
}

#[test]
fn manual_scroll_is_clamped_when_the_document_shrinks() {
    let long: String = (0..60).map(|i| format!("Line {i}\n\n")).collect();
    let mut w = watched("md", &long);
    draw(&mut w.app);
    w.app.scroll_by(1_000);
    assert!(w.app.display.scroll_offset() > 0);
    external_write(&w.path, "Short\n");
    assert!(settle(&mut w.app));
    assert_eq!(w.app.display.scroll_offset(), 0);
}

#[test]
fn unsaved_changes_are_kept_and_warned_once() {
    let mut w = watched("md", "Hello\n");
    w.app.insert_char('x');
    w.app.after_edit(UndoKind::Other);
    let edited = document(&w.app).clone();
    external_write(&w.path, "World\n");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), edited);
    assert!(w.app.dirty);
    assert_eq!(
        status(&w.app),
        "File changed on disk — keeping your unsaved changes"
    );
    assert!(!w.app.on_tick(), "warned once per version, not every tick");
    assert!(!w.app.on_tick());
}

#[test]
fn touch_is_ignored_even_with_unsaved_changes() {
    for edited in [false, true] {
        let mut w = watched("md", "Hello\n");
        if edited {
            w.app.insert_char('x');
            w.app.after_edit(UndoKind::Other);
        }
        external_write(&w.path, "Hello\n");
        assert!(!settle(&mut w.app));
        assert_eq!(status(&w.app), "");
        assert_eq!(w.app.display.editor().can_undo(), edited);
    }
}

/// Pure's writers don't always round-trip (a blank line ending a code block
/// is lost on reload), so comparing parsed documents would reload after our
/// own save. The byte hash doesn't.
#[test]
fn own_save_then_touch_is_not_reloaded() {
    let source = "```\ncode\n\n```\n";
    let mut w = watched("md", source);
    w.app.save().unwrap();
    let saved = fs::read(&w.path).unwrap();
    assert_ne!(md(std::str::from_utf8(&saved).unwrap()), md(source));
    assert!(!w.app.on_tick());
    assert!(!w.app.on_tick());
    external_write(&w.path, &saved);
    assert!(!settle(&mut w.app));
    assert!(!w.app.display.editor().can_undo(), "no reload step");
    assert!(status(&w.app).starts_with("Saved"), "{}", status(&w.app));
}

#[test]
fn deleted_file_keeps_the_document_until_it_comes_back() {
    let mut w = watched("md", "Hello\n");
    fs::remove_file(&w.path).unwrap();
    for _ in 0..3 {
        assert!(!w.app.on_tick());
    }
    assert_eq!(*document(&w.app), md("Hello\n"));
    external_write(&w.path, "Hello\n");
    assert!(!settle(&mut w.app), "back unchanged: nothing to do");
    external_write(&w.path, "Back\n");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("Back\n"));
}

#[test]
fn unreadable_or_unparsable_file_keeps_the_document() {
    let mut w = watched("ftml", "<p>Hello</p>\n");
    let before = document(&w.app).clone();
    external_write(&w.path, "<p>Hello <b>world</p>\n");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), before);
    assert!(
        status(&w.app).starts_with("File changed on disk but can't be read"),
        "{}",
        status(&w.app)
    );
    assert!(!w.app.on_tick(), "the broken version is not re-read");

    external_write(&w.path, [0xff, 0xfe]);
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), before);
    assert!(status(&w.app).starts_with("File changed on disk but can't be read"));
}

#[test]
fn emptied_file_is_not_loaded_over_the_document() {
    let mut w = watched("md", "Hello\n");
    external_write(&w.path, "");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("Hello\n"));
    assert_eq!(status(&w.app), "File on disk is now empty — text kept");
}

#[test]
fn reload_waits_for_menus_and_dialogs_to_close() {
    let mut w = watched("md", "Hello\n");
    w.app.menu_bar = Some(MenuBarState::new());
    external_write(&w.path, "World\n");
    for _ in 0..4 {
        assert!(!w.app.on_tick());
    }
    assert_eq!(*document(&w.app), md("Hello\n"));
    w.app.menu_bar = None;
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("World\n"));
}

#[test]
fn auto_reload_can_be_turned_off() {
    let mut w = watched("md", "Hello\n");
    w.app.set_config(Config {
        auto_reload: false,
        ..Config::default()
    });
    external_write(&w.path, "World\n");
    for _ in 0..3 {
        assert!(!w.app.on_tick());
    }
    assert_eq!(*document(&w.app), md("Hello\n"));
}

#[test]
fn opening_another_file_moves_the_watch() {
    let mut w = watched("md", "Hello\n");
    let other = temp_path("md");
    external_write(&other, "Other\n");
    w.app.open_file(other.clone());
    external_write(&w.path, "Old file changed\n");
    assert!(!w.app.on_tick());
    assert!(!w.app.on_tick());
    assert_eq!(*document(&w.app), md("Other\n"));
    external_write(&other, "Other changed\n");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("Other changed\n"));
    let _ = fs::remove_file(&other);
}

#[test]
fn new_document_is_not_watched() {
    let mut w = watched("md", "Hello\n");
    w.app.new_document();
    external_write(&w.path, "World\n");
    assert!(!w.app.on_tick());
    assert!(!w.app.on_tick());
    assert!(document(&w.app).is_empty());
}

#[test]
fn markdown_file_reloads_as_markdown() {
    let mut w = watched("md", "Hello\n");
    external_write(&w.path, "# Heading\n\n- item\n");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("# Heading\n\n- item\n"));
    assert_eq!(w.app.document_format, DocumentFormat::Markdown);
}

#[cfg(unix)]
#[test]
fn transient_read_error_is_retried() {
    let mut w = watched("md", "Hello\n");
    external_write(&w.path, "World\n");
    chmod(&w.path, 0o000);
    if !permissions_apply(&w.path) {
        chmod(&w.path, 0o644);
        eprintln!("running as root, skipping");
        return;
    }
    assert!(settle(&mut w.app));
    assert!(
        status(&w.app).starts_with("File changed on disk but can't be read"),
        "{}",
        status(&w.app)
    );
    assert!(!w.app.on_tick(), "reported once");
    chmod(&w.path, 0o644);
    // chmod moves the ctime, so the stamp changes: settle again.
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("World\n"));
}

#[cfg(unix)]
#[test]
fn failed_save_keeps_watching() {
    let mut w = watched("md", "Hello\n");
    chmod(&w.path, 0o444);
    if fs::OpenOptions::new().write(true).open(&w.path).is_ok() {
        chmod(&w.path, 0o644);
        eprintln!("running as root, skipping");
        return;
    }
    w.app.save().unwrap();
    assert!(
        status(&w.app).starts_with("Save failed"),
        "{}",
        status(&w.app)
    );
    chmod(&w.path, 0o644);
    external_write(&w.path, "World\n");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("World\n"));
}

#[test]
fn save_as_to_another_file_watches_the_new_one() {
    let mut w = watched("md", "Hello\n");
    let other = temp_path("md");
    w.app.save_as(other.clone());
    external_write(&w.path, "Old file changed\n");
    assert!(!w.app.on_tick());
    assert!(!w.app.on_tick());
    assert_eq!(*document(&w.app), md("Hello\n"));
    external_write(&other, "New file changed\n");
    assert!(settle(&mut w.app));
    assert_eq!(*document(&w.app), md("New file changed\n"));
    let _ = fs::remove_file(&other);
}
