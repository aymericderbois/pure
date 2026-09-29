use std::{
    env, io,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};

use pure_tui::app::{App, DocumentFormat, open_document};
use pure_tui::config::Config;
use tdoc::Document;

fn main() -> Result<()> {
    run()
}

fn run() -> Result<()> {
    // Without an argument, start with an untitled document; saving it asks
    // for a name through the Save As dialog.
    let path = env::args().nth(1).map(PathBuf::from);
    let (document, format, initial_status, disk) = match &path {
        Some(path) => {
            let loaded = open_document(path)?;
            (loaded.document, loaded.format, loaded.message, loaded.disk)
        }
        None => (
            Document::new(),
            DocumentFormat::Ftml,
            Some("New document".to_string()),
            None,
        ),
    };
    let mut app = App::new(document, path, format, initial_status);
    app.set_config(Config::load());
    // Watch the file for changes made by other programs.
    app.set_disk_baseline(disk);

    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .context("failed to initialize terminal")?;
    // Ask the terminal (when it supports the Kitty keyboard protocol) to disambiguate
    // modified keys, so Shift+Enter / Ctrl+Enter arrive as distinct events (an ordinary
    // terminal reports them all as plain Enter). Popped on exit.
    let keyboard_enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    if keyboard_enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .ok();
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;
    terminal.clear().ok();

    let res = run_app(&mut terminal, &mut app).context("application error");

    disable_raw_mode().ok();
    if keyboard_enhanced {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags).ok();
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        SetCursorStyle::DefaultUserShape
    )
    .ok();
    terminal.show_cursor().ok();

    res
}

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();
    let mut needs_redraw = true;

    while !app.should_quit() {
        // Only draw if needed
        if needs_redraw {
            terminal
                .draw(|frame| app.draw(frame))
                .context("failed to draw frame")?;
            needs_redraw = false;
        }

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        // Block waiting for events
        if event::poll(timeout).context("event poll failed")? {
            let evt = event::read().context("failed to read event")?;

            // Skip spurious Resize events that don't change size
            if let Event::Resize(_, _) = evt {
                // Always redraw on resize to handle terminal size changes
                needs_redraw = true;
                continue;
            }

            app.handle_event(evt)?;

            // Mark that we need to redraw after handling event
            needs_redraw = true;
        }

        // Tick: expire status messages; redraw only when something changed.
        if last_tick.elapsed() >= tick_rate {
            if app.on_tick() {
                needs_redraw = true;
            }
            last_tick = Instant::now();
        }
    }

    Ok(())
}
