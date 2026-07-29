use std::time::Instant;

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    prelude::{Buffer, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Gauge, Padding, Paragraph, Widget},
};
use ratatui_glamour::{color::blend_2d, widgets::spinner};

use super::palette;

const SPINNER_FPS_MS: u64 = 83;

pub enum PopupContent {
    Spinner { message: String, model: spinner::Model, last_tick: Instant },
    Progress { title: String, percent: f64, label: Option<String> },
    Password { title: String, prompt: String, value: String },
    Info { title: Option<String>, message: String, fg: ratatui::style::Color },
}

pub struct PopupWidget {
    pub visible: bool,
    pub content: PopupContent,
    pub border_color: ratatui::style::Color,
    shadow: bool,
}

impl PopupWidget {
    pub fn new() -> Self {
        Self {
            visible: false,
            content: PopupContent::Info { title: None, message: String::new(), fg: palette::FG },
            border_color: palette::BORDER,
            shadow: true,
        }
    }

    pub fn show_spinner(&mut self, message: impl Into<String>) {
        let mut model = spinner::Model::new();
        model.spinner = spinner::Spinner::mini_dot();
        model.style = Style::default().fg(palette::ACCENT);
        self.content = PopupContent::Spinner { message: message.into(), model, last_tick: Instant::now() };
        self.visible = true;
    }

    pub fn show_progress(&mut self, title: impl Into<String>, percent: f64, label: Option<String>) {
        self.border_color = palette::ACCENT;
        self.content = PopupContent::Progress { title: title.into(), percent: percent.clamp(0.0, 1.0), label };
        self.visible = true;
    }

    pub fn set_progress(&mut self, pct: f64, lbl: Option<String>) {
        if let PopupContent::Progress { ref mut percent, ref mut label, .. } = &mut self.content {
            *percent = pct.clamp(0.0, 1.0);
            *label = lbl;
        }
    }

    pub fn show_password(&mut self, title: impl Into<String>, prompt: impl Into<String>) {
        self.border_color = palette::ACCENT;
        self.content = PopupContent::Password { title: title.into(), prompt: prompt.into(), value: String::new() };
        self.visible = true;
    }

    pub fn show_info(
        &mut self, title: Option<String>, message: impl Into<String>, fg: Option<ratatui::style::Color>, border_color: Option<ratatui::style::Color>,
    ) {
        self.border_color = border_color.unwrap_or(palette::BORDER);
        self.content = PopupContent::Info { title, message: message.into(), fg: fg.unwrap_or(palette::FG) };
        self.visible = true;
    }

    pub fn tick(&mut self) {
        if let PopupContent::Spinner { ref mut model, ref mut last_tick, .. } = &mut self.content {
            if last_tick.elapsed().as_millis() as u64 >= SPINNER_FPS_MS {
                let tick = model.tick();
                model.update(tick);
                *last_tick = Instant::now();
            }
        }
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn push_char(&mut self, c: char) {
        if let PopupContent::Password { ref mut value, .. } = &mut self.content {
            value.push(c);
        }
    }

    pub fn pop_char(&mut self) {
        if let PopupContent::Password { ref mut value, .. } = &mut self.content {
            value.pop();
        }
    }

    pub fn take_password(&mut self) -> Option<String> {
        if matches!(self.content, PopupContent::Password { .. }) {
            let old =
                std::mem::replace(&mut self.content, PopupContent::Password { title: String::new(), prompt: String::new(), value: String::new() });
            if let PopupContent::Password { value, .. } = old {
                return Some(value);
            }
        }
        None
    }

    /// Compute the natural content dimensions (width, height) for the popup interior.
    fn content_size(&self) -> (u16, u16) {
        match &self.content {
            PopupContent::Spinner { message, .. } => {
                let w = (message.len() as u16 + 6).max(30);
                (w, 5)
            }
            PopupContent::Progress { title, label, .. } => {
                let label_w = label.as_ref().map_or(0, |l| l.len() as u16);
                let w = (title.len() as u16).max(label_w).max(40);
                (w + 8, 7)
            }
            PopupContent::Password { title: _, prompt, .. } => {
                let w = (prompt.len() as u16 + 20).max(40);
                (w, 7)
            }
            PopupContent::Info { message, .. } => {
                let lines = text_lines(message);
                let max_w = message.lines().map(|l| l.len() as u16).max().unwrap_or(20);
                let w = max_w.max(30);
                let h = lines.max(1) + 4;
                (w + 8, h)
            }
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if !self.visible {
            return;
        }

        let (content_w, content_h) = self.content_size();
        let width = content_w.min(area.width.saturating_sub(4)).max(20);
        let height = content_h.min(area.height.saturating_sub(4));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let canvas = Rect { x, y, width, height };

        Clear.render(canvas, buf);

        self.render_gradient(canvas, buf);

        let title_line = self.build_title();
        let has_title = !matches!(&self.content, PopupContent::Spinner { .. });

        let block = Block::default()
            .title(if has_title { title_line } else { Line::from("") })
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(self.border_color))
            .padding(Padding::horizontal(2))
            .style(Style::default());

        let inner = block.inner(canvas);
        block.render(canvas, buf);

        match &self.content {
            PopupContent::Spinner { message, model, .. } => {
                self.render_spinner(inner, buf, message, model);
            }
            PopupContent::Progress { title: _, percent, label } => {
                self.render_progress(inner, buf, *percent, label);
            }
            PopupContent::Password { title: _, prompt, value } => {
                self.render_password(inner, buf, prompt, value);
            }
            PopupContent::Info { message, fg, .. } => {
                self.render_info(inner, buf, message, *fg);
            }
        }

        if self.shadow {
            self.draw_shadow(canvas, height, buf);
        }
    }

    fn build_title(&self) -> Line<'static> {
        let (title_text, bc) = match &self.content {
            PopupContent::Progress { title, .. } => (title.clone(), self.border_color),
            PopupContent::Password { title, .. } => (title.clone(), self.border_color),
            PopupContent::Info { title, .. } => (title.clone().unwrap_or_default(), self.border_color),
            PopupContent::Spinner { .. } => return Line::from(""),
        };

        if title_text.is_empty() {
            return Line::from("");
        }

        let text: String = format!(" {} ", title_text);
        Line::from(vec![
            Span::styled("\u{E0B2}", Style::default().fg(bc)),
            Span::styled(text, Style::default().fg(palette::BLACK).bg(bc)),
            Span::styled("\u{E0B0}", Style::default().fg(bc)),
        ])
    }

    fn render_gradient(&self, canvas: Rect, buf: &mut Buffer) {
        let stops: &[ratatui::style::Color] = &[palette::GRAY_0, palette::BG_2];
        let colors = blend_2d(canvas.width as usize, canvas.height as usize, 10.0, stops);
        for row in 0..canvas.height {
            for col in 0..canvas.width {
                let idx = row as usize * canvas.width as usize + col as usize;
                if idx < colors.len() {
                    if let Some(cell) = buf.cell_mut(Position::new(canvas.x + col, canvas.y + row)) {
                        cell.set_bg(colors[idx]);
                    }
                }
            }
        }
    }

    fn render_spinner(&self, inner: Rect, buf: &mut Buffer, message: &str, model: &spinner::Model) {
        let frame_str = model.view();
        let text = format!("{}  {}", frame_str, message);
        let text_w = text.len() as u16;

        let [_, mid, _]: [Rect; 3] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1), Constraint::Min(0)])
            .split(inner)
            .as_ref()
            .try_into()
            .unwrap();

        let x = mid.x + (mid.width.saturating_sub(text_w)) / 2;
        let text_area = Rect { x, y: mid.y, width: text_w, height: 1 };

        let mut spans: Vec<Span> = vec![frame_str.spans[0].clone(), Span::raw("  ")];
        spans.push(Span::styled(message.to_string(), Style::default().fg(palette::FG)));
        Paragraph::new(Line::from(spans)).alignment(Alignment::Left).render(text_area, buf);
    }

    fn render_progress(&self, inner: Rect, buf: &mut Buffer, percent: f64, label: &Option<String>) {
        let [_, gauge_area, label_area, _]: [Rect; 4] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
            .split(inner)
            .as_ref()
            .try_into()
            .unwrap();

        let pct = (percent * 100.0) as u16;
        Gauge::default().gauge_style(Style::default().fg(palette::ACCENT).bg(palette::BG_3)).percent(pct).render(gauge_area, buf);

        if let Some(lbl) = label {
            let pct_text = format!(" {}  {}%", lbl, pct);
            let w = pct_text.len() as u16;
            let x = label_area.x + (label_area.width.saturating_sub(w)) / 2;
            let rect = Rect { x, y: label_area.y, width: w, height: 1 };
            Paragraph::new(pct_text).style(Style::default().fg(palette::MUTED)).alignment(Alignment::Center).render(rect, buf);
        } else {
            let pct_text = format!("{}%", pct);
            let w = pct_text.len() as u16;
            let x = label_area.x + (label_area.width.saturating_sub(w)) / 2;
            let rect = Rect { x, y: label_area.y, width: w, height: 1 };
            Paragraph::new(pct_text).style(Style::default().fg(palette::MUTED)).alignment(Alignment::Center).render(rect, buf);
        }
    }

    fn render_password(&self, inner: Rect, buf: &mut Buffer, prompt: &str, value: &str) {
        let [_, prompt_area, input_area, _]: [Rect; 4] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1), Constraint::Length(2), Constraint::Min(0)])
            .split(inner)
            .as_ref()
            .try_into()
            .unwrap();

        let pw = prompt.len() as u16;
        let px = prompt_area.x + (prompt_area.width.saturating_sub(pw)) / 2;
        Paragraph::new(prompt.to_string())
            .style(Style::default().fg(palette::FG))
            .alignment(Alignment::Center)
            .render(Rect { x: px, y: prompt_area.y, width: pw, height: 1 }, buf);

        let mask = "\u{2022}".repeat(if value.is_empty() { 12 } else { value.len() });
        let mw = mask.len() as u16;
        let mx = input_area.x + (input_area.width.saturating_sub(mw)) / 2;
        Paragraph::new(Line::from(vec![Span::styled(mask, Style::default().fg(palette::MUTED).bg(palette::BG_3))]))
            .alignment(Alignment::Center)
            .render(Rect { x: mx, y: input_area.y + 1, width: mw, height: 1 }, buf);
    }

    fn render_info(&self, inner: Rect, buf: &mut Buffer, message: &str, fg: ratatui::style::Color) {
        let text = format!("\n{}", message);
        Paragraph::new(text).alignment(Alignment::Center).style(Style::default().fg(fg)).render(inner, buf);
    }

    fn draw_shadow(&self, canvas: Rect, height: u16, buf: &mut Buffer) {
        let buf_area = buf.area();
        let max_x = buf_area.right().saturating_sub(1);
        let max_y = buf_area.bottom().saturating_sub(1);

        for idx in 0..canvas.width {
            let sx = canvas.x.saturating_add(2).saturating_add(idx);
            let sy = canvas.y.saturating_add(height);
            if sx > max_x || sy > max_y {
                continue;
            }
            if let Some(cell) = buf.cell_mut(Position::new(sx, sy)) {
                cell.set_bg(palette::SHADOW_BG);
                cell.set_fg(palette::SHADOW_FG);
            }
        }

        for offset in 0..2 {
            for idx in 0..height {
                let sx = canvas.x.saturating_add(canvas.width).saturating_add(offset);
                let sy = canvas.y.saturating_add(idx).saturating_add(1);
                if sx > max_x || sy > max_y {
                    continue;
                }
                if let Some(cell) = buf.cell_mut(Position::new(sx, sy)) {
                    cell.set_bg(palette::SHADOW_BG);
                    cell.set_fg(palette::SHADOW_FG);
                }
            }
        }
    }
}

fn text_lines(text: &str) -> u16 {
    let count = text.lines().count();
    if count == 0 {
        0
    } else {
        count as u16
    }
}
