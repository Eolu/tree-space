//! Desktop-theme integration for `[theme] mode = "system"`.
//!
//! In the default `custom` mode tree-space reads its `main.css`. In `system`
//! mode it instead builds the palette from the desktop theme and reuses the
//! theme-independent structure of the shipped stylesheet, so `main.css` is
//! neither required nor created.
//!
//! Only Omarchy is implemented today. Its active theme lives at
//! `$XDG_STATE_HOME/omarchy/current/theme` (`~/.local/state/omarchy/current/theme`
//! by default) and carries the color palette in `colors.toml`; the UI font
//! comes from fontconfig, the same source `omarchy font current` uses.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::config::{
    DEFAULT_STYLESHEET, LoadProblem, StyleSource, Stylesheet, SystemTheme, ThemeConfig, ThemeMode,
    default_structure,
};
use crate::highlight;

/// The Omarchy theme currently applied, as read from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct OmarchyTheme {
    /// The theme's name (from `theme.name`), for diagnostics.
    pub name: String,
    pub colors: Colors,
    /// Omarchy's monospace family, or `None` when fontconfig isn't available.
    pub font_family: Option<String>,
}

/// The subset of Omarchy's `colors.toml` that tree-space's palette maps onto.
///
/// Stock and generated Omarchy themes always include every key; a theme that
/// omits one falls back to the shipped default (see [`omarchy_stylesheet`]).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Colors {
    pub accent: String,
    pub selection: String,
    pub muted: String,
    pub background: String,
    pub dark_background: String,
    pub darker_background: String,
    pub lighter_background: String,
    pub foreground: String,
    pub dark_foreground: String,
    pub bright_foreground: String,
    pub red: String,
    pub green: String,
    // Used only for syntax highlighting; a theme that omits one falls back to
    // the built-in palette for that class.
    #[serde(default)]
    pub blue: String,
    #[serde(default)]
    pub cyan: String,
    #[serde(default)]
    pub yellow: String,
    #[serde(default)]
    pub orange: String,
    #[serde(default)]
    pub magenta: String,
}

impl Colors {
    /// The `@define-color` palette tree-space's structure consumes, in order.
    fn palette(&self) -> [(&'static str, &str); 13] {
        [
            ("tn_bg", &self.background),
            ("tn_bg_dark", &self.dark_background),
            ("tn_bg_darker", &self.darker_background),
            ("tn_bg_light", &self.lighter_background),
            ("tn_fg", &self.foreground),
            ("tn_fg_bright", &self.bright_foreground),
            ("tn_fg_dim", &self.dark_foreground),
            ("tn_muted", &self.muted),
            ("tn_selection", &self.selection),
            // The panel's highlights follow the theme's accent.
            ("tn_blue", &self.accent),
            ("tn_accent", &self.accent),
            ("tn_green", &self.green),
            ("tn_red", &self.red),
        ]
    }
}

/// Build the stylesheet for Omarchy's active theme, falling back to the shipped
/// default when no theme can be read.
pub fn omarchy_stylesheet() -> Stylesheet {
    match load_omarchy_theme() {
        Some(theme) => Stylesheet {
            css: render(&theme),
            source: StyleSource::System,
            problem: None,
        },
        None => Stylesheet {
            css: DEFAULT_STYLESHEET.to_owned(),
            source: StyleSource::Builtin,
            problem: Some(LoadProblem::SystemTheme(
                "no active Omarchy theme found; using the built-in default".to_owned(),
            )),
        },
    }
}

/// The syntax-highlighting palette for the configured theme. Custom `main.css`
/// mode is assumed dark (the shipped default); Omarchy mode maps the active
/// theme's colors so highlighting matches the panel. Falls back to the dark
/// default when no theme can be read.
pub fn syntax_palette(theme: &ThemeConfig) -> highlight::Palette {
    match theme.mode {
        ThemeMode::Custom => highlight::Palette::dark(),
        ThemeMode::System => match theme.system {
            SystemTheme::Omarchy => load_omarchy_theme()
                .map(|theme| palette_from(&theme.colors))
                .unwrap_or_else(highlight::Palette::dark),
        },
    }
}

/// Map a theme's `colors.toml` onto the syntax token classes. Missing optional
/// colors fall back to the built-in dark palette.
fn palette_from(colors: &Colors) -> highlight::Palette {
    let fallback = highlight::Palette::dark();
    let pick = |value: &str, default: &str| {
        if value.is_empty() { default.to_owned() } else { value.to_owned() }
    };
    highlight::Palette {
        keyword: pick(&colors.accent, &fallback.keyword),
        type_: pick(&colors.cyan, &fallback.type_),
        function: pick(&colors.blue, &fallback.function),
        string: pick(&colors.green, &fallback.string),
        comment: pick(&colors.dark_foreground, &fallback.comment),
        number: pick(&colors.orange, &fallback.number),
        constant: pick(&colors.magenta, &fallback.constant),
        preprocessor: pick(&colors.yellow, &fallback.preprocessor),
        tag: pick(&colors.red, &fallback.tag),
        attribute: pick(&colors.magenta, &fallback.attribute),
        heading: pick(&colors.accent, &fallback.heading),
        link: pick(&colors.cyan, &fallback.link),
    }
}

/// Read the active Omarchy theme, or `None` when Omarchy isn't installed or its
/// state directory is unreadable.
pub fn load_omarchy_theme() -> Option<OmarchyTheme> {
    let dir = current_theme_dir()?;
    load_omarchy_theme_from(&dir)
}

/// Read an Omarchy theme from an explicit directory (tests). The font is looked
/// up through fontconfig.
pub fn load_omarchy_theme_from(dir: &Path) -> Option<OmarchyTheme> {
    let raw = std::fs::read_to_string(dir.join("colors.toml")).ok()?;
    let colors: Colors = toml::from_str(&raw).ok()?;
    let name = std::fs::read_to_string(dir.join("theme.name"))
        .map(|name| name.trim().to_owned())
        .unwrap_or_default();
    Some(OmarchyTheme { name, colors, font_family: omarchy_font_family() })
}

/// Render a full stylesheet from an Omarchy theme: the theme's palette, then the
/// theme-independent structure, then a font override so the panel follows the
/// desktop font.
pub fn render(theme: &OmarchyTheme) -> String {
    let mut css = String::from(
        "/* Generated from the active Omarchy theme; main.css is not read in system mode. */\n",
    );
    for (name, value) in theme.colors.palette() {
        let _ = writeln!(css, "@define-color {name} {value};");
    }
    css.push_str(default_structure());
    if let Some(family) = &theme.font_family {
        let _ = write!(
            css,
            "\n.panel, .bookmark-editor-root, .props {{ font-family: \"{family}\"; }}\n"
        );
    }
    css
}

/// `$XDG_STATE_HOME/omarchy/current/theme`, or the `~/.local/state` fallback.
fn current_theme_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("omarchy").join("current").join("theme"))
}

/// Omarchy's monospace family from fontconfig — the same source
/// `omarchy font current` reads. `fc-match` returns a comma-separated alias
/// list, so take the first entry.
fn omarchy_font_family() -> Option<String> {
    let output = Command::new("fc-match")
        .args(["monospace", "-f", "%{family}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let families = String::from_utf8(output.stdout).ok()?;
    let first = families.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
mode = "dark"

accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"
background = "#1a1b26"
dark_background = "#13141c"
darker_background = "#0e0e14"
lighter_background = "#24283b"
foreground = "#a9b1d6"
dark_foreground = "#565f89"
bright_foreground = "#c0caf5"
red = "#f7768e"
green = "#9ece6a"
blue = "#7aa2f7"
"##;

    fn sample_colors() -> Colors {
        toml::from_str(SAMPLE).unwrap()
    }

    #[test]
    fn colors_parse_from_colors_toml() {
        let colors = sample_colors();
        assert_eq!(colors.background, "#1a1b26");
        assert_eq!(colors.accent, "#7aa2f7");
        assert_eq!(colors.green, "#9ece6a");
    }

    #[test]
    fn render_defines_the_palette_and_reuses_the_structure() {
        let theme = OmarchyTheme {
            name: "tokyo-night".to_owned(),
            colors: sample_colors(),
            font_family: Some("JetBrainsMono Nerd Font".to_owned()),
        };
        let css = render(&theme);
        // Palette values reach the CSS.
        assert!(css.contains("@define-color tn_bg #1a1b26;"));
        assert!(css.contains("@define-color tn_accent #7aa2f7;"));
        assert!(css.contains("@define-color tn_green #9ece6a;"));
        assert!(css.contains("@define-color tn_red #f7768e;"));
        // Highlights follow the theme accent, not a fixed blue.
        assert!(css.contains("@define-color tn_blue #7aa2f7;"));
        // The structural rules are reused and never re-declare the palette.
        assert!(css.contains(".tree-row {"));
        let structure = css.split_once(".tree-row {").unwrap().1;
        assert!(!structure.contains("@define-color tn_bg"));
        // The desktop font overrides the one baked into the structure.
        assert!(css.contains(
            ".panel, .bookmark-editor-root, .props { font-family: \"JetBrainsMono Nerd Font\"; }"
        ));
    }

    #[test]
    fn render_omits_the_font_override_without_a_family() {
        let without = render(&OmarchyTheme {
            name: String::new(),
            colors: sample_colors(),
            font_family: None,
        });
        // The combined override selector is absent; the structure's own
        // per-widget font rules remain.
        assert!(!without.contains(".panel, .bookmark-editor-root, .props {"));
    }

    #[test]
    fn missing_colors_toml_is_not_a_theme() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_omarchy_theme_from(dir.path()).is_none());
    }

    #[test]
    fn theme_loads_from_an_explicit_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("colors.toml"), SAMPLE).unwrap();
        std::fs::write(dir.path().join("theme.name"), "tokyo-night\n").unwrap();
        let theme = load_omarchy_theme_from(dir.path()).unwrap();
        assert_eq!(theme.name, "tokyo-night");
        assert_eq!(theme.colors.background, "#1a1b26");
    }
}
