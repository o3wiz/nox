use gpui::{App, Font, FontFallbacks, Hsla, Pixels, SharedString, font, px, rgb, rgb_to_hsla};
use nox::config::{self, Command, Config};
use std::collections::HashMap;

const UI_FONT_PREFERENCES: &[&str] = &[
    "SF Pro Text",
    "Inter",
    "IBM Plex Sans",
    "SF Pro Display",
    ".AppleSystemUIFont",
    ".SF NS",
    "Segoe UI",
    "Helvetica Neue",
    "Arial",
];

const MONO_FONT_PREFERENCES: &[&str] = &[
    "JetBrains Mono",
    "SauceCodePro Nerd Font Mono",
    "SauceCodePro NFM",
    "Fira Code",
    "IBM Plex Mono",
    "Cascadia Code",
    "Menlo",
    "Consolas",
    "DejaVu Sans Mono",
];

const SYMBOL_FONT_FALLBACKS: &[&str] = &[
    "Apple Symbols",
    "Lucida Grande",
    "Menlo",
    "Segoe UI Symbol",
    "DejaVu Sans",
    "Noto Sans Symbols 2",
    "Noto Sans Symbols",
];

/// Consistent icon sizes (in logical pixels) matching the 4/8px design rhythm.
pub const ICON_XS: Pixels = px(13.0);
pub const ICON_SM: Pixels = px(15.0);
pub const ICON_MD: Pixels = px(17.0);
pub const ICON_HERO: Pixels = px(32.0);

/// Resolved design tokens derived from `Config`.
#[derive(Clone)]
pub struct Theme {
    pub ui_font: SharedString,
    pub ui_fallbacks: FontFallbacks,
    pub ui_size: Pixels,
    pub small_size: Pixels,
    pub xs_size: Pixels,
    pub mono_font: SharedString,
    pub mono_fallbacks: FontFallbacks,
    pub mono_size: Pixels,

    pub background: Hsla,
    pub panel: Hsla,
    pub elevated: Hsla,
    pub hover: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub on_accent: Hsla,
    pub accent_subtle: Hsla,
    pub accent_border: Hsla,
    pub selection: Hsla,
    pub selection_soft: Hsla,
    pub border: Hsla,
    pub danger: Hsla,
    pub danger_subtle: Hsla,
    pub warning: Hsla,

    pub shortcuts: HashMap<Command, SharedString>,
}

impl Theme {
    pub fn from_config(config: &Config, cx: &App) -> Self {
        let installed = cx.text_system().all_font_names();
        Self::from_config_with_fonts(config, &installed)
    }

    pub fn from_config_with_fonts(config: &Config, installed_fonts: &[String]) -> Self {
        let a = &config.appearance;
        let bg_hex = config::color(&a.background).unwrap();
        let panel_hex = config::color(&a.panel).unwrap();
        let fg_hex = config::color(&a.foreground).unwrap();
        let muted_hex = config::color(&a.muted).unwrap();
        let accent_hex = config::color(&a.accent).unwrap();
        let selection_hex = config::color(&a.selection).unwrap();
        let border_hex = config::color(&a.border).unwrap();
        let danger_hex = 0xF38BA8;
        let warning_hex = 0xF9D58B;

        let background = rgb_to_hsla(rgb(bg_hex));
        let panel = rgb_to_hsla(rgb(panel_hex));
        let foreground = rgb_to_hsla(rgb(fg_hex));
        let muted = rgb_to_hsla(rgb(muted_hex));
        let accent = rgb_to_hsla(rgb(accent_hex));
        let selection = rgb_to_hsla(rgb(selection_hex));
        let border = rgb_to_hsla(rgb(border_hex));

        // Derive elevation, hover, and semantic tints cleanly from user tokens in RGB space.
        let elevated = mix_hex(panel_hex, fg_hex, 0.045);
        let hover = mix_hex(panel_hex, fg_hex, 0.09);
        let selection_soft = mix_hex(bg_hex, selection_hex, 0.45);
        let accent_subtle = mix_hex(panel_hex, accent_hex, 0.16);
        let accent_border = mix_hex(border_hex, accent_hex, 0.45);
        let on_accent = contrast_ink(accent_hex);

        let danger = rgb_to_hsla(rgb(danger_hex));
        let danger_subtle = mix_hex(panel_hex, danger_hex, 0.14);
        let warning = rgb_to_hsla(rgb(warning_hex));

        let ui_font = resolve_font(&a.ui_font_family, UI_FONT_PREFERENCES, installed_fonts);
        let mono_font = resolve_font(&a.font_family, MONO_FONT_PREFERENCES, installed_fonts);

        let ui_fallbacks = build_fallbacks(&ui_font, UI_FONT_PREFERENCES, installed_fonts);
        let mono_fallbacks = build_fallbacks(&mono_font, MONO_FONT_PREFERENCES, installed_fonts);

        let mut shortcuts = HashMap::new();
        for binding in &config.keyboard.bindings {
            shortcuts
                .entry(binding.action)
                .or_insert_with(|| SharedString::from(format_shortcut(&binding.keys)));
        }

        Self {
            ui_font: SharedString::from(ui_font),
            ui_fallbacks,
            ui_size: px(a.ui_font_size),
            small_size: px((a.ui_font_size - 1.5).max(10.5)),
            xs_size: px((a.ui_font_size - 2.0).max(10.0)),
            mono_font: SharedString::from(mono_font),
            mono_fallbacks,
            mono_size: px(a.font_size),
            background,
            panel,
            elevated,
            hover,
            foreground,
            muted,
            accent,
            on_accent,
            accent_subtle,
            accent_border,
            selection,
            selection_soft,
            border,
            danger,
            danger_subtle,
            warning,
            shortcuts,
        }
    }

    pub fn ui_font_spec(&self) -> Font {
        let mut spec = font(self.ui_font.clone());
        spec.fallbacks = Some(self.ui_fallbacks.clone());
        spec
    }

    pub fn mono_font_spec(&self) -> Font {
        let mut spec = font(self.mono_font.clone());
        spec.fallbacks = Some(self.mono_fallbacks.clone());
        spec
    }

    pub fn shortcut(&self, command: Command) -> Option<SharedString> {
        self.shortcuts.get(&command).cloned()
    }
}

/// Resolves a configured font family against installed system fonts.
/// Generic placeholders (`monospace`, `sans-serif`, `system`) or unavailable fonts
/// gracefully promote to the highest-craft installed font in `preferences`.
pub fn resolve_font(configured: &str, preferences: &[&str], installed: &[String]) -> String {
    let trimmed = configured.trim();
    let is_generic = trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("monospace")
        || trimmed.eq_ignore_ascii_case("sans-serif")
        || trimmed.eq_ignore_ascii_case("system")
        || trimmed.eq_ignore_ascii_case("auto");

    if !is_generic {
        if let Some(exact) = find_installed(trimmed, installed) {
            return exact;
        }
        // In headless tests or platforms without font enumeration, keep explicit config.
        if installed.is_empty() {
            return trimmed.to_owned();
        }
    }

    for candidate in preferences {
        if let Some(found) = find_installed(candidate, installed) {
            return found;
        }
    }

    if trimmed.is_empty() {
        preferences[0].to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn find_installed(name: &str, installed: &[String]) -> Option<String> {
    installed
        .iter()
        .find(|font| font.eq_ignore_ascii_case(name))
        .cloned()
}

fn build_fallbacks(primary: &str, preferences: &[&str], installed: &[String]) -> FontFallbacks {
    let mut list: Vec<String> = Vec::new();
    for candidate in preferences.iter().chain(SYMBOL_FONT_FALLBACKS.iter()) {
        if candidate.eq_ignore_ascii_case(primary)
            || list
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(candidate))
        {
            continue;
        }
        if let Some(found) = find_installed(candidate, installed) {
            list.push(found);
        }
    }
    FontFallbacks::from_fonts(list)
}

fn mix_hex(base: u32, over: u32, alpha: f32) -> Hsla {
    let blend = |b: u32, o: u32| -> u32 {
        let mixed = (b as f32 * (1.0 - alpha) + o as f32 * alpha).round();
        mixed.clamp(0.0, 255.0) as u32
    };
    let r = blend((base >> 16) & 0xFF, (over >> 16) & 0xFF);
    let g = blend((base >> 8) & 0xFF, (over >> 8) & 0xFF);
    let b = blend(base & 0xFF, over & 0xFF);
    rgb_to_hsla(rgb((r << 16) | (g << 8) | b))
}

/// Picks dark or light ink with strong WCAG contrast on top of an accent fill.
fn contrast_ink(hex: u32) -> Hsla {
    let r = ((hex >> 16) & 0xFF) as f32 / 255.0;
    let g = ((hex >> 8) & 0xFF) as f32 / 255.0;
    let b = (hex & 0xFF) as f32 / 255.0;
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    if luminance > 0.45 {
        rgb_to_hsla(rgb(0x0B1016))
    } else {
        rgb_to_hsla(rgb(0xF8FAFC))
    }
}

/// Converts a GPUI keybinding sequence (e.g. `secondary-o`, `shift-f3`) into a concise keycap label.
pub fn format_shortcut(keys: &str) -> String {
    let mac = cfg!(target_os = "macos");
    keys.split_whitespace()
        .map(|stroke| {
            let parts: Vec<&str> = stroke.split('-').collect();
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                let token = match part.to_ascii_lowercase().as_str() {
                    "secondary" | "cmd" => {
                        if mac {
                            "⌘".to_owned()
                        } else {
                            "Ctrl".to_owned()
                        }
                    }
                    "ctrl" | "control" => {
                        if mac {
                            "⌃".to_owned()
                        } else {
                            "Ctrl".to_owned()
                        }
                    }
                    "shift" => {
                        if mac {
                            "⇧".to_owned()
                        } else {
                            "Shift".to_owned()
                        }
                    }
                    "alt" | "opt" => {
                        if mac {
                            "⌥".to_owned()
                        } else {
                            "Alt".to_owned()
                        }
                    }
                    "enter" => "↵".to_owned(),
                    "escape" => "Esc".to_owned(),
                    "tab" => "⇥".to_owned(),
                    "up" => "↑".to_owned(),
                    "down" => "↓".to_owned(),
                    "pageup" => "PgUp".to_owned(),
                    "pagedown" => "PgDn".to_owned(),
                    other => other.to_ascii_uppercase(),
                };
                out.push(token);
            }
            if mac { out.join("") } else { out.join("+") }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
