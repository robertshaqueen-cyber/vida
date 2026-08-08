//! Vida's lightweight visual system.
//!
//! The application intentionally keeps the styling in iced primitives: a
//! single palette, one small icon font, and stateless style functions. This
//! keeps the UI coherent without adding a web runtime or an SVG renderer.

use iced::widget::{button, container, pick_list, text, text_input};
use iced::{Background, Border, Color, Shadow, Theme, Vector};

pub const BG_APP: Color = Color::from_rgb(0.058, 0.082, 0.078);
pub const BG_CHROME: Color = Color::from_rgb(0.047, 0.071, 0.067);
pub const BG_SIDEBAR: Color = Color::from_rgb(0.055, 0.079, 0.074);
pub const BG_SURFACE: Color = Color::from_rgb(0.105, 0.137, 0.129);
pub const BG_SURFACE_HOVER: Color = Color::from_rgb(0.126, 0.169, 0.158);
pub const BG_INPUT: Color = Color::from_rgb(0.077, 0.105, 0.099);
pub const ACCENT: Color = Color::from_rgb(0.106, 0.690, 0.655);
pub const ACCENT_HOVER: Color = Color::from_rgb(0.135, 0.769, 0.725);
pub const ACCENT_MUTED: Color = Color::from_rgb(0.071, 0.220, 0.204);
pub const TEXT_PRIMARY: Color = Color::from_rgb(0.875, 0.910, 0.898);
pub const TEXT_SECONDARY: Color = Color::from_rgb(0.585, 0.635, 0.618);
pub const TEXT_MUTED: Color = Color::from_rgb(0.365, 0.420, 0.402);
pub const BORDER: Color = Color::from_rgb(0.145, 0.200, 0.188);
pub const DANGER: Color = Color::from_rgb(0.914, 0.337, 0.357);
pub const DANGER_TEXT: Color = Color::from_rgb(0.945, 0.665, 0.680);
pub const WARNING: Color = Color::from_rgb(0.930, 0.690, 0.200);
pub const SUCCESS: Color = Color::from_rgb(0.245, 0.780, 0.530);

pub const RADIUS_SM: f32 = 6.0;
pub const RADIUS_MD: f32 = 10.0;

pub fn theme() -> Theme {
    Theme::custom(
        "Vida Dark",
        iced::theme::Palette {
            background: BG_APP,
            text: TEXT_PRIMARY,
            primary: ACCENT,
            success: SUCCESS,
            danger: DANGER,
            warning: WARNING,
        },
    )
}

pub fn app_background(_: &Theme) -> container::Style {
    container::Style::default()
        .background(BG_APP)
        .color(TEXT_PRIMARY)
}

pub fn chrome(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_CHROME)),
        text_color: Some(TEXT_PRIMARY),
        border: Border {
            color: ACCENT_MUTED,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

pub fn sidebar(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_SIDEBAR)),
        text_color: Some(TEXT_PRIMARY),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

pub fn surface(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_SURFACE)),
        text_color: Some(TEXT_PRIMARY),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: RADIUS_MD.into(),
        },
        ..container::Style::default()
    }
}

pub fn tab_surface(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: if active {
            Some(Background::Color(BG_SURFACE))
        } else {
            None
        },
        text_color: Some(if active { TEXT_PRIMARY } else { TEXT_SECONDARY }),
        border: Border {
            color: if active {
                ACCENT_MUTED
            } else {
                Color::TRANSPARENT
            },
            width: if active { 1.0 } else { 0.0 },
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

pub fn accent_badge(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(ACCENT_MUTED)),
        text_color: Some(ACCENT),
        border: Border {
            color: ACCENT.scale_alpha(0.35),
            width: 1.0,
            radius: RADIUS_MD.into(),
        },
        ..container::Style::default()
    }
}

pub fn error_notice(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(DANGER.scale_alpha(0.10))),
        text_color: Some(DANGER_TEXT),
        border: Border {
            color: DANGER.scale_alpha(0.28),
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

pub fn success_notice(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SUCCESS.scale_alpha(0.08))),
        text_color: Some(TEXT_PRIMARY),
        border: Border {
            color: SUCCESS.scale_alpha(0.24),
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

pub fn input_container(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_INPUT)),
        text_color: Some(TEXT_PRIMARY),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }
}

pub fn elevated(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_SURFACE)),
        text_color: Some(TEXT_PRIMARY),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 12.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.28),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    }
}

pub fn tab(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: if hovered {
                Some(Background::Color(BG_SURFACE_HOVER))
            } else {
                None
            },
            text_color: if active { TEXT_PRIMARY } else { TEXT_SECONDARY },
            border: Border {
                radius: RADIUS_SM.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn icon_button(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: if active || hovered {
                Some(Background::Color(if active {
                    ACCENT_MUTED
                } else {
                    BG_SURFACE_HOVER
                }))
            } else {
                None
            },
            text_color: if active { ACCENT } else { TEXT_SECONDARY },
            border: Border {
                radius: RADIUS_SM.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn nav_item(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: if active || hovered {
                Some(Background::Color(if active {
                    ACCENT_MUTED
                } else {
                    BG_SURFACE
                }))
            } else {
                None
            },
            text_color: if active { ACCENT } else { TEXT_SECONDARY },
            border: Border {
                radius: RADIUS_MD.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn primary_button(_: &Theme, status: button::Status) -> button::Style {
    let disabled = matches!(status, button::Status::Disabled);
    let color = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        ACCENT_HOVER
    } else {
        ACCENT
    };
    button::Style {
        background: Some(Background::Color(color.scale_alpha(if disabled {
            0.35
        } else {
            1.0
        }))),
        text_color: BG_CHROME.scale_alpha(if disabled { 0.55 } else { 1.0 }),
        border: Border {
            radius: RADIUS_SM.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

pub fn secondary_button(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(Background::Color(if hovered {
            BG_SURFACE_HOVER
        } else {
            BG_INPUT
        })),
        text_color: TEXT_PRIMARY,
        border: Border {
            color: if hovered { ACCENT_MUTED } else { BORDER },
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..button::Style::default()
    }
}

pub fn danger_button(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(Background::Color(if hovered {
            Color::from_rgb(0.245, 0.105, 0.110)
        } else {
            Color::from_rgb(0.170, 0.088, 0.092)
        })),
        text_color: DANGER,
        border: Border {
            color: DANGER.scale_alpha(0.55),
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..button::Style::default()
    }
}

pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    let hovered = matches!(status, text_input::Status::Hovered);
    text_input::Style {
        background: Background::Color(BG_INPUT),
        border: Border {
            color: if focused {
                ACCENT
            } else if hovered {
                TEXT_MUTED
            } else {
                BORDER
            },
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        icon: TEXT_SECONDARY,
        placeholder: TEXT_MUTED,
        value: TEXT_PRIMARY,
        selection: ACCENT_MUTED,
    }
}

pub fn picker(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let emphasized = matches!(
        status,
        pick_list::Status::Hovered | pick_list::Status::Opened { .. }
    );
    pick_list::Style {
        text_color: TEXT_PRIMARY,
        placeholder_color: TEXT_MUTED,
        handle_color: TEXT_SECONDARY,
        background: Background::Color(BG_INPUT),
        border: Border {
            color: if emphasized { ACCENT } else { BORDER },
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
    }
}

pub fn muted<'a>(value: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(value).color(TEXT_SECONDARY)
}

pub mod icons {
    use iced::Font;
    use iced::widget::{Text, text};

    pub const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/Lucide.ttf");
    pub const FONT: Font = Font::with_name("lucide");

    pub const CIRCLE_ALERT: &str = "\u{e07b}";
    pub const CIRCLE_PLUS: &str = "\u{e086}";
    pub const CLOUD: &str = "\u{e08c}";
    pub const COPY: &str = "\u{e0a2}";
    pub const DATABASE: &str = "\u{e0b1}";
    pub const EYE: &str = "\u{e0be}";
    pub const FOLDER: &str = "\u{e0db}";
    pub const HOUSE: &str = "\u{e0f9}";
    pub const LANGUAGES: &str = "\u{e104}";
    pub const LOCK: &str = "\u{e10f}";
    pub const PANEL_LEFT: &str = "\u{e12e}";
    pub const PLUS: &str = "\u{e141}";
    pub const REFRESH: &str = "\u{e149}";
    pub const SERVER: &str = "\u{e157}";
    pub const SETTINGS: &str = "\u{e158}";
    pub const SHIELD: &str = "\u{e15c}";
    pub const TERMINAL: &str = "\u{e185}";
    pub const TRASH: &str = "\u{e18e}";
    pub const X: &str = "\u{e1b2}";
    pub const HISTORY: &str = "\u{e1f5}";
    pub const PENCIL: &str = "\u{e1f9}";
    pub const CIRCLE_CHECK: &str = "\u{e226}";
    pub const KEY: &str = "\u{e4a8}";

    pub fn icon(glyph: &'static str, size: u32) -> Text<'static> {
        text(glyph).font(FONT).size(size)
    }
}
