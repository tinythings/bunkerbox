use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::cursor;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};
use ratatui::Terminal;

static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_: libc::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

const WIDGET_PROGRESS: &str = "progress";
const WIDGET_STATUS: &str = "status";
const WIDGET_POPUP: &str = "popup";

const CMD_SHOW: &str = "show";
const CMD_HIDE: &str = "hide";
const CMD_SET: &str = "set";
const CMD_CLEAR: &str = "clear";

pub struct OverlayState {
    pub status_text: String,
    pub progress_percent: Option<f64>,
    pub progress_label: Option<String>,
    pub popup_text: Option<String>,
}

impl OverlayState {
    pub fn new(status_text: String) -> Self {
        Self { status_text, progress_percent: None, progress_label: None, popup_text: None }
    }
}

pub fn dispatch_ui_command(state: &mut OverlayState, widget: &str, command: &str, value: &str) {
    match (widget, command) {
        (WIDGET_PROGRESS, CMD_SET) => {
            if let Ok(pct) = value.parse::<f64>() {
                state.progress_percent = Some(pct.clamp(0.0, 1.0));
            }
        }
        (WIDGET_PROGRESS, CMD_SHOW) => {
            if let Some((pct, label)) = value.split_once(';') {
                if let Ok(pct) = pct.trim().parse::<f64>() {
                    state.progress_percent = Some(pct.clamp(0.0, 1.0));
                }
                state.progress_label = Some(label.trim().to_string());
            } else if let Ok(pct) = value.parse::<f64>() {
                state.progress_percent = Some(pct.clamp(0.0, 1.0));
            }
        }
        (WIDGET_PROGRESS, CMD_HIDE) => {
            state.progress_percent = None;
            state.progress_label = None;
        }
        (WIDGET_STATUS, CMD_SET) => {
            state.status_text = value.to_string();
        }
        (WIDGET_STATUS, CMD_CLEAR) => {
            state.status_text.clear();
        }
        (WIDGET_POPUP, CMD_SHOW) => {
            state.popup_text = Some(value.to_string());
        }
        (WIDGET_POPUP, CMD_HIDE) => {
            state.popup_text = None;
        }
        _ => {}
    }
}

/// Terminal emulator wrapper around [`vt100::Parser`] with DEC Special Graphics
/// character set translation and HVP-to-CUP normalization.
struct Term {
    parser: vt100::Parser,
    escape_state: EscapeState,
    g0_dec_special_graphics: bool,
    g1_dec_special_graphics: bool,
    using_g1_charset: bool,
}

#[derive(Clone, Copy)]
enum EscapeState {
    Ground,
    Escape,
    CharsetSelect(u8),
    Csi,
    String,
    StringEscape,
}

impl Term {
    /// Creates a new terminal of the given rows and columns.
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            escape_state: EscapeState::Ground,
            g0_dec_special_graphics: false,
            g1_dec_special_graphics: false,
            using_g1_charset: false,
        }
    }

    /// Returns a reference to the vt100 screen grid for rendering.
    fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Resizes the terminal grid after a window resize event.
    fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(rows, cols);
    }

    /// Feeds raw bytes through DEC translation then into the vt100 parser.
    fn process(&mut self, bytes: &[u8]) {
        let translated = self.translate_dec_special_graphics(bytes);
        self.parser.process(&translated);
    }

    fn active_dec_special_graphics(&self) -> bool {
        if self.using_g1_charset {
            self.g1_dec_special_graphics
        } else {
            self.g0_dec_special_graphics
        }
    }

    /// Translates `\e(0` DEC Special Graphics characters to Unicode
    /// box-drawing glyphs and normalizes HVP (`CSI … f`) to CUP (`CSI … H`).
    fn translate_dec_special_graphics(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut translated = Vec::with_capacity(bytes.len());

        for &byte in bytes {
            match self.escape_state {
                EscapeState::Ground => match byte {
                    0x1b => {
                        self.escape_state = EscapeState::Escape;
                    }
                    0x0e => {
                        self.using_g1_charset = true;
                    }
                    0x0f => {
                        self.using_g1_charset = false;
                    }
                    0x20..=0x7e if self.active_dec_special_graphics() => {
                        push_dec_special_graphic(&mut translated, byte);
                    }
                    _ => {
                        translated.push(byte);
                    }
                },
                EscapeState::Escape => match byte {
                    b'(' | b')' => {
                        self.escape_state = EscapeState::CharsetSelect(byte);
                    }
                    b'[' => {
                        self.escape_state = EscapeState::Csi;
                        translated.push(0x1b);
                        translated.push(byte);
                    }
                    b']' | b'P' | b'^' | b'_' => {
                        self.escape_state = EscapeState::String;
                        translated.push(0x1b);
                        translated.push(byte);
                    }
                    _ => {
                        self.escape_state = EscapeState::Ground;
                        translated.push(0x1b);
                        translated.push(byte);
                    }
                },
                EscapeState::CharsetSelect(charset) => {
                    match charset {
                        b'(' => self.g0_dec_special_graphics = byte == b'0',
                        b')' => self.g1_dec_special_graphics = byte == b'0',
                        _ => {}
                    }
                    self.escape_state = EscapeState::Ground;
                }
                EscapeState::Csi => {
                    if byte == b'f' {
                        translated.push(b'H');
                    } else {
                        translated.push(byte);
                    }
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape_state = EscapeState::Ground;
                    }
                }
                EscapeState::String => {
                    translated.push(byte);
                    match byte {
                        0x07 => self.escape_state = EscapeState::Ground,
                        0x1b => self.escape_state = EscapeState::StringEscape,
                        _ => {}
                    }
                }
                EscapeState::StringEscape => {
                    translated.push(byte);
                    self.escape_state = if byte == b'\\' { EscapeState::Ground } else { EscapeState::String };
                }
            }
        }

        translated
    }
}

/// Converts a single byte from the DEC Special Graphics table to its Unicode
/// equivalent (e.g. `x` → `│`, `q` → `─`). Appends the UTF-8 bytes to `out`.
fn push_dec_special_graphic(out: &mut Vec<u8>, byte: u8) {
    let mapped = match byte {
        b'`' => '◆',
        b'a' => '▒',
        b'b' => '␉',
        b'c' => '␌',
        b'd' => '␍',
        b'e' => '␊',
        b'f' => '°',
        b'g' => '±',
        b'h' => '␤',
        b'i' => '␋',
        b'j' => '┘',
        b'k' => '┐',
        b'l' => '┌',
        b'm' => '└',
        b'n' => '┼',
        b'o' => '⎺',
        b'p' => '⎻',
        b'q' => '─',
        b'r' => '⎼',
        b's' => '⎽',
        b't' => '├',
        b'u' => '┤',
        b'v' => '┴',
        b'w' => '┬',
        b'x' => '│',
        b'y' => '≤',
        b'z' => '≥',
        b'{' => 'π',
        b'|' => '≠',
        b'}' => '£',
        b'~' => '·',
        _ => {
            out.push(byte);
            return;
        }
    };
    let mut buf = [0u8; 4];
    out.extend_from_slice(mapped.encode_utf8(&mut buf).as_bytes());
}

/// Renders PTY output through ratatui until the child process exits.
///
/// Forwards real keystrokes to the PTY master, parses terminal output through
/// [`vt100::Parser`], and draws each frame with full 24-bit color plus a
/// floating status overlay in the top-right corner.
///
/// `status_fd` is polled continuously for newline-delimited messages. The
/// first message is forwarded to `on_setup` (workspace path); subsequent
/// messages update `overlay.status_text`.
///
/// `overlay` is shared with the VSOCK status listener so VM-originated
/// UI commands can update popups, progress bars, and status text.
pub fn event_loop<F>(master_fd: RawFd, rows: u16, cols: u16, status_fd: RawFd, on_setup: F, overlay: Arc<Mutex<OverlayState>>) -> Result<(), String>
where
    F: FnOnce(Vec<u8>) -> Result<(), String>,
{
    let stdin_fd = io::stdin().as_raw_fd();

    let mut stdout = io::stdout();
    terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    stdout.execute(EnterAlternateScreen).map_err(|e| format!("alt screen: {e}"))?;
    stdout.execute(cursor::Show).map_err(|e| format!("cursor: {e}"))?;

    let mut term = Term::new(rows, cols);

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("terminal: {e}"))?;

    let mut pty_buf = [0u8; 4096];
    let mut stdin_buf = [0u8; 1];

    let mut last_rows = rows;
    let mut last_cols = cols;

    let mut on_setup = Some(on_setup);
    let mut status_buf = Vec::new();

    unsafe {
        libc::signal(libc::SIGWINCH, handle_sigwinch as *const () as libc::sighandler_t);
    }

    loop {
        if RESIZED.swap(false, Ordering::SeqCst) {
            if let Ok((new_cols, new_rows)) = terminal::size() {
                if new_cols != last_cols || new_rows != last_rows {
                    last_cols = new_cols;
                    last_rows = new_rows;
                    term.set_size(new_rows, new_cols);
                    let ws = libc::winsize { ws_row: new_rows, ws_col: new_cols, ws_xpixel: 0, ws_ypixel: 0 };
                    unsafe {
                        libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws);
                    }
                }
            }
        }

        let mut fds = [
            libc::pollfd { fd: master_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stdin_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: status_fd, events: libc::POLLIN, revents: 0 },
        ];

        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 3, 16) };

        if ret == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            terminal.backend_mut().execute(LeaveAlternateScreen).ok();
            terminal::disable_raw_mode().ok();
            return Err(format!("poll: {err}"));
        }

        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = unsafe { libc::read(master_fd, pty_buf.as_mut_ptr() as *mut libc::c_void, pty_buf.len()) };
            if n > 0 {
                term.process(&pty_buf[..n as usize]);
            } else {
                break;
            }
        }

        if fds[1].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(stdin_fd, stdin_buf.as_mut_ptr() as *mut libc::c_void, 1usize) };
            if n > 0 {
                unsafe {
                    libc::write(master_fd, stdin_buf.as_ptr() as *const libc::c_void, 1usize);
                }
            }
        }

        if fds[2].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut chunk = [0u8; 256];
            let n = unsafe { libc::read(status_fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
            if n > 0 {
                status_buf.extend_from_slice(&chunk[..n as usize]);
            }
            while let Some(pos) = status_buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&status_buf[..pos]).into_owned();
                status_buf.drain(..=pos);
                if let Some(cb) = on_setup.take() {
                    cb(line.into_bytes())?;
                } else {
                    let mut state = overlay.lock().unwrap();
                    state.status_text = line;
                }
            }
        }

        {
            let state = overlay.lock().unwrap();
            terminal.draw(|f| render_frame(f, term.screen(), &state)).map_err(|e| format!("draw: {e}"))?;
        }
    }

    terminal.backend_mut().execute(LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    unsafe {
        libc::signal(libc::SIGWINCH, libc::SIG_DFL);
    }

    Ok(())
}

/// Converts a [`vt100::Color`] to a [`ratatui::style::Color`], preserving
/// 24-bit RGB, 256-color indexed palette, and terminal default.
fn to_ratatui_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Renders one frame: writes every vt100 screen cell to the ratatui buffer
/// with full color and attributes, then draws overlay widgets (popup,
/// progress bar, status box) on top.
fn render_frame(f: &mut Frame, screen: &vt100::Screen, overlay: &OverlayState) {
    let area = f.area();
    let (rows, cols) = screen.size();
    let max_rows = area.height.min(rows);
    let max_cols = area.width.min(cols);
    let buf = f.buffer_mut();

    for row in 0..max_rows {
        let mut col: u16 = 0;
        while col < max_cols {
            let x = area.x + col;
            let y = area.y + row;

            if let Some(cell) = screen.cell(row, col) {
                if cell.is_wide_continuation() {
                    col += 1;
                    continue;
                }

                let mut style = Style::default().fg(to_ratatui_color(cell.fgcolor())).bg(to_ratatui_color(cell.bgcolor()));

                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                let ch = cell.contents();
                let display: &str = if ch.is_empty() { " " } else { &ch };

                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_symbol(display);
                    c.set_style(style);
                }

                if cell.is_wide() {
                    if col + 1 < max_cols {
                        if let Some(c) = buf.cell_mut((x + 1, y)) {
                            c.set_symbol(" ");
                            c.set_style(style);
                        }
                    }
                    col += 2;
                } else {
                    col += 1;
                }
            } else {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_symbol(" ");
                    c.set_style(Style::default());
                }
                col += 1;
            }
        }
    }

    if let Some(ref text) = overlay.popup_text {
        let popup_w = (text.len() as u16 + 4).min(area.width.saturating_sub(4));
        let popup_h = 3u16;
        let popup_x = (area.width.saturating_sub(popup_w)) / 2;
        let popup_y = (area.height.saturating_sub(popup_h)) / 2;
        let rect = Rect::new(popup_x, popup_y, popup_w, popup_h);
        f.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(format!(" {} ", text))
            .title_style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));
        f.render_widget(block, rect);
    }

    if let Some(pct) = overlay.progress_percent {
        let bar_w = (area.width.saturating_sub(8)).min(60);
        let bar_x = (area.width.saturating_sub(bar_w)) / 2;
        let bar_y = area.height.saturating_sub(4);
        let rect = Rect::new(bar_x, bar_y, bar_w, 3u16);
        f.render_widget(Clear, rect);

        let pct_label = format!(" {:.0}% ", pct * 100.0);
        let label = overlay.progress_label.as_deref().unwrap_or("");
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!(" {label} {pct_label}"))
            .title_style(Style::default().fg(Color::White));
        f.render_widget(block, rect);

        let gauge =
            ratatui::widgets::Gauge::default().gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray)).percent((pct * 100.0) as u16);
        let inner = Rect::new(bar_x + 1, bar_y + 1, bar_w.saturating_sub(2), 1);
        f.render_widget(gauge, inner);
    }

    {
        let title = format!(" {} ", overlay.status_text);
        let win_w = (title.len() as u16).max(15);
        let win_h = 3u16;
        let win_x = area.width.saturating_sub(win_w + 4);
        let win_y = 2;
        let rect = Rect::new(win_x, win_y, win_w, win_h);

        f.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title)
            .title_style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));
        f.render_widget(block, rect);
    }

    let (cursor_row, cursor_col) = screen.cursor_position();
    if cursor_row < max_rows && cursor_col < max_cols {
        f.set_cursor_position((area.x + cursor_col, area.y + cursor_row));
    }
}
