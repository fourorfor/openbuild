//! Import external colour themes (opencode TUI JSON, opencode desktop JSON,
//! VSCode `.json`) into the pager's [`Theme`] shape.
//!
//! The pager's theme engine is a fixed set of compile-time palettes with no
//! runtime-loaded path, so importing cannot mean "teach the renderer a new
//! format". Instead the importer **converts** a foreign document into the
//! `Theme` field set, and [`crate::theme::custom`] installs the result as an
//! override on top of the selected built-in kind.
//!
//! Three dialects are understood:
//!
//! - **opencode TUI** — `{ "defs", "theme": { … } }` with ~46 semantic tokens
//!   (`packages/theme/src/tui/v1.ts`).
//! - **opencode desktop** — `{ "light": { "palette" }, "dark": { … } }`
//!   (`packages/ui/src/theme/desktop-theme.schema.json`).
//! - **VSCode** — `{ "colors": { "editor.background": … } }`. `tokenColors` are
//!   ignored: the pager highlights with `.tmTheme` assets, not these tokens.
//!
//! A fourth shape is read back for round-tripping: the **stored** form written
//! by [`save`], which puts a resolved `palette` at the root. Imported themes are
//! converted once and then read from our own format, so the mapping never has to
//! be re-derived (and a source theme becoming unreachable cannot break them).
//!
//! Alpha is the subtle part. VSCode writes `#RRGGBBAA` and the pager's fields
//! are opaque `Color::Rgb`, so translucent colours are **composited onto the
//! background**. Dropping alpha instead turns a 10% overlay into a saturated
//! colour, which is how imported diff bands end up looking like error banners.

use std::path::PathBuf;

use ratatui::style::{Color, Modifier};
use serde_json::Value;

use crate::theme::Theme;

/// Directory under the grok home holding imported themes.
pub const THEMES_DIR: &str = "themes";

/// A parsed and mapped theme document.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedTheme {
    /// Slug used as the file name; always `[a-z0-9-]+`.
    pub slug: String,
    /// Human-readable name, shown by `/theme import` and stored in the file.
    pub name: String,
    /// The resolved palette.
    pub theme: Theme,
    /// Which input dialect produced it, for the status message.
    pub source: ThemeSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    /// opencode TUI v1 (`theme.*` semantic tokens).
    OpenCodeV1,
    /// opencode desktop (`light`/`dark` + `palette`).
    OpenCodeDesktop,
    /// VSCode `colors` map.
    VSCode,
    /// A previously imported theme, re-read from its stored form.
    Stored,
}

impl ThemeSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenCodeV1 => "opencode theme",
            Self::OpenCodeDesktop => "opencode desktop theme",
            Self::VSCode => "VSCode theme",
            Self::Stored => "imported theme",
        }
    }
}

/// Failure carrying a message the TUI can show verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportError(pub String);

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ImportError {}

fn err(msg: impl Into<String>) -> ImportError {
    ImportError(msg.into())
}

/// Parse a theme document and map it onto [`Theme`].
///
/// `slug_hint` is the fallback identifier (a file stem); VSCode themes carry no
/// `id`, so without it they could not be addressed by `/theme`.
pub fn parse(text: &str, slug_hint: &str) -> Result<ImportedTheme, ImportError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| err(format!("not valid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| err("expected a JSON object at the top level"))?;

    if obj.contains_key("theme") {
        parse_opencode_v1(obj, slug_hint)
    } else if obj.contains_key("palette") {
        // Our own stored format (see `save`). Checked before the desktop shape
        // because both carry a `palette` key.
        parse_stored(obj, slug_hint)
    } else if obj.contains_key("light") || obj.contains_key("dark") {
        parse_opencode_desktop(obj, slug_hint)
    } else if obj.contains_key("colors") {
        parse_vscode(obj, slug_hint)
    } else {
        Err(err(
            "unrecognised theme: expected opencode `theme`, opencode desktop \
             `light`/`dark`, VSCode `colors`, or an OpenBuild `palette`",
        ))
    }
}

/// Read and parse a theme file from disk.
pub fn parse_file(path: &std::path::Path) -> Result<ImportedTheme, ImportError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| err(format!("cannot read {}: {e}", path.display())))?;
    let hint = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("imported");
    parse(&text, hint)
}

/// Persist an imported theme under the grok home, returning the written path.
pub fn save(theme: &ImportedTheme) -> Result<PathBuf, ImportError> {
    let dir = themes_dir().ok_or_else(|| err("cannot resolve the grok home directory"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| err(format!("cannot create {}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.json", theme.slug));
    let body = serde_json::to_string_pretty(&to_json(theme))
        .map_err(|e| err(format!("cannot serialise theme: {e}")))?;
    std::fs::write(&path, body).map_err(|e| err(format!("cannot write {}: {e}", path.display())))?;
    Ok(path)
}

/// `~/.grok/themes`, or `None` when no home resolves.
///
/// `OPENBUILD_THEME_DIR` overrides the location. Besides giving users a way to
/// keep themes outside the grok home, it is what makes the tests hermetic: the
/// enumeration must not depend on whatever a developer happens to have on disk.
pub fn themes_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OPENBUILD_THEME_DIR") {
        return Some(PathBuf::from(dir));
    }
    xai_dirs::user_grok_home().map(|home| home.join(THEMES_DIR))
}

/// Slugs of every theme file under [`themes_dir`], sorted.
pub fn list_imported() -> Vec<String> {
    let Some(dir) = themes_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            path.file_stem()?.to_str().map(str::to_owned)
        })
        .collect();
    out.sort();
    out
}

/// Load one imported theme by slug.
pub fn load_user_theme(slug: &str) -> Option<ImportedTheme> {
    let dir = themes_dir()?;
    parse_file(&dir.join(format!("{slug}.json"))).ok()
}

// -- document shapes ---------------------------------------------------------

/// Read the format [`save`] wrote.
///
/// The palette is already in `Theme`'s own vocabulary, so this is a plain
/// field-by-field read with no mapping and no defaults: a stored file is
/// generated, not hand-written, so a missing key means the file is corrupt.
/// `Theme` has no `Default`, so each field needs *some* fallback; `Reset` is
/// used, which renders as the terminal's own colour instead of a wrong one.
fn parse_stored(
    obj: &serde_json::Map<String, Value>,
    slug_hint: &str,
) -> Result<ImportedTheme, ImportError> {
    let palette = obj
        .get("palette")
        .and_then(Value::as_object)
        .ok_or_else(|| err("`palette` must be an object"))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(slug_hint)
        .to_string();
    let slug = slug_from(obj.get("id").and_then(Value::as_str), &name, slug_hint);

    let c = |key: &str| -> Color {
        palette
            .get(key)
            .and_then(Value::as_str)
            .and_then(parse_hex)
            .map(|h| h.color)
            .unwrap_or(Color::Reset)
    };
    let m = |key: &str| -> Modifier {
        // Only the six heading modifiers are stored, and only `BOLD` was ever
        // written; a custom file that omits them gets the same default.
        let _ = key;
        Modifier::BOLD
    };

    Ok(ImportedTheme {
        slug,
        name,
        theme: Theme {
            bg_base: c("bg_base"),
            bg_light: c("bg_light"),
            bg_dark: c("bg_dark"),
            bg_highlight: c("bg_highlight"),
            bg_hover: c("bg_hover"),
            bg_terminal: c("bg_base"),
            accent_user: c("accent_user"),
            accent_assistant: c("accent_assistant"),
            accent_thinking: c("accent_thinking"),
            accent_tool: c("accent_tool"),
            accent_system: c("accent_system"),
            accent_error: c("accent_error"),
            accent_success: c("accent_success"),
            accent_running: c("accent_running"),
            accent_skill: c("accent_skill"),
            text_primary: c("text_primary"),
            text_secondary: c("text_secondary"),
            gray_dim: c("gray_dim"),
            gray: c("gray"),
            gray_bright: c("gray_bright"),
            command: c("command"),
            path: c("path"),
            running: c("running"),
            warning: c("warning"),
            fuzzy_accent: c("fuzzy_accent"),
            accent_plan: c("accent_plan"),
            accent_verify: c("accent_verify"),
            accent_remember: c("accent_remember"),
            selection_border: c("selection_border"),
            hover_border: c("hover_border"),
            prompt_border: c("prompt_border"),
            prompt_border_active: c("prompt_border_active"),
            accent_model: c("accent_model"),
            scrollbar_bg: c("scrollbar_bg"),
            scrollbar_fg: c("scrollbar_fg"),
            diff_delete_bg: c("diff_delete_bg"),
            diff_delete_fg: c("diff_delete_fg"),
            diff_insert_bg: c("diff_insert_bg"),
            diff_insert_fg: c("diff_insert_fg"),
            diff_equal_fg: c("diff_equal_fg"),
            diff_gutter_fg: c("diff_gutter_fg"),
            bg_visual: c("bg_visual"),
            paste_bg: c("paste_bg"),
            paste_fg: c("paste_fg"),
            paste_dim: c("paste_dim"),
            md_heading_h1: c("md_heading_h1"),
            md_heading_h1_mod: m("md_heading_h1_mod"),
            md_heading_h2: c("md_heading_h2"),
            md_heading_h2_mod: m("md_heading_h2_mod"),
            md_heading_h3: c("md_heading_h3"),
            md_heading_h3_mod: m("md_heading_h3_mod"),
            md_heading_h4: c("md_heading_h4"),
            md_heading_h4_mod: m("md_heading_h4_mod"),
            md_heading_h5: c("md_heading_h5"),
            md_heading_h5_mod: m("md_heading_h5_mod"),
            md_heading_h6: c("md_heading_h6"),
            md_heading_h6_mod: m("md_heading_h6_mod"),
            md_code: c("md_code"),
            md_task_checked: c("md_task_checked"),
            md_task_unchecked: c("md_task_unchecked"),
            md_muted: c("md_muted"),
            md_code_bg: c("md_code_bg"),
            md_text: c("md_text"),
            link_fg: c("link_fg"),
        },
        source: ThemeSource::Stored,
    })
}

fn parse_opencode_v1(
    obj: &serde_json::Map<String, Value>,
    slug_hint: &str,
) -> Result<ImportedTheme, ImportError> {
    let raw_theme = obj
        .get("theme")
        .and_then(Value::as_object)
        .ok_or_else(|| err("`theme` must be an object"))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(slug_hint)
        .to_string();
    let slug = slug_from(obj.get("id").and_then(Value::as_str), &name, slug_hint);

    // `defs` aliases colours (`"primary": "blue"`); the v1 format allows one
    // level of indirection, which is all any shipped theme uses.
    let defs = obj.get("defs").and_then(Value::as_object);
    let token = |key: &str| -> Option<HexColor> {
        let raw = raw_theme.get(key)?;
        let text = match raw {
            // `{ "dark": …, "light": … }`: the pager is single-palette and its
            // default polarity is dark, so `dark` wins.
            Value::Object(map) => map.get("dark").and_then(Value::as_str)?,
            Value::String(s) => s.as_str(),
            _ => return None,
        };
        if text.starts_with('#') {
            return parse_hex(text);
        }
        parse_hex(defs?.get(text)?.as_str()?)
    };

    let bg = token("background").unwrap_or_else(|| opaque(20, 20, 20));
    // Everything else is composited over `bg`, since v1 themes may use alpha.
    let flat = |key: &str| token(key).map(|c| composite_over(c, bg));

    let panel = flat("backgroundPanel").unwrap_or(bg.color);
    let element = flat("backgroundElement").unwrap_or(panel);
    let text = flat("text").unwrap_or(Color::Rgb(225, 225, 225));
    let text_muted = flat("textMuted").unwrap_or_else(|| mute(text, 0.55));
    let primary = flat("primary").unwrap_or(Color::Rgb(122, 162, 247));
    let secondary = flat("secondary").unwrap_or(primary);
    let accent = flat("accent").unwrap_or(primary);
    let error = flat("error").unwrap_or(Color::Rgb(247, 118, 142));
    let warning = flat("warning").unwrap_or(Color::Rgb(224, 175, 104));
    let success = flat("success").unwrap_or(Color::Rgb(158, 206, 106));
    let info = flat("info").unwrap_or(primary);

    Ok(ImportedTheme {
        slug,
        name,
        theme: build_theme(PaletteInput {
            bg: bg.color,
            panel,
            element,
            text,
            text_muted,
            primary,
            secondary,
            accent,
            error,
            warning,
            success,
            info,
            border: flat("border").unwrap_or_else(|| mute(text, 0.25)),
            diff_added: flat("diffAdded").unwrap_or(success),
            diff_removed: flat("diffRemoved").unwrap_or(error),
            diff_context: flat("diffContext").unwrap_or(text_muted),
            diff_added_bg: flat("diffAddedBg"),
            diff_removed_bg: flat("diffRemovedBg"),
            md_text: flat("markdownText").unwrap_or(text),
            md_heading: flat("markdownHeading").unwrap_or(primary),
            md_link: flat("markdownLink").unwrap_or(info),
            md_code: flat("markdownCode").unwrap_or(success),
            md_muted: flat("markdownBlockQuote")
                .or_else(|| flat("markdownListItem"))
                .unwrap_or(text_muted),
            md_code_block: flat("markdownCodeBlock").unwrap_or(element),
        }),
        source: ThemeSource::OpenCodeV1,
    })
}

fn parse_opencode_desktop(
    obj: &serde_json::Map<String, Value>,
    slug_hint: &str,
) -> Result<ImportedTheme, ImportError> {
    // These documents ship both variants; `dark` matches the pager's default
    // polarity.
    let variant = obj
        .get("dark")
        .or_else(|| obj.get("light"))
        .and_then(Value::as_object)
        .ok_or_else(|| err("expected `dark` or `light` to be an object"))?;
    let palette = variant
        .get("palette")
        .and_then(Value::as_object)
        .ok_or_else(|| err("expected `palette` inside the variant"))?;
    let overrides = variant.get("overrides").and_then(Value::as_object);

    let pick = |key: &str| -> Option<Color> {
        let raw = palette.get(key).or_else(|| overrides?.get(key))?;
        parse_hex(raw.as_str()?).map(|c| c.color)
    };
    // `overrides` uses prefixed names (`syntax-keyword`) that never collide with
    // `palette` keys, so a separate lookup keeps the fallback chain readable.
    let pick_override = |key: &str| -> Option<Color> {
        let raw = overrides?.get(key)?;
        parse_hex(raw.as_str()?).map(|c| c.color)
    };

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(slug_hint)
        .to_string();
    let slug = slug_from(obj.get("id").and_then(Value::as_str), &name, slug_hint);

    let bg = pick("neutral").unwrap_or(Color::Rgb(30, 30, 46));
    let text = pick("ink").unwrap_or(Color::Rgb(205, 214, 244));
    let primary = pick("primary").unwrap_or(Color::Rgb(180, 190, 254));
    let accent = pick("accent").unwrap_or(primary);
    let error = pick("error").unwrap_or(Color::Rgb(243, 139, 168));
    let warning = pick("warning").unwrap_or(Color::Rgb(249, 226, 175));
    let success = pick("success").unwrap_or(Color::Rgb(166, 227, 161));
    let info = pick("info").unwrap_or(Color::Rgb(137, 220, 235));
    // Real desktop themes carry `text-weak` in `overrides`; without it, muted
    // text would be derived and could end up brighter than the body text.
    let text_muted = pick_override("text-weak").unwrap_or_else(|| mute(text, 0.55));

    Ok(ImportedTheme {
        slug,
        name,
        theme: build_theme(PaletteInput {
            bg,
            panel: shift(bg, 0.05),
            element: shift(bg, 0.1),
            text,
            text_muted,
            primary,
            secondary: accent,
            accent,
            error,
            warning,
            success,
            info,
            border: mute(text, 0.25),
            diff_added: pick("diffAdd").unwrap_or(success),
            diff_removed: pick("diffDelete").unwrap_or(error),
            diff_context: text_muted,
            diff_added_bg: None,
            diff_removed_bg: None,
            md_text: pick_override("markdown-text").unwrap_or(text),
            md_heading: pick_override("markdown-heading")
                .or_else(|| pick_override("syntax-keyword"))
                .unwrap_or(primary),
            md_link: pick_override("markdown-link")
                .or_else(|| pick_override("markdown-link-text"))
                .unwrap_or(info),
            md_code: pick_override("markdown-code")
                .or_else(|| pick_override("syntax-string"))
                .unwrap_or(success),
            md_muted: pick_override("markdown-block-quote")
                .or_else(|| pick_override("markdown-list-item"))
                .unwrap_or(text_muted),
            md_code_block: shift(bg, 0.08),
        }),
        source: ThemeSource::OpenCodeDesktop,
    })
}

fn parse_vscode(
    obj: &serde_json::Map<String, Value>,
    slug_hint: &str,
) -> Result<ImportedTheme, ImportError> {
    let colors = obj
        .get("colors")
        .and_then(Value::as_object)
        .ok_or_else(|| err("`colors` must be an object"))?;
    let raw = |key: &str| -> Option<HexColor> { parse_hex(colors.get(key)?.as_str()?) };

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(slug_hint)
        .to_string();
    let slug = slug_from(None, &name, slug_hint);

    let bg = raw("editor.background").unwrap_or_else(|| opaque(30, 30, 30));
    // Every other token is composited over the editor background, because VSCode
    // themes routinely express overlays (borders, diff bands) as alpha.
    let token = |key: &str| raw(key).map(|c| composite_over(c, bg));

    let text = token("editor.foreground").unwrap_or(Color::Rgb(212, 212, 212));
    let text_muted = token("editorLineNumber.foreground").unwrap_or_else(|| mute(text, 0.5));
    let primary = token("focusBorder")
        .or_else(|| token("button.background"))
        .unwrap_or(Color::Rgb(122, 162, 247));
    let info = token("textLink.foreground").unwrap_or(primary);

    let panel = token("sideBar.background").unwrap_or_else(|| shift(bg.color, 0.04));
    let element = token("editorWidget.background").unwrap_or_else(|| shift(bg.color, 0.08));

    Ok(ImportedTheme {
        slug,
        name,
        theme: build_theme(PaletteInput {
            bg: bg.color,
            panel,
            element,
            text,
            text_muted,
            primary,
            secondary: token("activityBarBadge.background").unwrap_or(primary),
            accent: info,
            error: token("errorForeground")
                .or_else(|| token("editorError.foreground"))
                .unwrap_or(Color::Rgb(247, 118, 142)),
            warning: token("editorWarning.foreground")
                .or_else(|| token("editorWarning.border"))
                .unwrap_or(Color::Rgb(224, 175, 104)),
            success: token("gitDecoration.addedResourceForeground")
                .or_else(|| token("editorGutter.addedBackground"))
                .unwrap_or(Color::Rgb(158, 206, 106)),
            info,
            border: token("panel.border")
                .or_else(|| token("editorGroup.border"))
                .unwrap_or_else(|| mute(text, 0.25)),
            diff_added: token("diffEditor.insertedTextBackground")
                .unwrap_or(Color::Rgb(158, 206, 106)),
            diff_removed: token("diffEditor.removedTextBackground")
                .unwrap_or(Color::Rgb(247, 118, 142)),
            diff_context: text_muted,
            diff_added_bg: token("diffEditor.insertedLineBackground"),
            diff_removed_bg: token("diffEditor.removedLineBackground"),
            md_text: text,
            md_heading: token("markdownHeading.foreground").unwrap_or(primary),
            md_link: token("markdownLink.foreground").unwrap_or(info),
            md_code: token("markdownInlineCode.foreground")
                .or_else(|| token("textPreformat.foreground"))
                .unwrap_or(Color::Rgb(158, 206, 106)),
            md_muted: token("markdownQuote.foreground").unwrap_or(text_muted),
            md_code_block: token("textCodeBlock.background").unwrap_or_else(|| shift(bg.color, 0.07)),
        }),
        source: ThemeSource::VSCode,
    })
}

// -- palette assembly --------------------------------------------------------

/// Every colour the mapper resolved, before `Theme` is populated.
///
/// A source format cannot express every pager field; the missing ones stay
/// `None` and are derived from the colours that *are* known, so an imported
/// theme never mixes two palettes.
struct PaletteInput {
    bg: Color,
    panel: Color,
    element: Color,
    text: Color,
    text_muted: Color,
    primary: Color,
    secondary: Color,
    accent: Color,
    error: Color,
    warning: Color,
    success: Color,
    info: Color,
    border: Color,
    diff_added: Color,
    diff_removed: Color,
    diff_context: Color,
    diff_added_bg: Option<Color>,
    diff_removed_bg: Option<Color>,
    md_text: Color,
    md_heading: Color,
    md_link: Color,
    md_code: Color,
    md_muted: Color,
    md_code_block: Color,
}

/// Populate `Theme` from a resolved palette.
///
/// Mirrors the plan's §7.5 mapping table (opencode token → `Theme` field).
fn build_theme(p: PaletteInput) -> Theme {
    let dark = luminance(p.bg) < 0.5;
    // Diff bands are the one place where a derived colour beats a source token:
    // a theme that specifies only foregrounds still needs a readable band, and
    // the band must be a tint of the background, not of the foreground.
    let band_weight = if dark { 0.18 } else { 0.24 };

    Theme {
        bg_base: p.bg,
        bg_light: p.element,
        bg_dark: p.panel,
        bg_highlight: p.element,
        bg_hover: shift_by(p.element, dark, 0.06),
        bg_terminal: p.bg,

        accent_user: p.primary,
        accent_assistant: p.secondary,
        accent_thinking: p.text_muted,
        accent_tool: p.border,
        accent_system: p.info,
        accent_error: p.error,
        accent_success: p.success,
        accent_running: p.accent,
        accent_skill: p.accent,

        text_primary: p.text,
        text_secondary: p.text_muted,

        gray_dim: mute(p.text, 0.35),
        gray: p.text_muted,
        gray_bright: mute(p.text, 0.7),

        command: p.warning,
        path: p.accent,
        running: p.info,
        warning: p.warning,

        fuzzy_accent: p.accent,

        accent_plan: p.warning,
        accent_verify: p.accent,
        accent_remember: p.success,

        selection_border: shift_by(p.border, dark, 0.12),
        hover_border: shift_by(p.border, dark, 0.06),
        prompt_border: p.border,
        prompt_border_active: shift_by(p.border, dark, 0.2),

        accent_model: p.info,

        scrollbar_bg: p.panel,
        scrollbar_fg: shift_by(p.border, dark, 0.1),

        diff_delete_bg: p
            .diff_removed_bg
            .unwrap_or_else(|| tint(p.bg, p.diff_removed, band_weight)),
        diff_delete_fg: p.diff_removed,
        diff_insert_bg: p
            .diff_added_bg
            .unwrap_or_else(|| tint(p.bg, p.diff_added, band_weight)),
        diff_insert_fg: p.diff_added,
        diff_equal_fg: p.diff_context,
        diff_gutter_fg: p.diff_context,

        bg_visual: shift_by(p.element, dark, 0.06),

        paste_bg: p.panel,
        paste_fg: p.text_muted,
        paste_dim: mute(p.text, 0.4),

        md_heading_h1: p.md_heading,
        md_heading_h1_mod: Modifier::BOLD,
        md_heading_h2: p.primary,
        md_heading_h2_mod: Modifier::BOLD,
        md_heading_h3: p.accent,
        md_heading_h3_mod: Modifier::BOLD,
        md_heading_h4: p.success,
        md_heading_h4_mod: Modifier::BOLD,
        md_heading_h5: p.warning,
        md_heading_h5_mod: Modifier::BOLD,
        md_heading_h6: p.info,
        md_heading_h6_mod: Modifier::BOLD,
        md_code: p.md_code,
        md_task_checked: p.success,
        md_task_unchecked: p.text_muted,
        md_muted: p.md_muted,
        md_code_bg: p.md_code_block,
        md_text: p.md_text,
        link_fg: p.md_link,
    }
}

// -- colour helpers ----------------------------------------------------------

/// Parsed `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa`.
#[derive(Debug, Clone, Copy)]
struct HexColor {
    color: Color,
    /// 0.0–1.0; opaque forms are 1.0.
    alpha: f32,
    rgb: (u8, u8, u8),
}

fn opaque(r: u8, g: u8, b: u8) -> HexColor {
    HexColor {
        color: Color::Rgb(r, g, b),
        alpha: 1.0,
        rgb: (r, g, b),
    }
}

fn parse_hex(raw: &str) -> Option<HexColor> {
    let hex = raw.trim().trim_start_matches('#');
    let digits: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<Vec<u8>>>()?;
    let (r, g, b, a) = match digits.len() {
        3 => (digits[0] * 17, digits[1] * 17, digits[2] * 17, 255),
        4 => (
            digits[0] * 17,
            digits[1] * 17,
            digits[2] * 17,
            digits[3] * 17,
        ),
        6 => (
            digits[0] * 16 + digits[1],
            digits[2] * 16 + digits[3],
            digits[4] * 16 + digits[5],
            255,
        ),
        8 => (
            digits[0] * 16 + digits[1],
            digits[2] * 16 + digits[3],
            digits[4] * 16 + digits[5],
            digits[6] * 16 + digits[7],
        ),
        _ => return None,
    };
    Some(HexColor {
        color: Color::Rgb(r, g, b),
        alpha: a as f32 / 255.0,
        rgb: (r, g, b),
    })
}

/// Composite `color` over an opaque `base`, honouring the source alpha.
fn composite_over(color: HexColor, base: HexColor) -> Color {
    if color.alpha >= 0.999 {
        return color.color;
    }
    let (br, bg, bb) = base.rgb;
    let (cr, cg, cb) = color.rgb;
    let a = color.alpha.clamp(0.0, 1.0);
    let mix = |b: u8, c: u8| (b as f32 * (1.0 - a) + c as f32 * a).round() as u8;
    Color::Rgb(mix(br, cr), mix(bg, cg), mix(bb, cb))
}

fn rgb_of(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// BT.709 relative luminance in 0.0–1.0; non-RGB reads as dark.
fn luminance(color: Color) -> f32 {
    let Some((r, g, b)) = rgb_of(color) else {
        return 0.0;
    };
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// Desaturate `color`, keeping `keep` of its distance from the channel mean.
///
/// Muting is a saturation change, not a brightness change: scaling channels
/// independently would shift the hue.
fn mute(color: Color, keep: f32) -> Color {
    let Some((r, g, b)) = rgb_of(color) else {
        return color;
    };
    let mean = (r as f32 + g as f32 + b as f32) / 3.0;
    let f = |c: u8| (mean + (c as f32 - mean) * keep).round().clamp(0.0, 255.0) as u8;
    Color::Rgb(f(r), f(g), f(b))
}

/// Move every channel `amount` (0.0–1.0 of full range) away from black.
fn shift(color: Color, amount: f32) -> Color {
    let Some((r, g, b)) = rgb_of(color) else {
        return color;
    };
    let step = (amount * 255.0).round() as i16;
    let f = |c: u8| (c as i16 + step).clamp(0, 255) as u8;
    Color::Rgb(f(r), f(g), f(b))
}

/// [`shift`] in the direction that increases contrast with a dark (or light) bg.
fn shift_by(color: Color, dark: bool, amount: f32) -> Color {
    shift(color, if dark { amount } else { -amount })
}

/// Blend `overlay` into `base` at `weight`, used for diff bands.
fn tint(base: Color, overlay: Color, weight: f32) -> Color {
    let (Some((br, bg, bb)), Some((or, og, ob))) = (rgb_of(base), rgb_of(overlay)) else {
        return base;
    };
    let mix = |b: u8, o: u8| (b as f32 * (1.0 - weight) + o as f32 * weight).round() as u8;
    Color::Rgb(mix(br, or), mix(bg, og), mix(bb, ob))
}

/// `[a-z0-9-]+` slug: document `id`, then display name, then file stem.
fn slug_from(id: Option<&str>, name: &str, hint: &str) -> String {
    let raw = id.filter(|s| !s.is_empty()).unwrap_or(name);
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    // Nothing usable survived. Fall back to the hint once; if that also
    // degrades, pick a stable literal instead of recursing.
    if raw != hint {
        let hint_slug = slug_from(None, hint, "");
        if hint_slug != "imported" || hint.is_empty() {
            return hint_slug;
        }
    }
    "imported".to_string()
}

// -- serialisation -----------------------------------------------------------

fn to_json(theme: &ImportedTheme) -> Value {
    let t = &theme.theme;
    // Built with a map rather than `json!`: ~60 keys blow the macro's default
    // recursion limit, and this also keeps the key order stable for diffs.
    let mut palette = serde_json::Map::new();
    let mut put = |key: &str, color: Color| {
        palette.insert(key.to_string(), Value::String(hex(color)));
    };
    put("bg_base", t.bg_base);
    put("bg_light", t.bg_light);
    put("bg_dark", t.bg_dark);
    put("bg_highlight", t.bg_highlight);
    put("bg_hover", t.bg_hover);
    put("accent_user", t.accent_user);
    put("accent_assistant", t.accent_assistant);
    put("accent_thinking", t.accent_thinking);
    put("accent_tool", t.accent_tool);
    put("accent_system", t.accent_system);
    put("accent_error", t.accent_error);
    put("accent_success", t.accent_success);
    put("accent_running", t.accent_running);
    put("accent_skill", t.accent_skill);
    put("text_primary", t.text_primary);
    put("text_secondary", t.text_secondary);
    put("gray_dim", t.gray_dim);
    put("gray", t.gray);
    put("gray_bright", t.gray_bright);
    put("command", t.command);
    put("path", t.path);
    put("running", t.running);
    put("warning", t.warning);
    put("fuzzy_accent", t.fuzzy_accent);
    put("accent_plan", t.accent_plan);
    put("accent_verify", t.accent_verify);
    put("accent_remember", t.accent_remember);
    put("selection_border", t.selection_border);
    put("hover_border", t.hover_border);
    put("prompt_border", t.prompt_border);
    put("prompt_border_active", t.prompt_border_active);
    put("accent_model", t.accent_model);
    put("scrollbar_bg", t.scrollbar_bg);
    put("scrollbar_fg", t.scrollbar_fg);
    put("diff_delete_bg", t.diff_delete_bg);
    put("diff_delete_fg", t.diff_delete_fg);
    put("diff_insert_bg", t.diff_insert_bg);
    put("diff_insert_fg", t.diff_insert_fg);
    put("diff_equal_fg", t.diff_equal_fg);
    put("diff_gutter_fg", t.diff_gutter_fg);
    put("bg_visual", t.bg_visual);
    put("paste_bg", t.paste_bg);
    put("paste_fg", t.paste_fg);
    put("paste_dim", t.paste_dim);
    put("md_heading_h1", t.md_heading_h1);
    put("md_heading_h2", t.md_heading_h2);
    put("md_heading_h3", t.md_heading_h3);
    put("md_heading_h4", t.md_heading_h4);
    put("md_heading_h5", t.md_heading_h5);
    put("md_heading_h6", t.md_heading_h6);
    put("md_code", t.md_code);
    put("md_task_checked", t.md_task_checked);
    put("md_task_unchecked", t.md_task_unchecked);
    put("md_muted", t.md_muted);
    put("md_code_bg", t.md_code_bg);
    put("md_text", t.md_text);
    put("link_fg", t.link_fg);

    let mut root = serde_json::Map::new();
    root.insert(
        "$schema".to_string(),
        Value::String("https://openbuild.dev/user-theme.json".to_string()),
    );
    root.insert("name".to_string(), Value::String(theme.name.clone()));
    root.insert("id".to_string(), Value::String(theme.slug.clone()));
    root.insert(
        "source".to_string(),
        Value::String(theme.source.label().to_string()),
    );
    root.insert("palette".to_string(), Value::Object(palette));
    Value::Object(root)
}

fn hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => format!("indexed:{n}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opencode_v1() -> &'static str {
        r##"{
          "name": "Test Night",
          "id": "test-night",
          "defs": { "brand": "#7aa2f7" },
          "theme": {
            "background": "#101010",
            "backgroundPanel": "#161616",
            "backgroundElement": "#1c1c1c",
            "text": "#dddddd",
            "textMuted": "#888888",
            "primary": "brand",
            "accent": "#bb9af7",
            "error": "#f7768e",
            "warning": "#e0af68",
            "success": "#9ece6a",
            "info": "#7dcfff",
            "border": "#2a2a2a",
            "diffAdded": "#9ece6a",
            "diffRemoved": "#f7768e",
            "markdownHeading": "#7aa2f7"
          }
        }"##
    }

    fn vscode() -> &'static str {
        r##"{
          "name": "Test Dark",
          "colors": {
            "editor.background": "#1e1e1e",
            "editor.foreground": "#d4d4d4",
            "editorLineNumber.foreground": "#858585",
            "focusBorder": "#007acc",
            "errorForeground": "#f14c4c",
            "editorWarning.foreground": "#cca700",
            "gitDecoration.addedResourceForeground": "#81b88b",
            "textLink.foreground": "#3794ff",
            "diffEditor.insertedTextBackground": "#9bb95533",
            "diffEditor.removedTextBackground": "#ff000033"
          }
        }"##
    }

    #[test]
    fn opencode_v1_resolves_defs_and_maps_tokens() {
        let theme = parse(opencode_v1(), "fallback").expect("must parse");
        assert_eq!(theme.slug, "test-night");
        assert_eq!(theme.name, "Test Night");
        assert_eq!(theme.source, ThemeSource::OpenCodeV1);
        assert_eq!(theme.theme.bg_base, Color::Rgb(0x10, 0x10, 0x10));
        // `primary` was a `defs` reference, not a literal hex.
        assert_eq!(theme.theme.accent_user, Color::Rgb(0x7a, 0xa2, 0xf7));
        assert_eq!(theme.theme.accent_error, Color::Rgb(0xf7, 0x76, 0x8e));
    }

    #[test]
    fn vscode_alpha_is_composited_not_dropped() {
        let theme = parse(vscode(), "fallback").expect("must parse");
        assert_eq!(theme.slug, "test-dark");
        // `#ff000033` is a 20% red overlay. Dropping alpha would give pure red.
        let diff = theme.theme.diff_delete_fg;
        assert_ne!(diff, Color::Rgb(255, 0, 0), "alpha must be composited");
        if let Color::Rgb(r, g, b) = diff {
            assert!(r > g && r > b, "should still read as reddish: {r},{g},{b}");
        }
    }

    #[test]
    fn vscode_diff_bands_are_tinted_backgrounds() {
        let theme = parse(vscode(), "fallback").expect("must parse");
        let bg = theme.theme.bg_base;
        assert_ne!(
            theme.theme.diff_insert_bg, bg,
            "insert band must differ from the background"
        );
        assert_ne!(theme.theme.diff_delete_bg, bg);
    }

    #[test]
    fn desktop_theme_prefers_the_dark_variant() {
        let doc = r##"{
          "name": "Desktop",
          "id": "desktop",
          "light": { "palette": { "neutral": "#ffffff", "ink": "#000000",
            "primary": "#0000ff", "success": "#00ff00", "warning": "#ffff00",
            "error": "#ff0000", "info": "#00ffff" } },
          "dark": { "palette": { "neutral": "#111111", "ink": "#eeeeee",
            "primary": "#7aa2f7", "success": "#9ece6a", "warning": "#e0af68",
            "error": "#f7768e", "info": "#7dcfff" } }
        }"##;
        let theme = parse(doc, "fallback").expect("must parse");
        assert_eq!(theme.source, ThemeSource::OpenCodeDesktop);
        assert_eq!(theme.theme.bg_base, Color::Rgb(0x11, 0x11, 0x11));
        assert_eq!(theme.theme.text_primary, Color::Rgb(0xee, 0xee, 0xee));
    }

    #[test]
    fn unknown_document_is_rejected_with_a_readable_message() {
        let err = parse(r#"{"hello": 1}"#, "x").expect_err("must reject");
        assert!(err.0.contains("unrecognised theme"), "{}", err.0);
    }

    #[test]
    fn invalid_json_is_rejected() {
        let err = parse("{ not json", "x").expect_err("must reject");
        assert!(err.0.contains("not valid JSON"), "{}", err.0);
    }

    #[test]
    fn slug_is_normalised_and_never_empty() {
        assert_eq!(slug_from(Some("My Theme!"), "x", "y"), "my-theme");
        assert_eq!(slug_from(None, "UPPER", "y"), "upper");
        assert_eq!(slug_from(None, "!!!", "fallback-name"), "fallback-name");
        assert_eq!(slug_from(None, "!!!", "!!!"), "imported");
    }

    #[test]
    fn hex_forms_parse_consistently() {
        let short = parse_hex("#123").expect("3-digit");
        let long = parse_hex("#112233").expect("6-digit");
        assert_eq!(short.rgb, long.rgb);
        let with_alpha = parse_hex("#11223380").expect("8-digit");
        assert!((with_alpha.alpha - 0.5).abs() < 0.01, "{}", with_alpha.alpha);
        assert!(parse_hex("nope").is_none());
        assert!(parse_hex("#12345").is_none());
    }

    #[test]
    fn saved_document_round_trips_through_parse() {
        let original = parse(opencode_v1(), "fallback").expect("must parse");
        let json = serde_json::to_string(&to_json(&original)).expect("serialise");
        // The stored shape puts the resolved palette at the root, so `/theme <slug>`
        // can re-read it without the original source file.
        let reloaded = parse(&json, "fallback").expect("stored theme must be re-readable");
        assert_eq!(reloaded.slug, original.slug);
        assert_eq!(reloaded.name, original.name);
        assert_eq!(reloaded.source, ThemeSource::Stored);
        assert_eq!(reloaded.theme.bg_base, original.theme.bg_base);
        assert_eq!(reloaded.theme.text_primary, original.theme.text_primary);
        assert_eq!(reloaded.theme.diff_insert_bg, original.theme.diff_insert_bg);
    }

    /// The shipped desktop themes use hyphenated override keys (`text-weak`,
    /// `markdown-heading`). Reading them is what makes a real GitHub/Dracula
    /// theme look right instead of falling back to derived greys.
    #[test]
    fn desktop_theme_reads_hyphenated_overrides() {
        let doc = r##"{
          "name": "Overrides",
          "id": "overrides",
          "dark": {
            "palette": { "neutral": "#0d1117", "ink": "#c9d1d9",
              "primary": "#58a6ff", "accent": "#1f6feb",
              "success": "#3fb950", "warning": "#d29922",
              "error": "#f85149", "info": "#58a6ff" },
            "overrides": {
              "text-weak": "#8b949e",
              "markdown-heading": "#79c0ff",
              "markdown-link": "#a5d6ff",
              "markdown-code": "#ff7b72",
              "markdown-block-quote": "#8b949e"
            }
          }
        }"##;
        let theme = parse(doc, "fallback").expect("must parse");
        assert_eq!(theme.theme.text_secondary, Color::Rgb(0x8b, 0x94, 0x9e));
        assert_eq!(theme.theme.md_heading_h1, Color::Rgb(0x79, 0xc0, 0xff));
        assert_eq!(theme.theme.link_fg, Color::Rgb(0xa5, 0xd6, 0xff));
        assert_eq!(theme.theme.md_code, Color::Rgb(0xff, 0x7b, 0x72));
        assert_eq!(theme.theme.md_muted, Color::Rgb(0x8b, 0x94, 0x9e));
    }

    /// Every shipped opencode desktop theme must be importable: one unparsable
    /// document means `/theme import` fails for a theme the user can see on disk.
    #[test]
    fn every_vendored_opencode_theme_parses() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("vendor/opencode/packages/ui/src/theme/themes");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // The vendored tree is not present in a standalone checkout of this
            // crate; the assertion only has meaning inside the OpenBuild repo.
            return;
        };
        let mut checked = 0;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let theme = parse_file(&path)
                .unwrap_or_else(|e| panic!("{} must parse: {e}", path.display()));
            // A palette that failed to resolve would leave the default grey.
            assert_ne!(
                theme.theme.bg_base,
                Color::Reset,
                "{} produced no background",
                path.display()
            );
            checked += 1;
        }
        assert!(checked > 0, "expected at least one vendored theme in {dir:?}");
    }
}
