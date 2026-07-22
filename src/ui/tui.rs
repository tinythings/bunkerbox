use std::io::{self, Write};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::style::{Color, Print, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn show() -> io::Result<()> {
    execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), Hide)?;
    draw_splash()?;
    Ok(())
}

pub fn progress(step: u32, total: u32, msg: &str) -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let center_y = rows / 2;

    execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    center_text(cols, center_y - 2, "bunkerbox", true)?;
    center_text(cols, center_y, msg, false)?;

    let bar_width = 40u16;
    let filled = (step as u16).checked_mul(bar_width).and_then(|v| v.checked_div(total as u16)).unwrap_or(0);
    let bar_x = cols.saturating_sub(bar_width + 2) / 2;

    queue!(io::stdout(), MoveTo(bar_x, center_y + 1), Print("["))?;
    for i in 0..bar_width {
        if i < filled {
            execute!(io::stdout(), SetForegroundColor(Color::Green), Print("█"), SetForegroundColor(Color::Reset))?;
        } else {
            execute!(io::stdout(), Print("░"))?;
        }
    }
    let pct = step.checked_mul(100).and_then(|v| v.checked_div(total)).unwrap_or(0);
    queue!(io::stdout(), Print("]"), Print(format!(" {}%", pct)))?;
    io::stdout().flush()?;
    Ok(())
}

pub fn spinner(text: &str) -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let center_y = rows / 2;
    let start = Instant::now();
    let mut frame = 0usize;

    while start.elapsed() < Duration::from_secs(2) {
        if event::poll(Duration::from_millis(50)).is_ok_and(|b| b) {
            let _ = event::read();
        }

        execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
        center_text(cols, center_y - 1, "bunkerbox", true)?;
        let line = format!("{} {}", SPINNER_FRAMES[frame], text);
        center_text(cols, center_y + 1, &line, false)?;

        frame = (frame + 1) % SPINNER_FRAMES.len();
        std::thread::sleep(Duration::from_millis(80));
    }
    Ok(())
}

pub fn prompt_password(msg: &str) -> Option<String> {
    let (cols, rows) = terminal::size().ok()?;
    let center_y = rows / 2;
    let mut input = String::new();

    loop {
        execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0)).ok()?;
        center_text(cols, center_y - 1, "bunkerbox", true).ok()?;
        center_text(cols, center_y + 1, msg, false).ok()?;

        let masked: String = input.chars().map(|_| '*').collect();
        let display = format!("{} ", masked);
        let x = cols.saturating_sub(display.len() as u16) / 2;
        queue!(io::stdout(), MoveTo(x, center_y + 2), Print(&display)).ok()?;
        io::stdout().flush().ok()?;

        if let Ok(true) = event::poll(Duration::from_millis(200)) {
            if let Ok(Event::Key(key)) = event::read() {
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                    match key.code {
                        KeyCode::Enter => return Some(input),
                        KeyCode::Esc => return None,
                        KeyCode::Backspace => { input.pop(); }
                        KeyCode::Char(c) => input.push(c),
                        _ => {}
                    }
                }
            }
        }
    }
}

pub fn hide() -> io::Result<()> {
    execute!(io::stdout(), Show, Clear(ClearType::All), MoveTo(0, 0))?;
    execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()
}

fn draw_splash() -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let center_y = rows / 2;
    execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
    center_text(cols, center_y, "bunkerbox", true)?;
    center_text(cols, center_y + 1, &format!("v{}", env!("CARGO_PKG_VERSION")), false)?;
    center_text(cols, center_y + 3, "Starting...", false)?;
    Ok(())
}

fn center_text(cols: u16, row: u16, text: &str, bold: bool) -> io::Result<()> {
    let x = cols.saturating_sub(text.len() as u16) / 2;
    if bold {
        queue!(io::stdout(), SetForegroundColor(Color::Yellow))?;
    }
    queue!(io::stdout(), MoveTo(x, row), Print(text))?;
    if bold {
        queue!(io::stdout(), SetForegroundColor(Color::Reset))?;
    }
    io::stdout().flush()?;
    Ok(())
}
