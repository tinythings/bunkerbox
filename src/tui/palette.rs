use ratatui::style::Color;

// ---------------------------------------------------------------------------
// Industrial / bunker palette — cold steel, amber warning lights, radar cyan
// ---------------------------------------------------------------------------

// ---- Neutral foundation ----

pub const BG_0: Color = Color::Indexed(232);
pub const BG_1: Color = Color::Indexed(233);
pub const BG_2: Color = Color::Indexed(235);
pub const BG_3: Color = Color::Indexed(237);

pub const GRAY_0: Color = Color::Indexed(239);
pub const GRAY_1: Color = Color::Indexed(244);
pub const GRAY_2: Color = Color::Indexed(249);

// ---- Semantic aliases ----

pub const SURFACE: Color = Color::Indexed(236);
pub const FG: Color = Color::Indexed(253);
pub const MUTED: Color = Color::Indexed(243);
pub const FAINT: Color = Color::Indexed(238);
pub const BORDER: Color = Color::Indexed(240);

pub const ACCENT: Color = Color::Indexed(208);
pub const PROCESSING: Color = Color::Indexed(39);
pub const SUCCESS: Color = Color::Indexed(42);
pub const ERROR: Color = Color::Indexed(197);
pub const WARNING: Color = Color::Indexed(178);
pub const HIGHLIGHT: Color = Color::Indexed(133);

pub const POPUP_BG: Color = SURFACE;

// ---- MS-DOS shadow colours ----

pub const SHADOW_BG: Color = Color::Black;
pub const SHADOW_FG: Color = SURFACE;

// ---- Absolute contrast ----

pub const WHITE: Color = Color::White;
pub const BLACK: Color = Color::Black;
