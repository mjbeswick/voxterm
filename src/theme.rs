//! Colour themes. Palettes follow btop's built-in themes (Default, Dracula, Nord, Gruvbox, Tokyo Night, Solarized).

use ratatui::style::Color;

#[derive(Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub fg: Color,
    pub dim: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub good: Color,
    pub warn: Color,
    pub bad: Color,
    pub special: Color,
    pub on_accent: Color,
    pub header_bg: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const THEMES: [Theme; 6] = [
    Theme {
        name: "default",
        fg: rgb(0xe6e6e6),
        dim: rgb(0x9a9a9a),
        muted: rgb(0x6c6c6c),
        border: rgb(0x4a4a4a),
        accent: rgb(0x00d7ff),
        good: rgb(0x00ff87),
        warn: rgb(0xffd75f),
        bad: rgb(0xff5f5f),
        special: rgb(0xd787ff),
        on_accent: rgb(0x000000),
        header_bg: rgb(0x1c1c1c),
    },
    Theme {
        name: "dracula",
        fg: rgb(0xf8f8f2),
        dim: rgb(0xbfc7d5),
        muted: rgb(0x6272a4),
        border: rgb(0x44475a),
        accent: rgb(0xbd93f9),
        good: rgb(0x50fa7b),
        warn: rgb(0xf1fa8c),
        bad: rgb(0xff5555),
        special: rgb(0xff79c6),
        on_accent: rgb(0x282a36),
        header_bg: rgb(0x21222c),
    },
    Theme {
        name: "nord",
        fg: rgb(0xd8dee9),
        dim: rgb(0xb8c2d0),
        muted: rgb(0x4c566a),
        border: rgb(0x3b4252),
        accent: rgb(0x88c0d0),
        good: rgb(0xa3be8c),
        warn: rgb(0xebcb8b),
        bad: rgb(0xbf616a),
        special: rgb(0xb48ead),
        on_accent: rgb(0x2e3440),
        header_bg: rgb(0x2e3440),
    },
    Theme {
        name: "gruvbox",
        fg: rgb(0xebdbb2),
        dim: rgb(0xa89984),
        muted: rgb(0x7c6f64),
        border: rgb(0x504945),
        accent: rgb(0x83a598),
        good: rgb(0xb8bb26),
        warn: rgb(0xfabd2f),
        bad: rgb(0xfb4934),
        special: rgb(0xd3869b),
        on_accent: rgb(0x282828),
        header_bg: rgb(0x1d2021),
    },
    Theme {
        name: "tokyo-night",
        fg: rgb(0xc0caf5),
        dim: rgb(0x9aa5ce),
        muted: rgb(0x565f89),
        border: rgb(0x292e42),
        accent: rgb(0x7aa2f7),
        good: rgb(0x9ece6a),
        warn: rgb(0xe0af68),
        bad: rgb(0xf7768e),
        special: rgb(0xbb9af7),
        on_accent: rgb(0x1a1b26),
        header_bg: rgb(0x16161e),
    },
    Theme {
        name: "solarized",
        fg: rgb(0x93a1a1),
        dim: rgb(0x839496),
        muted: rgb(0x586e75),
        border: rgb(0x073642),
        accent: rgb(0x2aa198),
        good: rgb(0x859900),
        warn: rgb(0xb58900),
        bad: rgb(0xdc322f),
        special: rgb(0x6c71c4),
        on_accent: rgb(0x002b36),
        header_bg: rgb(0x002b36),
    },
];

pub fn by_name(name: &str) -> Option<usize> {
    THEMES.iter().position(|t| t.name == name)
}
