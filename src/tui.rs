use std::ffi::CString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::*;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget, Wrap};

use ratatui::Terminal;

use crate::remote_target::{ActiveBuildTarget, BuildTargetCatalog};
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
const WIDGET_ERROR: &str = "error";

const CMD_SHOW: &str = "show";
const CMD_HIDE: &str = "hide";
const CMD_SET: &str = "set";
const CMD_CLEAR: &str = "clear";

const ERROR_TOAST_IN: Duration = Duration::from_millis(220);
const ERROR_TOAST_HOLD: Duration = Duration::from_secs(5);
const ERROR_TOAST_OUT: Duration = Duration::from_millis(260);

struct ErrorToast {
    title: String,
    message: String,
    shown_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseTracking {
    Off,
    Normal,
    Button,
    Any,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseEncoding {
    X10,
    Urxvt,
    Sgr,
}

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
    error_toast: Option<ErrorToast>,
    pub hide_on_ascii: bool,
    pub hide_on_content: Option<String>,
    pub last_content_scan: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostPopup {
    None,
    Targets,
    Help,
}

struct HostUiState {
    popup: HostPopup,
    target_index: usize,
    confirmation: Option<(String, Instant)>,
    free_bytes: Option<u64>,
    last_free_refresh: Instant,
}

impl HostUiState {
    fn new(catalog: &BuildTargetCatalog) -> Self {
        Self {
            popup: HostPopup::None,
            target_index: catalog.summaries().iter().position(|target| target.label() == "localhost").unwrap_or(0),
            confirmation: None,
            free_bytes: None,
            last_free_refresh: Instant::now() - Duration::from_secs(10),
        }
    }

    fn refresh_free_space(&mut self, catalog: &BuildTargetCatalog) {
        if self.last_free_refresh.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_free_refresh = Instant::now();
        self.free_bytes = local_free_space(catalog.project_root());
    }

    fn handle_key(&mut self, key: KeyEvent, catalog: &BuildTargetCatalog, active: &ActiveBuildTarget) -> bool {
        match self.popup {
            HostPopup::Targets => {
                match key.code {
                    KeyCode::Up => {
                        self.target_index = self.target_index.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        self.target_index = (self.target_index + 1).min(catalog.summaries().len().saturating_sub(1));
                    }
                    KeyCode::Enter => {
                        if let Some(target) = catalog.summaries().get(self.target_index) {
                            if active.select(catalog, target.label()).is_ok() {
                                self.confirmation = Some((format!("Build target: {}", target.label()), Instant::now()));
                            }
                        }
                        self.popup = HostPopup::None;
                    }
                    KeyCode::Esc => {
                        self.popup = HostPopup::None;
                    }
                    _ => {}
                }
                return true;
            }
            HostPopup::Help => {
                if key.code == KeyCode::Esc {
                    self.popup = HostPopup::None;
                }
                return true;
            }
            HostPopup::None => {}
        }

        if key.code == KeyCode::Char('b') && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) {
            let current = active.current();
            self.target_index = catalog.summaries().iter().position(|target| target.label() == current).unwrap_or(0);
            self.popup = HostPopup::Targets;
            return true;
        }
        if key.code == KeyCode::Char('h') && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) {
            self.popup = HostPopup::Help;
            return true;
        }
        false
    }
}

pub fn show_host_error(overlay: &Arc<Mutex<OverlayState>>, title: &str, message: &str) {
    if let Ok(mut state) = overlay.lock() {
        state.error_toast =
            Some(ErrorToast { title: title.chars().take(80).collect(), message: message.chars().take(512).collect(), shown_at: Instant::now() });
    }
}

pub fn guest_rows(physical_rows: u16) -> u16 {
    physical_rows.saturating_sub(1).max(1)
}

impl Default for OverlayState {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayState {
    pub fn new() -> Self {
        Self {
            status_text: String::new(),
            popup: PopupWidget::new(),
            popup_title: None,
            pending: Vec::new(),
            has_error: false,
            error_toast: None,
            hide_on_ascii: false,
            hide_on_content: None,
            last_content_scan: Instant::now(),
        }
    }
}

pub fn dispatch_ui_command(state: &mut OverlayState, widget: &str, command: &str, options: &str, value: &str) {
    if widget == WIDGET_ERROR && command == CMD_SHOW {
        state.error_toast = Some(ErrorToast {
            title: if options.is_empty() { "Bunkerbox error".to_string() } else { options.chars().take(80).collect() },
            message: value.chars().take(512).collect(),
            shown_at: Instant::now(),
        });
        return;
    }

    if widget == WIDGET_POPUP && command == CMD_HIDE && !value.is_empty() {
        if value == "ASCII" {
            state.hide_on_ascii = true;
        } else {
            state.hide_on_content = Some(value.to_string());
        }
        return;
    }

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
    mouse_normal: bool,
    mouse_button: bool,
    mouse_any: bool,
    mouse_sgr: bool,
    mouse_urxvt: bool,
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
            mouse_normal: false,
            mouse_button: false,
            mouse_any: false,
            mouse_sgr: false,
            mouse_urxvt: false,
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

    fn mouse_tracking(&self) -> MouseTracking {
        if self.mouse_any {
            MouseTracking::Any
        } else if self.mouse_button {
            MouseTracking::Button
        } else if self.mouse_normal {
            MouseTracking::Normal
        } else {
            MouseTracking::Off
        }
    }

    fn mouse_encoding(&self) -> MouseEncoding {
        if self.mouse_sgr {
            MouseEncoding::Sgr
        } else if self.mouse_urxvt {
            MouseEncoding::Urxvt
        } else {
            MouseEncoding::X10
        }
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

    fn handle_csi_complete(&mut self, translated: &mut [u8]) -> bool {
        self.update_private_modes();

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

    fn update_private_modes(&mut self) {
        let bytes = self.csi_bytes.clone();
        if bytes.first() != Some(&b'?') || bytes.len() < 3 {
            return;
        }

        let Some((&final_byte, params)) = bytes.split_last() else {
            return;
        };
        let enabled = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };

        for raw_mode in params[1..].split(|byte| *byte == b';') {
            let Ok(mode) = std::str::from_utf8(raw_mode).unwrap_or_default().parse::<u16>() else {
                continue;
            };
            match mode {
                1000 => self.mouse_normal = enabled,
                1002 => self.mouse_button = enabled,
                1003 => self.mouse_any = enabled,
                1006 => self.mouse_sgr = enabled,
                1015 => self.mouse_urxvt = enabled,
                1005 => {}
                _ => {}
            }
        }
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

fn hold_error_popup(
    overlay: &Arc<Mutex<OverlayState>>, term: &mut Terminal<CrosstermBackend<io::Stdout>>, screen: &vt100::Screen, host: &HostUiState,
    catalog: &BuildTargetCatalog, active: &ActiveBuildTarget,
) {
    let is_error = overlay.lock().unwrap().has_error;
    if !is_error {
        return;
    }

    {
        let mut state = overlay.lock().unwrap();
        state.popup.show_info(Some("Error".to_string()), "Press any key to exit", Some(palette::FG), Some(palette::ACCENT));
    }
    term.draw(|f| render_frame(f, screen, &overlay.lock().unwrap(), host, catalog, active)).ok();
    let _ = event::read();
}

fn screen_contains(screen: &vt100::Screen, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let (rows, cols) = screen.size();
    let mut buf = String::with_capacity((rows * cols) as usize);
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if cell.is_wide_continuation() {
                    continue;
                }
                buf.push_str(cell.contents());
            }
        }
    }
    buf.contains(pattern)
}

fn screen_has_ascii_alphanumeric(screen: &vt100::Screen) -> bool {
    let (rows, cols) = screen.size();
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.contents().chars().any(|c| c.is_ascii_alphanumeric()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Renders PTY output through ratatui until the child process exits.
///
/// Forwards real keystrokes to the PTY master, parses terminal output through
/// [`vt100::Parser`], and draws each frame with full 24-bit color plus a
/// floating status overlay in the top-right corner.
///
/// `status_fd` is polled continuously for newline-delimited UI messages.
///
/// `overlay` is shared with the VSOCK status listener so VM-originated
/// UI commands can update popups, progress bars, and status text.
#[allow(clippy::too_many_arguments)]
pub fn event_loop(
    master_fd: RawFd, rows: u16, cols: u16, status_fd: RawFd, startup_status_fd: RawFd, overlay: Arc<Mutex<OverlayState>>,
    catalog: BuildTargetCatalog, active: ActiveBuildTarget,
) -> Result<(), String> {
    let stdin_fd = io::stdin().as_raw_fd();

    let mut stdout = io::stdout();
    terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    stdout.execute(EnterAlternateScreen).map_err(|e| format!("alt screen: {e}"))?;
    stdout.execute(cursor::Show).map_err(|e| format!("cursor: {e}"))?;

    let mut term = Term::new(guest_rows(rows), cols);

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("terminal: {e}"))?;

    let mut pty_buf = [0u8; 4096];

    let mut last_rows = rows;
    let mut last_cols = cols;
    let mut host = HostUiState::new(&catalog);

    let mut status_buf = Vec::new();
    let mut startup_status_buf = Vec::new();
    let mut startup_status_fd = startup_status_fd;
    let mut mouse_capture_enabled = false;

    unsafe {
        libc::signal(libc::SIGWINCH, handle_sigwinch as *const () as libc::sighandler_t);
    }

    loop {
        if RESIZED.swap(false, Ordering::SeqCst) {
            if let Ok((new_cols, new_rows)) = terminal::size() {
                if new_cols != last_cols || new_rows != last_rows {
                    last_cols = new_cols;
                    last_rows = new_rows;
                    term.set_size(guest_rows(new_rows), new_cols);
                    let ws = libc::winsize { ws_row: guest_rows(new_rows), ws_col: new_cols, ws_xpixel: 0, ws_ypixel: 0 };
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
            libc::pollfd { fd: startup_status_fd, events: libc::POLLIN, revents: 0 },
        ];

        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 4, 16) };

        if ret == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            cleanup_terminal(&mut terminal, mouse_capture_enabled);
            return Err(format!("poll: {err}"));
        }

        let mut pty_output = false;

        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = unsafe { libc::read(master_fd, pty_buf.as_mut_ptr() as *mut libc::c_void, pty_buf.len()) };
            if n > 0 {
                term.process(&pty_buf[..n as usize]);
                let wants_mouse_capture = term.mouse_tracking() != MouseTracking::Off;
                if wants_mouse_capture != mouse_capture_enabled {
                    let result = if wants_mouse_capture {
                        terminal.backend_mut().execute(EnableMouseCapture).map(|_| ())
                    } else {
                        terminal.backend_mut().execute(DisableMouseCapture).map(|_| ())
                    };
                    if let Err(err) = result {
                        cleanup_terminal(&mut terminal, mouse_capture_enabled);
                        return Err(format!("mouse capture: {err}"));
                    }
                    mouse_capture_enabled = wants_mouse_capture;
                }
                for response in term.drain_responses() {
                    unsafe {
                        libc::write(master_fd, response.as_ptr() as *const libc::c_void, response.len());
                    }
                }
                pty_output = true;
            } else {
                hold_error_popup(&overlay, &mut terminal, term.screen(), &host, &catalog, &active);
                break;
            }
        }

        if fds[2].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            read_status_messages(status_fd, &mut status_buf, &overlay);
        }
        if startup_status_fd >= 0
            && fds[3].revents & (libc::POLLIN | libc::POLLHUP) != 0
            && read_status_messages(startup_status_fd, &mut startup_status_buf, &overlay) == 0
        {
            startup_status_fd = -1;
        }

        if fds[1].revents & libc::POLLIN != 0 {
            if let Ok(input_event) = event::read() {
                match input_event {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }

                        let is_password = overlay.lock().is_ok_and(|s| matches!(s.popup.content, popup::PopupContent::Password { .. }));
                        if is_password {
                            handle_password_key(&overlay, status_fd, key);
                        } else if host.handle_key(key, &catalog, &active) {
                            continue;
                        } else if let Some(bytes) = key_to_bytes(&key, term.application_cursor_keys()) {
                            unsafe {
                                libc::write(master_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        if mouse.row >= guest_rows(last_rows) {
                            continue;
                        }
                        if let Some(bytes) = mouse_to_bytes(mouse, term.mouse_tracking(), term.mouse_encoding()) {
                            unsafe {
                                libc::write(master_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        {
            let mut state = overlay.lock().unwrap();
            let now = Instant::now();
            host.refresh_free_space(&catalog);

            if pty_output {
                for action in &mut state.pending {
                    if action.first_pty_at.is_none() && action.triggers.iter().any(|t| matches!(t, Trigger::OnPty | Trigger::DelayMsAfterPty(_))) {
                        action.first_pty_at = Some(now);
                    }
                }
            }

            if (now - state.last_content_scan).as_millis() >= 100 {
                state.last_content_scan = now;
                if state.hide_on_ascii && screen_has_ascii_alphanumeric(term.screen()) {
                    state.popup.hide();
                    state.hide_on_ascii = false;
                }
                if let Some(ref pat) = state.hide_on_content.clone() {
                    if screen_contains(term.screen(), pat) {
                        state.popup.hide();
                        state.hide_on_content = None;
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
            if let Some(toast) = state.error_toast.as_ref() {
                let lifetime = ERROR_TOAST_IN + ERROR_TOAST_HOLD + ERROR_TOAST_OUT;
                if toast.shown_at.elapsed() >= lifetime {
                    state.error_toast = None;
                }
            }

            if let Err(err) = terminal.draw(|f| render_frame(f, term.screen(), &state, &host, &catalog, &active)) {
                cleanup_terminal(&mut terminal, mouse_capture_enabled);
                return Err(format!("draw: {err}"));
            }
        }
    }

    cleanup_terminal(&mut terminal, mouse_capture_enabled);

    Ok(())
}

fn read_status_messages(fd: RawFd, buffer: &mut Vec<u8>, overlay: &Arc<Mutex<OverlayState>>) -> isize {
    let mut chunk = [0u8; 256];
    let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
    if n <= 0 {
        return 0;
    }
    process_status_bytes(buffer, &chunk[..n as usize], overlay);
    n
}

fn process_status_bytes(buffer: &mut Vec<u8>, bytes: &[u8], overlay: &Arc<Mutex<OverlayState>>) {
    buffer.extend_from_slice(bytes);
    while let Some(pos) = buffer.iter().position(|&byte| byte == b'\n') {
        let line = String::from_utf8_lossy(&buffer[..pos]).into_owned();
        buffer.drain(..=pos);
        if let Some(command) = line.strip_prefix('@') {
            if let Some((widget, command, options, value)) = vscomm::decode_ui_payload(command.as_bytes()) {
                let mut state = overlay.lock().unwrap();
                dispatch_ui_command(&mut state, widget, command, options, value);
            }
        } else {
            let mut state = overlay.lock().unwrap();
            let title = state.popup_title.clone();
            state.popup.show_info(title, &line, Some(palette::FG), Some(palette::ACCENT));
        }
    }
}

fn cleanup_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, mouse_capture_enabled: bool) {
    if mouse_capture_enabled {
        terminal.backend_mut().execute(DisableMouseCapture).ok();
    }
    terminal.backend_mut().execute(LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    unsafe {
        libc::signal(libc::SIGWINCH, libc::SIG_DFL);
    }
}

fn handle_password_key(overlay: &Arc<Mutex<OverlayState>>, status_fd: RawFd, key: KeyEvent) {
    let mut state = overlay.lock().unwrap();
    if key.code == KeyCode::Enter {
        if let Some(password) = state.popup.password_value() {
            let mut response = password.into_bytes();
            response.push(b'\n');
            state.popup.hide();
            unsafe {
                libc::write(status_fd, response.as_ptr() as *const libc::c_void, response.len());
            }
        }
    } else {
        state.popup.handle_password_key(&key);
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

fn mouse_to_bytes(event: MouseEvent, tracking: MouseTracking, encoding: MouseEncoding) -> Option<Vec<u8>> {
    let (base_code, is_drag, is_release) = match event.kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), false, false),
        MouseEventKind::Up(button) => (mouse_button_code(button), false, true),
        MouseEventKind::Drag(button) => (mouse_button_code(button), true, false),
        MouseEventKind::Moved => (3, true, false),
        MouseEventKind::ScrollUp => (64, false, false),
        MouseEventKind::ScrollDown => (65, false, false),
        MouseEventKind::ScrollLeft => (66, false, false),
        MouseEventKind::ScrollRight => (67, false, false),
    };

    match (tracking, event.kind) {
        (MouseTracking::Off, _) => return None,
        (MouseTracking::Normal, MouseEventKind::Drag(_) | MouseEventKind::Moved) => return None,
        (MouseTracking::Button, MouseEventKind::Moved) => return None,
        _ => {}
    }

    let mut code = base_code;
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    if is_drag {
        code += 32;
    }

    let column = u32::from(event.column) + 1;
    let row = u32::from(event.row) + 1;

    match encoding {
        MouseEncoding::Sgr => {
            let suffix = if is_release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{};{}{}", code, column, row, suffix).into_bytes())
        }
        MouseEncoding::Urxvt => {
            let legacy_code = if is_release { 3 } else { code };
            Some(format!("\x1b[{};{};{}M", legacy_code + 32, column, row).into_bytes())
        }
        MouseEncoding::X10 => {
            let legacy_code = if is_release { 3 } else { code };
            if column > 223 || row > 223 || legacy_code + 32 > 255 {
                return None;
            }
            Some(vec![0x1b, b'[', b'M', (legacy_code + 32) as u8, (column + 32) as u8, (row + 32) as u8])
        }
    }
}

fn mouse_button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
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

fn local_free_space(path: &std::path::Path) -> Option<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = unsafe { std::mem::zeroed::<libc::statvfs>() };
    let result = unsafe { libc::statvfs(path.as_ptr(), &mut stats) };
    if result != 0 {
        return None;
    }
    stats.f_bavail.checked_mul(stats.f_frsize)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + area.width.saturating_sub(width) / 2, y: area.y + area.height.saturating_sub(height) / 2, width, height }
}

fn render_status_bar(area: Rect, buf: &mut Buffer, host: &HostUiState, active: &ActiveBuildTarget) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let target = active.current();
    let free = host.free_bytes.map(format_bytes).unwrap_or_else(|| "unknown".to_string());
    let target_segment = host
        .confirmation
        .as_ref()
        .filter(|(_, shown_at)| shown_at.elapsed() < Duration::from_secs(3))
        .map_or_else(|| format!("Target: {target}"), |(message, _)| format!("Target: {target} ({message})"));
    let mut segments = vec![target_segment, "Ctrl+Alt+B Targets".to_string(), "Ctrl+Alt+H Help".to_string(), format!("Local workspace free: {free}")];
    while segments.len() > 1 {
        let text = format!(" {}", segments.join(" | "));
        if text.chars().count() <= usize::from(area.width) {
            break;
        }
        segments.pop();
    }
    let mut text = format!(" {}", segments.join(" | "));
    if text.chars().count() > usize::from(area.width) {
        text = text.chars().take(usize::from(area.width)).collect();
    }
    Paragraph::new(text).style(Style::default().fg(palette::FG).bg(palette::BG_1)).render(area, buf);
}

fn render_host_popup(area: Rect, buf: &mut Buffer, host: &HostUiState, catalog: &BuildTargetCatalog) {
    let (title, lines, height) = match host.popup {
        HostPopup::None => return,
        HostPopup::Help => (
            "Bunkerbox Help",
            vec![
                Line::from(Span::styled("Ctrl-Alt-B", Style::default().fg(palette::ACCENT))),
                Line::from("Select Build Target"),
                Line::from(Span::styled("Ctrl-Alt-H", Style::default().fg(palette::ACCENT))),
                Line::from("Show this help"),
                Line::from(Span::styled("Esc", Style::default().fg(palette::ACCENT))),
                Line::from("Close host popup"),
            ],
            10,
        ),
        HostPopup::Targets => {
            let mut lines = Vec::new();
            for (index, target) in catalog.summaries().iter().enumerate() {
                let marker = if index == host.target_index { "> " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::default().fg(palette::ACCENT)),
                    Span::styled(target.label(), Style::default().fg(palette::FG)),
                    Span::styled(format!(": {}", target.workspace()), Style::default().fg(palette::MUTED)),
                ]));
            }
            let height = (lines.len() as u16 + 4).max(5);
            ("Build Targets", lines, height)
        }
    };
    let popup_area = centered_rect(area, 64, height);
    if popup_area.width < 4 || popup_area.height < 3 {
        return;
    }
    Clear.render(popup_area, buf);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::ACCENT))
        .style(Style::default().fg(palette::FG).bg(palette::POPUP_BG))
        .padding(Padding::horizontal(1));
    Paragraph::new(lines).block(block).render(popup_area, buf);
}

/// Renders one frame: writes the guest vt100 screen into the reduced guest
/// viewport, then draws host-owned status and popup controls.
fn render_frame(
    f: &mut Frame, screen: &vt100::Screen, overlay: &OverlayState, host: &HostUiState, catalog: &BuildTargetCatalog, active: &ActiveBuildTarget,
) {
    let area = f.area();
    let guest_area = Rect { height: area.height.saturating_sub(1), ..area };
    let (rows, cols) = screen.size();
    let max_rows = guest_area.height.min(rows);
    let max_cols = guest_area.width.min(cols);
    let buf = f.buffer_mut();

    for row in 0..max_rows {
        let mut col: u16 = 0;
        while col < max_cols {
            let x = guest_area.x + col;
            let y = guest_area.y + row;

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
        overlay.popup.render(guest_area, buf);
        render_error_toast(guest_area, buf, overlay.error_toast.as_ref());
        render_status_bar(Rect { y: area.bottom().saturating_sub(1), height: area.height.min(1), ..area }, buf, host, active);
        render_host_popup(area, buf, host, catalog);
    }

    let (cursor_row, cursor_col) = screen.cursor_position();
    if cursor_row < max_rows && cursor_col < max_cols {
        f.set_cursor_position((guest_area.x + cursor_col, guest_area.y + cursor_row));
    }
}

fn render_error_toast(area: Rect, buf: &mut Buffer, toast: Option<&ErrorToast>) {
    let Some(toast) = toast else {
        return;
    };

    let elapsed = toast.shown_at.elapsed();
    let width = toast
        .message
        .lines()
        .chain(std::iter::once(toast.title.as_str()))
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(24)
        .saturating_add(8)
        .max(28)
        .min(area.width.saturating_sub(2));
    let height = (toast.message.lines().count().max(1) as u16 + 4).min(area.height.saturating_sub(2));
    if width < 4 || height < 3 {
        return;
    }

    let travel = width.saturating_add(2);
    let target_x = area.right().saturating_sub(travel);
    let offset = if elapsed < ERROR_TOAST_IN {
        let progress = elapsed.as_secs_f64() / ERROR_TOAST_IN.as_secs_f64();
        ((1.0 - progress) * f64::from(travel)) as u16
    } else if elapsed < ERROR_TOAST_IN + ERROR_TOAST_HOLD {
        0
    } else {
        let out_elapsed = elapsed - ERROR_TOAST_IN - ERROR_TOAST_HOLD;
        let progress = (out_elapsed.as_secs_f64() / ERROR_TOAST_OUT.as_secs_f64()).min(1.0);
        (progress * f64::from(travel)) as u16
    };
    let x = target_x.saturating_add(offset);
    let y = area.y.saturating_add(1);
    let canvas = Rect { x, y, width, height };

    Clear.render(canvas, buf);
    let block = Block::default()
        .title(toast.title.as_str())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::ERROR))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(palette::BG_1));
    let inner = block.inner(canvas);
    block.render(canvas, buf);
    Paragraph::new(toast.message.as_str()).style(Style::default().fg(palette::FG)).wrap(Wrap { trim: true }).render(inner, buf);
}

#[cfg(test)]
#[path = "tui_ut.rs"]
mod tui_tests;
