use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::*;

use ratatui::Terminal;

use crate::vscomm::{self, parse_triggers, Trigger};

mod palette;
mod popup;
use popup::PopupWidget;

static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_: libc::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

const WIDGET_PROGRESS: &str = "progress";
const WIDGET_STATUS: &str = "status";
const WIDGET_POPUP: &str = "popup";
const WIDGET_SPINNER: &str = "spinner";
const WIDGET_PASSWORD: &str = "password";

const CMD_SHOW: &str = "show";
const CMD_HIDE: &str = "hide";
const CMD_SET: &str = "set";
const CMD_CLEAR: &str = "clear";

pub struct PendingAction {
    pub widget: String,
    pub command: String,
    pub triggers: Vec<Trigger>,
    pub value: String,
    pub enqueued_at: Instant,
    pub first_pty_at: Option<Instant>,
}

pub struct OverlayState {
    pub status_text: String,
    pub popup: PopupWidget,
    pub popup_title: Option<String>,
    pub pending: Vec<PendingAction>,
    pub has_error: bool,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayState {
    pub fn new() -> Self {
        Self { status_text: String::new(), popup: PopupWidget::new(), popup_title: None, pending: Vec::new(), has_error: false }
    }
}

pub fn dispatch_ui_command(state: &mut OverlayState, widget: &str, command: &str, options: &str, value: &str) {
    let triggers = parse_triggers(options);
    if !triggers.is_empty() {
        state.pending.push(PendingAction {
            widget: widget.to_string(),
            command: command.to_string(),
            triggers,
            value: value.to_string(),
            enqueued_at: Instant::now(),
            first_pty_at: None,
        });
        return;
    }

    match (widget, command) {
        (WIDGET_PROGRESS, CMD_SET) => {
            if let Ok(pct) = value.parse::<f64>() {
                state.popup.set_progress(pct.clamp(0.0, 1.0), None);
            }
        }
        (WIDGET_PROGRESS, CMD_SHOW) => {
            if let Some((pct, label)) = value.split_once(';') {
                let pct: f64 = pct.trim().parse().unwrap_or(0.0);
                state.popup.show_progress("", pct.clamp(0.0, 1.0), Some(label.trim().to_string()));
            } else if let Ok(pct) = value.parse::<f64>() {
                state.popup.show_progress("", pct.clamp(0.0, 1.0), None);
            }
        }
        (WIDGET_PROGRESS, CMD_HIDE) => {
            state.popup.hide();
        }
        (WIDGET_STATUS, "phase") => {
            state.popup_title = if value.is_empty() { None } else { Some(value.to_string()) };
        }
        (WIDGET_STATUS, CMD_SET) => {
            let title = state.popup_title.clone();
            state.popup.show_info(title, value, Some(palette::FG), Some(palette::ACCENT));
        }
        (WIDGET_STATUS, CMD_CLEAR) => {
            state.popup.hide();
            state.popup_title = None;
        }
        (WIDGET_POPUP, CMD_SHOW) => {
            state.popup.show_info(None, value, None, None);
        }
        (WIDGET_POPUP, "info") => {
            let title = if options.is_empty() { None } else { Some(options.to_string()) };
            if title.as_deref() == Some("Error") {
                state.has_error = true;
            }
            state.popup.show_info(title, value, Some(palette::FG), Some(palette::ACCENT));
        }
        (WIDGET_POPUP, CMD_HIDE) => {
            state.popup.hide();
        }
        (WIDGET_SPINNER, CMD_SHOW) => {
            state.popup.show_spinner(value);
        }
        (WIDGET_SPINNER, CMD_HIDE) => {
            state.popup.hide();
        }
        (WIDGET_PASSWORD, CMD_SHOW) => {
            state.popup.show_password(options, value);
        }
        (WIDGET_PASSWORD, CMD_HIDE) => {
            state.popup.hide();
        }
        _ => {}
    }
}

/// Terminal emulator wrapper around [`vt100::Parser`] with DEC Special Graphics
/// character set translation and HVP-to-CUP normalization.
struct Term {
    parser: vt100::Parser,
    responses: Vec<Vec<u8>>,
    escape_state: EscapeState,
    g0_dec_special_graphics: bool,
    g1_dec_special_graphics: bool,
    using_g1_charset: bool,
    application_cursor_keys: bool,
    csi_bytes: Vec<u8>,
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
            responses: Vec::new(),
            escape_state: EscapeState::Ground,
            g0_dec_special_graphics: false,
            g1_dec_special_graphics: false,
            using_g1_charset: false,
            application_cursor_keys: false,
            csi_bytes: Vec::new(),
        }
    }

    /// Returns a reference to the vt100 screen grid for rendering.
    fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Resizes the terminal grid after a window resize event.
    fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Feeds raw bytes through DEC translation then into the vt100 parser.
    fn process(&mut self, bytes: &[u8]) {
        self.process_bytes(bytes);
    }

    fn drain_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.responses)
    }

    #[allow(dead_code)]
    fn application_cursor_keys(&self) -> bool {
        self.application_cursor_keys
    }

    fn active_dec_special_graphics(&self) -> bool {
        if self.using_g1_charset {
            self.g1_dec_special_graphics
        } else {
            self.g0_dec_special_graphics
        }
    }

    /// Translates `\e(0` DEC Special Graphics characters to Unicode
    /// box-drawing glyphs, normalizes HVP (`CSI … f`) to CUP (`CSI … H`),
    /// and answers stream-split terminal queries after preceding bytes have
    /// been parsed.
    fn process_bytes(&mut self, bytes: &[u8]) {
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
                        self.csi_bytes.clear();
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
                    self.csi_bytes.push(byte);
                    if byte == b'f' {
                        translated.push(b'H');
                    } else {
                        translated.push(byte);
                    }
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape_state = EscapeState::Ground;
                        if self.handle_csi_complete(&mut translated) {
                            translated.clear();
                        }
                        self.csi_bytes.clear();
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

        if !translated.is_empty() {
            self.parser.process(&translated);
        }
    }

    fn handle_csi_complete(&mut self, translated: &mut Vec<u8>) -> bool {
        match self.csi_bytes.as_slice() {
            b"?1h" => self.application_cursor_keys = true,
            b"?1l" => self.application_cursor_keys = false,
            b"6n" => {
                self.parser.process(translated);
                let (cur_row, cur_col) = self.parser.screen().cursor_position();
                self.responses.push(format!("\x1b[{};{}R", cur_row + 1, cur_col + 1).into_bytes());
                return true;
            }
            b"18t" => {
                self.parser.process(translated);
                let (rows, cols) = self.parser.screen().size();
                self.responses.push(format!("\x1b[8;{};{}t", rows, cols).into_bytes());
                return true;
            }
            _ => {}
        }

        false
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

fn hold_error_popup(overlay: &Arc<Mutex<OverlayState>>, term: &mut Terminal<CrosstermBackend<io::Stdout>>, screen: &vt100::Screen) {
    let is_error = overlay.lock().unwrap().has_error;
    if !is_error {
        return;
    }

    {
        let mut state = overlay.lock().unwrap();
        state.popup.show_info(Some("Error".to_string()), "Press any key to exit", Some(palette::FG), Some(palette::ACCENT));
    }
    term.draw(|f| render_frame(f, screen, &overlay.lock().unwrap())).ok();
    let _ = event::read();
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

        let mut pty_output = false;

        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = unsafe { libc::read(master_fd, pty_buf.as_mut_ptr() as *mut libc::c_void, pty_buf.len()) };
            if n > 0 {
                term.process(&pty_buf[..n as usize]);
                for response in term.drain_responses() {
                    unsafe {
                        libc::write(master_fd, response.as_ptr() as *const libc::c_void, response.len());
                    }
                }
                pty_output = true;
            } else {
                hold_error_popup(&overlay, &mut terminal, term.screen());
                break;
            }
        }

        if fds[1].revents & libc::POLLIN != 0 {
            if let Ok(Event::Key(key)) = event::read() {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                let is_password = overlay.lock().is_ok_and(|s| matches!(s.popup.content, popup::PopupContent::Password { .. }));
                if is_password {
                    handle_password_key(&overlay, status_fd, key);
                } else if let Some(bytes) = key_to_bytes(&key, term.application_cursor_keys()) {
                    unsafe {
                        libc::write(master_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
                    }
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
                } else if let Some(cmd) = line.strip_prefix('@') {
                    if let Some((widget, cmd, opts, val)) = vscomm::decode_ui_payload(cmd.as_bytes()) {
                        let mut state = overlay.lock().unwrap();
                        dispatch_ui_command(&mut state, widget, cmd, opts, val);
                    }
                } else {
                    let mut state = overlay.lock().unwrap();
                    let title = state.popup_title.clone();
                    state.popup.show_info(title, &line, Some(palette::FG), Some(palette::ACCENT));
                }
            }
        }

        {
            let mut state = overlay.lock().unwrap();
            let now = Instant::now();

            if pty_output {
                for action in &mut state.pending {
                    if action.first_pty_at.is_none() && action.triggers.iter().any(|t| matches!(t, Trigger::OnPty | Trigger::DelayMsAfterPty(_))) {
                        action.first_pty_at = Some(now);
                    }
                }
            }

            let mut i = 0;
            while i < state.pending.len() {
                let fire = state.pending[i].triggers.iter().any(|t| match t {
                    Trigger::OnPty => pty_output,
                    Trigger::DelayMs(d) => (now - state.pending[i].enqueued_at).as_millis() as u64 >= *d,
                    Trigger::DelayMsAfterPty(d) => {
                        if let Some(first) = state.pending[i].first_pty_at {
                            (now - first).as_millis() as u64 >= *d
                        } else {
                            false
                        }
                    }
                });
                if fire {
                    let action = state.pending.remove(i);
                    dispatch_ui_command(&mut state, &action.widget, &action.command, "", &action.value);
                } else {
                    i += 1;
                }
            }

            state.popup.tick();

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

fn handle_password_key(overlay: &Arc<Mutex<OverlayState>>, status_fd: RawFd, key: KeyEvent) {
    let mut state = overlay.lock().unwrap();
    match key.code {
        KeyCode::Enter => {
            if let Some(password) = state.popup.take_password() {
                let mut response = password.into_bytes();
                response.push(b'\n');
                state.popup.hide();
                unsafe {
                    libc::write(status_fd, response.as_ptr() as *const libc::c_void, response.len());
                }
            }
        }
        KeyCode::Backspace => state.popup.pop_char(),
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => state.popup.push_char(c),
        _ => {}
    }
}

fn key_to_bytes(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if c.is_ascii_alphabetic() {
                    Some(vec![(c.to_ascii_lowercase() as u8) & 0x1f])
                } else {
                    None
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                let mut v = vec![0x1b];
                let mut buf = [0u8; 4];
                let len = c.encode_utf8(&mut buf).len();
                v.extend_from_slice(&buf[..len]);
                Some(v)
            } else {
                let mut buf = [0u8; 4];
                let len = c.encode_utf8(&mut buf).len();
                Some(buf[..len].to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(if app_cursor { b"\x1bOA".to_vec() } else { b"\x1b[A".to_vec() }),
        KeyCode::Down => Some(if app_cursor { b"\x1bOB".to_vec() } else { b"\x1b[B".to_vec() }),
        KeyCode::Right => Some(if app_cursor { b"\x1bOC".to_vec() } else { b"\x1b[C".to_vec() }),
        KeyCode::Left => Some(if app_cursor { b"\x1bOD".to_vec() } else { b"\x1b[D".to_vec() }),
        KeyCode::Home => Some(if app_cursor { b"\x1bOH".to_vec() } else { b"\x1b[H".to_vec() }),
        KeyCode::End => Some(if app_cursor { b"\x1bOF".to_vec() } else { b"\x1b[F".to_vec() }),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => fn_key(n),
        _ => None,
    }
}

fn fn_key(n: u8) -> Option<Vec<u8>> {
    match n {
        1 => Some(b"\x1bOP".to_vec()),
        2 => Some(b"\x1bOQ".to_vec()),
        3 => Some(b"\x1bOR".to_vec()),
        4 => Some(b"\x1bOS".to_vec()),
        5 => Some(b"\x1b[15~".to_vec()),
        6 => Some(b"\x1b[17~".to_vec()),
        7 => Some(b"\x1b[18~".to_vec()),
        8 => Some(b"\x1b[19~".to_vec()),
        9 => Some(b"\x1b[20~".to_vec()),
        10 => Some(b"\x1b[21~".to_vec()),
        11 => Some(b"\x1b[23~".to_vec()),
        12 => Some(b"\x1b[24~".to_vec()),
        _ => None,
    }
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
                if cell.dim() {
                    style = style.add_modifier(Modifier::DIM);
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
                let display: &str = if ch.is_empty() { " " } else { ch };

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

    {
        let buf = f.buffer_mut();
        overlay.popup.render(area, buf);
    }

    let (cursor_row, cursor_col) = screen.cursor_position();
    if cursor_row < max_rows && cursor_col < max_cols {
        f.set_cursor_position((area.x + cursor_col, area.y + cursor_row));
    }
}

#[cfg(test)]
mod tests {
    use super::Term;

    #[test]
    fn cursor_report_uses_position_after_prior_bytes_in_same_chunk() {
        let mut term = Term::new(24, 80);

        term.process(b"\x1b[10;20H\x1b[6n");

        assert_eq!(term.drain_responses(), vec![b"\x1b[10;20R".to_vec()]);
    }

    #[test]
    fn cursor_report_handles_split_query() {
        let mut term = Term::new(24, 80);

        term.process(b"\x1b[10;20H\x1b[");
        assert!(term.drain_responses().is_empty());
        term.process(b"6n");

        assert_eq!(term.drain_responses(), vec![b"\x1b[10;20R".to_vec()]);
    }

    #[test]
    fn cursor_mode_handles_split_sequences() {
        let mut term = Term::new(24, 80);

        term.process(b"\x1b[?");
        term.process(b"1h");
        assert!(term.application_cursor_keys());

        term.process(b"\x1b[?1");
        term.process(b"l");
        assert!(!term.application_cursor_keys());
    }

    #[test]
    fn window_size_report_handles_split_query() {
        let mut term = Term::new(24, 80);

        term.process(b"\x1b[18");
        assert!(term.drain_responses().is_empty());
        term.process(b"t");

        assert_eq!(term.drain_responses(), vec![b"\x1b[8;24;80t".to_vec()]);
    }
}
