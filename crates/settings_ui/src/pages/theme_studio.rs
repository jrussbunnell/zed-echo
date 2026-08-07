//! Theme Studio: an in-app editor for the active theme's tokens, the app's
//! font settings, and this fork's agent-panel/read-aloud styling.
//!
//! Token edits are written to `theme.theme_overrides[<active theme name>]` so
//! they are scoped to one theme and survive light/dark switching. Live preview
//! is not implemented here: `theme_settings` already reloads the theme when
//! `theme_overrides` changes, so writing the setting is the whole update path.

use std::rc::Rc;

use gpui::{Hsla, ReadGlobal as _, Rgba, ScrollHandle, prelude::*};
use settings::{
    FontStyleContent, FontWeightContent, HighlightStyleContent, Settings as _, ThemeColor,
    ThemeStyleContent,
};
use std::collections::HashSet;
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;
use ui::{Divider, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::components::{SettingsInputField, SettingsSectionHeader};
use crate::{
    SettingField, SettingItem, SettingsPageItem, SettingsUiFile, SettingsWindow, USER,
    update_settings_file,
};

/// Which flattened sub-struct of [`ThemeStyleContent`] a color token lives in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TokenSection {
    Colors,
    Status,
}

/// One editable color token of [`ThemeStyleContent`].
///
/// `resolve` reads the *live* theme, which already has overrides applied, so it
/// is the effective value; `read` only reports whether this token is overridden.
#[derive(Clone, Copy)]
pub(crate) struct ColorToken {
    pub key: &'static str,
    pub section: TokenSection,
    read: fn(&ThemeStyleContent) -> Option<&ThemeColor>,
    write: fn(&mut ThemeStyleContent, Option<ThemeColor>),
    resolve: fn(&theme::Theme) -> Hsla,
}

impl ColorToken {
    fn override_value(&self, overrides: Option<&ThemeStyleContent>) -> Option<String> {
        overrides
            .and_then(|overrides| (self.read)(overrides))
            .map(|color| color.to_string())
    }

    fn effective_color(&self, theme: &theme::Theme) -> Hsla {
        (self.resolve)(theme)
    }
}

macro_rules! theme_color_tokens {
    ($($key:literal => $field:ident,)*) => {
        &[$(ColorToken {
            key: $key,
            section: TokenSection::Colors,
            read: |style| style.colors.$field.as_ref(),
            write: |style, value| style.colors.$field = value,
            resolve: |theme| theme.colors().$field,
        }),*]
    };
}

macro_rules! status_color_tokens {
    ($($key:literal => $field:ident,)*) => {
        &[$(ColorToken {
            key: $key,
            section: TokenSection::Status,
            read: |style| style.status.$field.as_ref(),
            write: |style, value| style.status.$field = value,
            resolve: |theme| theme.status().$field,
        }),*]
    };
}

static THEME_COLOR_TOKENS: &[ColorToken] = theme_color_tokens![
    "border" => border,
    "border.variant" => border_variant,
    "border.focused" => border_focused,
    "border.selected" => border_selected,
    "border.transparent" => border_transparent,
    "border.disabled" => border_disabled,
    "elevated_surface.background" => elevated_surface_background,
    "surface.background" => surface_background,
    "background" => background,
    "element.background" => element_background,
    "element.hover" => element_hover,
    "element.active" => element_active,
    "element.selected" => element_selected,
    "element.disabled" => element_disabled,
    "element.selection_background" => element_selection_background,
    "drop_target.background" => drop_target_background,
    "drop_target.border" => drop_target_border,
    "ghost_element.background" => ghost_element_background,
    "ghost_element.hover" => ghost_element_hover,
    "ghost_element.active" => ghost_element_active,
    "ghost_element.selected" => ghost_element_selected,
    "ghost_element.disabled" => ghost_element_disabled,
    "text" => text,
    "text.muted" => text_muted,
    "text.placeholder" => text_placeholder,
    "text.disabled" => text_disabled,
    "text.accent" => text_accent,
    "icon" => icon,
    "icon.muted" => icon_muted,
    "icon.disabled" => icon_disabled,
    "icon.placeholder" => icon_placeholder,
    "icon.accent" => icon_accent,
    "debugger.accent" => debugger_accent,
    "status_bar.background" => status_bar_background,
    "title_bar.background" => title_bar_background,
    "title_bar.inactive_background" => title_bar_inactive_background,
    "toolbar.background" => toolbar_background,
    "tab_bar.background" => tab_bar_background,
    "tab.inactive_background" => tab_inactive_background,
    "tab.active_background" => tab_active_background,
    "search.match_background" => search_match_background,
    "search.active_match_background" => search_active_match_background,
    "panel.background" => panel_background,
    "panel.focused_border" => panel_focused_border,
    "panel.indent_guide" => panel_indent_guide,
    "panel.indent_guide_hover" => panel_indent_guide_hover,
    "panel.indent_guide_active" => panel_indent_guide_active,
    "panel.overlay_background" => panel_overlay_background,
    "panel.overlay_hover" => panel_overlay_hover,
    "pane.focused_border" => pane_focused_border,
    "pane_group.border" => pane_group_border,
    "scrollbar.thumb.background" => scrollbar_thumb_background,
    "scrollbar.thumb.hover_background" => scrollbar_thumb_hover_background,
    "scrollbar.thumb.active_background" => scrollbar_thumb_active_background,
    "scrollbar.thumb.border" => scrollbar_thumb_border,
    "scrollbar.track.background" => scrollbar_track_background,
    "scrollbar.track.border" => scrollbar_track_border,
    "minimap.thumb.background" => minimap_thumb_background,
    "minimap.thumb.hover_background" => minimap_thumb_hover_background,
    "minimap.thumb.active_background" => minimap_thumb_active_background,
    "minimap.thumb.border" => minimap_thumb_border,
    "editor.foreground" => editor_foreground,
    "editor.background" => editor_background,
    "editor.gutter.background" => editor_gutter_background,
    "editor.subheader.background" => editor_subheader_background,
    "editor.active_line.background" => editor_active_line_background,
    "editor.highlighted_line.background" => editor_highlighted_line_background,
    "editor.debugger_active_line.background" => editor_debugger_active_line_background,
    "editor.line_number" => editor_line_number,
    "editor.active_line_number" => editor_active_line_number,
    "editor.hover_line_number" => editor_hover_line_number,
    "editor.invisible" => editor_invisible,
    "editor.wrap_guide" => editor_wrap_guide,
    "editor.active_wrap_guide" => editor_active_wrap_guide,
    "editor.indent_guide" => editor_indent_guide,
    "editor.indent_guide_active" => editor_indent_guide_active,
    "editor.document_highlight.read_background" => editor_document_highlight_read_background,
    "editor.document_highlight.write_background" => editor_document_highlight_write_background,
    "editor.document_highlight.bracket_background" => editor_document_highlight_bracket_background,
    "editor.diff_hunk.added.background" => editor_diff_hunk_added_background,
    "editor.diff_hunk.added.hollow_background" => editor_diff_hunk_added_hollow_background,
    "editor.diff_hunk.added.hollow_border" => editor_diff_hunk_added_hollow_border,
    "editor.diff_hunk.deleted.background" => editor_diff_hunk_deleted_background,
    "editor.diff_hunk.deleted.hollow_background" => editor_diff_hunk_deleted_hollow_background,
    "editor.diff_hunk.deleted.hollow_border" => editor_diff_hunk_deleted_hollow_border,
    "terminal.background" => terminal_background,
    "terminal.foreground" => terminal_foreground,
    "terminal.ansi.background" => terminal_ansi_background,
    "terminal.bright_foreground" => terminal_bright_foreground,
    "terminal.dim_foreground" => terminal_dim_foreground,
    "terminal.ansi.black" => terminal_ansi_black,
    "terminal.ansi.bright_black" => terminal_ansi_bright_black,
    "terminal.ansi.dim_black" => terminal_ansi_dim_black,
    "terminal.ansi.red" => terminal_ansi_red,
    "terminal.ansi.bright_red" => terminal_ansi_bright_red,
    "terminal.ansi.dim_red" => terminal_ansi_dim_red,
    "terminal.ansi.green" => terminal_ansi_green,
    "terminal.ansi.bright_green" => terminal_ansi_bright_green,
    "terminal.ansi.dim_green" => terminal_ansi_dim_green,
    "terminal.ansi.yellow" => terminal_ansi_yellow,
    "terminal.ansi.bright_yellow" => terminal_ansi_bright_yellow,
    "terminal.ansi.dim_yellow" => terminal_ansi_dim_yellow,
    "terminal.ansi.blue" => terminal_ansi_blue,
    "terminal.ansi.bright_blue" => terminal_ansi_bright_blue,
    "terminal.ansi.dim_blue" => terminal_ansi_dim_blue,
    "terminal.ansi.magenta" => terminal_ansi_magenta,
    "terminal.ansi.bright_magenta" => terminal_ansi_bright_magenta,
    "terminal.ansi.dim_magenta" => terminal_ansi_dim_magenta,
    "terminal.ansi.cyan" => terminal_ansi_cyan,
    "terminal.ansi.bright_cyan" => terminal_ansi_bright_cyan,
    "terminal.ansi.dim_cyan" => terminal_ansi_dim_cyan,
    "terminal.ansi.white" => terminal_ansi_white,
    "terminal.ansi.bright_white" => terminal_ansi_bright_white,
    "terminal.ansi.dim_white" => terminal_ansi_dim_white,
    "link_text.hover" => link_text_hover,
    "version_control.added" => version_control_added,
    "version_control.deleted" => version_control_deleted,
    "version_control.modified" => version_control_modified,
    "version_control.renamed" => version_control_renamed,
    "version_control.conflict" => version_control_conflict,
    "version_control.ignored" => version_control_ignored,
    "version_control.word_added" => version_control_word_added,
    "version_control.word_deleted" => version_control_word_deleted,
    "version_control.conflict_marker.ours" => version_control_conflict_marker_ours,
    "version_control.conflict_marker.theirs" => version_control_conflict_marker_theirs,
    "vim.normal.background" => vim_normal_background,
    "vim.insert.background" => vim_insert_background,
    "vim.replace.background" => vim_replace_background,
    "vim.visual.background" => vim_visual_background,
    "vim.visual_line.background" => vim_visual_line_background,
    "vim.visual_block.background" => vim_visual_block_background,
    "vim.yank.background" => vim_yank_background,
    "vim.helix_jump_label.foreground" => vim_helix_jump_label_foreground,
    "vim.helix_normal.background" => vim_helix_normal_background,
    "vim.helix_select.background" => vim_helix_select_background,
    "vim.normal.foreground" => vim_normal_foreground,
    "vim.insert.foreground" => vim_insert_foreground,
    "vim.replace.foreground" => vim_replace_foreground,
    "vim.visual.foreground" => vim_visual_foreground,
    "vim.visual_line.foreground" => vim_visual_line_foreground,
    "vim.visual_block.foreground" => vim_visual_block_foreground,
    "vim.helix_normal.foreground" => vim_helix_normal_foreground,
    "vim.helix_select.foreground" => vim_helix_select_foreground,
];

static STATUS_COLOR_TOKENS: &[ColorToken] = status_color_tokens![
    "conflict" => conflict,
    "conflict.background" => conflict_background,
    "conflict.border" => conflict_border,
    "created" => created,
    "created.background" => created_background,
    "created.border" => created_border,
    "deleted" => deleted,
    "deleted.background" => deleted_background,
    "deleted.border" => deleted_border,
    "error" => error,
    "error.background" => error_background,
    "error.border" => error_border,
    "hidden" => hidden,
    "hidden.background" => hidden_background,
    "hidden.border" => hidden_border,
    "hint" => hint,
    "hint.background" => hint_background,
    "hint.border" => hint_border,
    "ignored" => ignored,
    "ignored.background" => ignored_background,
    "ignored.border" => ignored_border,
    "info" => info,
    "info.background" => info_background,
    "info.border" => info_border,
    "modified" => modified,
    "modified.background" => modified_background,
    "modified.border" => modified_border,
    "predictive" => predictive,
    "predictive.background" => predictive_background,
    "predictive.border" => predictive_border,
    "renamed" => renamed,
    "renamed.background" => renamed_background,
    "renamed.border" => renamed_border,
    "success" => success,
    "success.background" => success_background,
    "success.border" => success_border,
    "unreachable" => unreachable,
    "unreachable.background" => unreachable_background,
    "unreachable.border" => unreachable_border,
    "warning" => warning,
    "warning.background" => warning_background,
    "warning.border" => warning_border,
];

pub(crate) fn all_color_tokens() -> impl Iterator<Item = &'static ColorToken> {
    THEME_COLOR_TOKENS.iter().chain(STATUS_COLOR_TOKENS)
}

/// Display groups, in the order they appear on the page.
///
/// `players` and `accents` are arrays rather than named tokens and are not
/// editable here yet, so they have no group.
pub(crate) const TOKEN_GROUPS: &[&str] = &[
    "Editor",
    "Syntax",
    "Terminal",
    "Text & Icons",
    "Borders",
    "Elements",
    "UI Chrome & Surfaces",
    "Scrollbar & Minimap",
    "Status & Diagnostics",
    "Version Control",
    "Vim & Helix",
    "Other",
];

/// Groups that start collapsed. The page has over two hundred rows in total,
/// so only the group users reach for most is open on arrival; searching
/// expands whatever matches regardless.
pub(crate) fn default_collapsed_groups() -> HashSet<&'static str> {
    TOKEN_GROUPS
        .iter()
        .copied()
        .filter(|group| *group != "Editor")
        .collect()
}

/// Assigns a token to a display group from its JSON key, so the grouping stays
/// correct as tokens are added upstream instead of needing a parallel table.
pub(crate) fn group_for_token(token: &ColorToken) -> &'static str {
    if token.section == TokenSection::Status {
        return "Status & Diagnostics";
    }
    let key = token.key;
    if key.starts_with("editor.") {
        "Editor"
    } else if key.starts_with("terminal.") {
        "Terminal"
    } else if key.starts_with("version_control.") {
        "Version Control"
    } else if key.starts_with("vim.") {
        "Vim & Helix"
    } else if key.starts_with("scrollbar.") || key.starts_with("minimap.") {
        "Scrollbar & Minimap"
    } else if key.starts_with("border") {
        "Borders"
    } else if key.starts_with("element.")
        || key.starts_with("ghost_element.")
        || key.starts_with("drop_target.")
    {
        "Elements"
    } else if key == "text"
        || key.starts_with("text.")
        || key == "icon"
        || key.starts_with("icon.")
        || key.starts_with("link_text.")
        || key == "debugger.accent"
    {
        "Text & Icons"
    } else if key == "background"
        || key.starts_with("surface.")
        || key.starts_with("elevated_surface.")
        || key.starts_with("panel.")
        || key.starts_with("pane.")
        || key.starts_with("pane_group.")
        || key.starts_with("tab.")
        || key.starts_with("tab_bar.")
        || key.starts_with("title_bar.")
        || key.starts_with("status_bar.")
        || key.starts_with("toolbar.")
        || key.starts_with("search.")
    {
        "UI Chrome & Surfaces"
    } else {
        "Other"
    }
}

/// Case-insensitive substring match against a token key.
pub(crate) fn token_matches_query(key: &str, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    key.to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

/// Color tokens that match `query`, bucketed by display group and returned in
/// [`TOKEN_GROUPS`] order. Groups with no matches are omitted.
pub(crate) fn grouped_color_tokens(query: &str) -> Vec<(&'static str, Vec<&'static ColorToken>)> {
    let mut groups: Vec<(&'static str, Vec<&'static ColorToken>)> = TOKEN_GROUPS
        .iter()
        .map(|group| (*group, Vec::new()))
        .collect();

    for token in all_color_tokens() {
        if !token_matches_query(token.key, query) {
            continue;
        }
        let group = group_for_token(token);
        if let Some(bucket) = groups.iter_mut().find(|(name, _)| *name == group) {
            bucket.1.push(token);
        }
    }

    groups.retain(|(_, tokens)| !tokens.is_empty());
    groups
}

/// Formats a color the way theme JSON expects: `#RRGGBB`, or `#RRGGBBAA` when
/// the color is not fully opaque.
pub(crate) fn format_color(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    let (red, green, blue, alpha) = (
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b),
        channel(rgba.a),
    );
    if alpha == u8::MAX {
        format!("#{red:02X}{green:02X}{blue:02X}")
    } else {
        format!("#{red:02X}{green:02X}{blue:02X}{alpha:02X}")
    }
}

/// Validates typed hex and returns the string to persist.
///
/// Parsing is delegated to [`theme::try_parse_color`], the same function the
/// theme system uses on these values, so the preview cannot disagree with what
/// the theme will render. The leading `#` is optional here only because the
/// fork's own color settings accept it that way.
pub(crate) fn normalize_hex_input(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = if trimmed.starts_with('#') {
        trimmed.to_string()
    } else {
        format!("#{trimmed}")
    };
    theme::try_parse_color(&candidate).ok()?;
    Some(candidate.to_ascii_uppercase())
}

fn parse_color_or(text: Option<&str>, fallback: Hsla) -> Hsla {
    text.and_then(|text| theme::try_parse_color(text).ok())
        .unwrap_or(fallback)
}

/// Applies `mutate` to the current theme's override entry in the user settings
/// file. The edit is targeted at that entry, leaving the rest of settings.json
/// untouched.
fn update_theme_override(
    theme_name: String,
    window: &mut Window,
    cx: &mut App,
    mutate: impl 'static + Send + FnOnce(&mut ThemeStyleContent),
) {
    update_settings_file(
        SettingsUiFile::User,
        Some("theme_overrides"),
        window,
        cx,
        move |settings, _| {
            let entry = settings
                .theme
                .theme_overrides
                .entry(theme_name)
                .or_default();
            mutate(entry);
        },
    )
    .log_err();
}

fn clear_theme_overrides(theme_name: String, window: &mut Window, cx: &mut App) {
    update_settings_file(
        SettingsUiFile::User,
        Some("theme_overrides"),
        window,
        cx,
        move |settings, _| {
            settings.theme.theme_overrides.remove(&theme_name);
        },
    )
    .log_err();
}

/// Identifies the single row that is currently expanded for editing. Only one
/// row is expanded at a time so the page holds one text editor rather than one
/// per token.
pub(crate) type EditingRowId = SharedString;

pub(crate) fn render_theme_studio_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let theme_name = theme.name.to_string();
    let overrides = ThemeSettings::get_global(cx)
        .theme_overrides
        .get(&theme_name)
        .cloned();
    let query = settings_window.theme_studio_search.read(cx).text(cx);

    v_flex()
        .id("theme-studio-page")
        .min_w_0()
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_4()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(render_header(
            settings_window,
            &theme_name,
            overrides.as_ref(),
            cx,
        ))
        .child(render_search_bar(settings_window, cx))
        .children(render_token_groups(
            settings_window,
            &theme,
            &theme_name,
            overrides.as_ref(),
            &query,
            cx,
        ))
        .child(render_syntax_section(
            settings_window,
            &theme,
            &theme_name,
            overrides.as_ref(),
            &query,
            cx,
        ))
        .child(render_fonts_section(settings_window, window, cx))
        .child(render_agent_panel_section(settings_window, window, cx))
        .child(render_read_aloud_section(settings_window, window, cx))
        .into_any_element()
}

fn override_count(overrides: Option<&ThemeStyleContent>) -> usize {
    let Some(overrides) = overrides else {
        return 0;
    };
    all_color_tokens()
        .filter(|token| (token.read)(overrides).is_some())
        .count()
        + overrides.syntax.len()
}

fn render_header(
    settings_window: &SettingsWindow,
    theme_name: &str,
    overrides: Option<&ThemeStyleContent>,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let count = override_count(overrides);
    let confirming = settings_window.theme_studio_reset_all_confirming;
    let theme_name_for_reset = theme_name.to_string();
    let summary = if count == 0 {
        format!("No customizations for \"{theme_name}\" yet.")
    } else if count == 1 {
        format!("1 customization applied to \"{theme_name}\".")
    } else {
        format!("{count} customizations applied to \"{theme_name}\".")
    };

    v_flex()
        .gap_1()
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .items_start()
                .child(
                    v_flex().child(Label::new("Theme Studio")).child(
                        Label::new(summary)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    Button::new(
                        "theme-studio-reset-all",
                        if confirming {
                            "Click Again to Confirm"
                        } else {
                            "Reset All"
                        },
                    )
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .disabled(count == 0)
                    .tooltip(Tooltip::text(
                        "Remove every customization for the active theme",
                    ))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.theme_studio_reset_all_confirming {
                            this.theme_studio_reset_all_confirming = false;
                            clear_theme_overrides(theme_name_for_reset.clone(), window, cx);
                        } else {
                            this.theme_studio_reset_all_confirming = true;
                        }
                        cx.notify();
                    })),
                ),
        )
        .child(Divider::horizontal())
}

fn render_search_bar(
    settings_window: &SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    h_flex()
        .w_full()
        .h_8()
        .px_2()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .child(
            Icon::new(IconName::MagnifyingGlass)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .child(settings_window.theme_studio_search.clone())
}

fn render_token_groups(
    settings_window: &SettingsWindow,
    theme: &theme::Theme,
    theme_name: &str,
    overrides: Option<&ThemeStyleContent>,
    query: &str,
    cx: &mut Context<SettingsWindow>,
) -> Vec<AnyElement> {
    let searching = !query.trim().is_empty();
    grouped_color_tokens(query)
        .into_iter()
        .map(|(group, tokens)| {
            // A search should show its results, so an active query overrides
            // whatever collapse state the user left the group in.
            let expanded = searching
                || !settings_window
                    .theme_studio_collapsed_groups
                    .contains(group);
            let rows = expanded.then(|| {
                tokens
                    .iter()
                    .map(|token| {
                        render_color_token_row(
                            settings_window,
                            token,
                            theme,
                            theme_name,
                            overrides,
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            });

            v_flex()
                .gap_1()
                .child(render_group_header(
                    group,
                    tokens.len(),
                    expanded,
                    !searching,
                    cx,
                ))
                .children(rows.into_iter().flatten())
                .into_any_element()
        })
        .collect()
}

fn render_group_header(
    group: &'static str,
    count: usize,
    expanded: bool,
    collapsible: bool,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    h_flex()
        .id(SharedString::from(format!("theme-studio-group-{group}")))
        .w_full()
        .gap_2()
        .items_center()
        .when(collapsible, |this| {
            this.cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.theme_studio_collapsed_groups.remove(group) {
                        this.theme_studio_collapsed_groups.insert(group);
                    }
                    cx.notify();
                }))
        })
        .when(collapsible, |this| {
            this.child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::Small)
                .color(Color::Muted),
            )
        })
        .child(SettingsSectionHeader::new(SharedString::new_static(group)).no_padding(true))
        .child(
            Label::new(count.to_string())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
}

fn render_color_token_row(
    settings_window: &SettingsWindow,
    token: &'static ColorToken,
    theme: &theme::Theme,
    theme_name: &str,
    overrides: Option<&ThemeStyleContent>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let row_id: EditingRowId = SharedString::from(format!("token:{}", token.key));
    let override_value = token.override_value(overrides);
    let effective = token.effective_color(theme);

    let commit = {
        let theme_name = theme_name.to_string();
        Rc::new(
            move |value: Option<String>, window: &mut Window, cx: &mut App| {
                update_theme_override(theme_name.clone(), window, cx, move |style| {
                    (token.write)(style, value.map(ThemeColor::from));
                });
            },
        ) as Rc<dyn Fn(Option<String>, &mut Window, &mut App)>
    };

    render_color_row(
        settings_window,
        ColorRow {
            row_id,
            label: SharedString::new_static(token.key),
            description: None,
            override_value,
            effective,
            commit,
        },
        cx,
    )
}

struct ColorRow {
    row_id: EditingRowId,
    label: SharedString,
    description: Option<SharedString>,
    /// The raw string currently stored in settings, if this value is customized.
    override_value: Option<String>,
    /// The color actually in effect, used for the swatch.
    effective: Hsla,
    commit: Rc<dyn Fn(Option<String>, &mut Window, &mut App)>,
}

fn render_color_row(
    settings_window: &SettingsWindow,
    row: ColorRow,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let is_editing = settings_window.theme_studio_editing_row.as_ref() == Some(&row.row_id);
    let is_overridden = row.override_value.is_some();
    let displayed_hex = row
        .override_value
        .clone()
        .unwrap_or_else(|| format_color(row.effective));
    let colors = cx.theme().colors();

    let header = h_flex()
        .id(SharedString::from(format!("row-{}", row.row_id)))
        .w_full()
        .py_1()
        .gap_2()
        .items_center()
        .justify_between()
        .cursor_pointer()
        .on_click(cx.listener({
            let row_id = row.row_id.clone();
            move |this, _, _, cx| {
                if this.theme_studio_editing_row.as_ref() == Some(&row_id) {
                    this.theme_studio_editing_row = None;
                } else {
                    this.theme_studio_editing_row = Some(row_id.clone());
                }
                this.theme_studio_color_error = None;
                this.theme_studio_reset_all_confirming = false;
                cx.notify();
            }
        }))
        .child(
            h_flex()
                .gap_2()
                .min_w_0()
                .items_center()
                .child(
                    div()
                        .size_4()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border)
                        .bg(row.effective),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .child(Label::new(row.label.clone()).size(LabelSize::Small))
                        .when_some(row.description.clone(), |this, description| {
                            this.child(
                                Label::new(description)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        }),
                ),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Label::new(displayed_hex.clone())
                        .size(LabelSize::Small)
                        .color(if is_overridden {
                            Color::Accent
                        } else {
                            Color::Muted
                        }),
                )
                .when(is_overridden, |this| {
                    this.child(
                        IconButton::new(
                            SharedString::from(format!("reset-{}", row.row_id)),
                            IconName::RotateCcw,
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .aria_label("Reset to theme default")
                        .tooltip(Tooltip::text("Reset to theme default"))
                        .on_click({
                            let commit = row.commit.clone();
                            move |_, window, cx| commit(None, window, cx)
                        }),
                    )
                }),
        );

    let editor = is_editing.then(|| {
        let error = settings_window.theme_studio_color_error.clone();
        // `on_confirm` runs outside the entity lease, so the page updates its
        // own validation state through a weak handle rather than `cx.listener`,
        // whose callback signature this component does not use.
        let settings_window_handle = cx.entity().downgrade();
        let commit = row.commit.clone();
        let label = row.label.clone();
        v_flex()
            .pb_2()
            .gap_1()
            .child(
                SettingsInputField::new(SharedString::from(format!("hex-{}", row.row_id)))
                    .with_initial_text(displayed_hex)
                    .with_placeholder("#RRGGBB or #RRGGBBAA")
                    .with_buffer_font()
                    .aria_label(format!("Hex color for {label}"))
                    .display_confirm_button()
                    .on_confirm(move |text, window, cx| {
                        let error = match text.as_deref().map(str::trim) {
                            None | Some("") => {
                                commit(None, window, cx);
                                None
                            }
                            Some(text) => match normalize_hex_input(text) {
                                Some(value) => {
                                    commit(Some(value), window, cx);
                                    None
                                }
                                None => Some(SharedString::from(format!(
                                    "\"{text}\" is not a valid hex color."
                                ))),
                            },
                        };
                        settings_window_handle
                            .update(cx, |settings_window, cx| {
                                settings_window.theme_studio_color_error = error;
                                cx.notify();
                            })
                            .log_err();
                    }),
            )
            .when_some(error, |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
    });

    v_flex()
        .w_full()
        .child(header)
        .children(editor)
        .into_any_element()
}

fn render_syntax_section(
    settings_window: &SettingsWindow,
    theme: &theme::Theme,
    theme_name: &str,
    overrides: Option<&ThemeStyleContent>,
    query: &str,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let names = syntax_token_names(theme, overrides, query);
    let searching = !query.trim().is_empty();
    let expanded = searching
        || !settings_window
            .theme_studio_collapsed_groups
            .contains("Syntax");

    v_flex()
        .gap_1()
        .when(!names.is_empty(), |this| {
            this.child(render_group_header(
                "Syntax",
                names.len(),
                expanded,
                !searching,
                cx,
            ))
        })
        .when(expanded, |this| {
            this.children(names.into_iter().map(|name| {
                render_syntax_row(settings_window, name, theme, theme_name, overrides, cx)
            }))
        })
}

/// Syntax capture names offered for editing: those the active theme defines,
/// plus any the user has already overridden (which may not exist in the theme).
pub(crate) fn syntax_token_names(
    theme: &theme::Theme,
    overrides: Option<&ThemeStyleContent>,
    query: &str,
) -> Vec<SharedString> {
    let mut names = HashSet::new();
    let syntax = theme.syntax();
    // `SyntaxTheme` exposes no iterator over its capture names, so walk the
    // highlight indices, which are dense from zero.
    let mut index = 0usize;
    while syntax.get(index).is_some() {
        if let Some(name) = syntax.get_capture_name(index) {
            names.insert(SharedString::from(name.to_string()));
        }
        index += 1;
    }
    if let Some(overrides) = overrides {
        for name in overrides.syntax.keys() {
            names.insert(SharedString::from(name.clone()));
        }
    }

    let mut names: Vec<SharedString> = names
        .into_iter()
        .filter(|name| token_matches_query(name, query))
        .collect();
    names.sort();
    names
}

fn render_syntax_row(
    settings_window: &SettingsWindow,
    name: SharedString,
    theme: &theme::Theme,
    theme_name: &str,
    overrides: Option<&ThemeStyleContent>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let row_id: EditingRowId = SharedString::from(format!("syntax:{name}"));
    let style_override = overrides.and_then(|overrides| overrides.syntax.get(name.as_ref()));
    let override_value = style_override
        .and_then(|style| style.color.as_ref())
        .map(|color| color.to_string());
    let effective = theme
        .syntax()
        .style_for_name(&name)
        .and_then(|style| style.color)
        .unwrap_or(theme.colors().editor_foreground);
    let is_editing = settings_window.theme_studio_editing_row.as_ref() == Some(&row_id);

    let commit = {
        let theme_name = theme_name.to_string();
        let name = name.to_string();
        Rc::new(
            move |value: Option<String>, window: &mut Window, cx: &mut App| {
                let name = name.clone();
                update_theme_override(theme_name.clone(), window, cx, move |style| {
                    update_syntax_style(style, &name, |highlight| {
                        highlight.color = value.map(ThemeColor::from);
                    });
                });
            },
        ) as Rc<dyn Fn(Option<String>, &mut Window, &mut App)>
    };

    let color_row = render_color_row(
        settings_window,
        ColorRow {
            row_id,
            label: name.clone(),
            description: None,
            override_value,
            effective,
            commit,
        },
        cx,
    );

    v_flex()
        .w_full()
        .child(color_row)
        .when(is_editing, |this| {
            this.child(render_syntax_style_controls(
                name.clone(),
                theme,
                theme_name,
                style_override,
            ))
        })
        .into_any_element()
}

/// Applies `mutate` to a syntax entry, removing the entry entirely once it no
/// longer carries any customization so resets do not leave empty objects behind.
fn update_syntax_style(
    style: &mut ThemeStyleContent,
    name: &str,
    mutate: impl FnOnce(&mut HighlightStyleContent),
) {
    let entry = style.syntax.entry(name.to_string()).or_default();
    mutate(entry);
    if entry.color.is_none()
        && entry.background_color.is_none()
        && entry.font_style.is_none()
        && entry.font_weight.is_none()
    {
        style.syntax.shift_remove(name);
    }
}

const FONT_STYLES: [(FontStyleContent, &str); 3] = [
    (FontStyleContent::Normal, "Normal"),
    (FontStyleContent::Italic, "Italic"),
    (FontStyleContent::Oblique, "Oblique"),
];

const FONT_WEIGHTS: [(f32, &str); 4] = [
    (FontWeightContent::LIGHT.0, "Light"),
    (FontWeightContent::NORMAL.0, "Normal"),
    (FontWeightContent::SEMIBOLD.0, "Semibold"),
    (FontWeightContent::BOLD.0, "Bold"),
];

fn render_syntax_style_controls(
    name: SharedString,
    theme: &theme::Theme,
    theme_name: &str,
    style_override: Option<&HighlightStyleContent>,
) -> impl IntoElement {
    let resolved = theme.syntax().style_for_name(&name);
    let current_style = style_override.and_then(|style| style.font_style);
    let current_weight = style_override
        .and_then(|style| style.font_weight)
        .map(|weight| weight.0);
    let inherited_style_italic = resolved
        .and_then(|style| style.font_style)
        .is_some_and(|style| style != gpui::FontStyle::Normal);
    let inherited_weight = resolved
        .and_then(|style| style.font_weight)
        .map(|weight| weight.0);

    let style_buttons = FONT_STYLES.map(|(style, label)| {
        let selected = current_style == Some(style)
            || (current_style.is_none()
                && style == FontStyleContent::Italic
                && inherited_style_italic);
        let theme_name = theme_name.to_string();
        let name = name.to_string();
        Button::new(
            SharedString::from(format!("syntax-style-{name}-{label}")),
            label,
        )
        .label_size(LabelSize::Small)
        .style(if selected {
            ButtonStyle::Filled
        } else {
            ButtonStyle::Subtle
        })
        .on_click(move |_, window, cx| {
            let name = name.clone();
            let new_style = (!selected).then_some(style);
            update_theme_override(theme_name.clone(), window, cx, move |content| {
                update_syntax_style(content, &name, |highlight| {
                    highlight.font_style = new_style;
                });
            });
        })
        .into_any_element()
    });

    let weight_buttons = FONT_WEIGHTS.map(|(weight, label)| {
        let selected = current_weight == Some(weight)
            || (current_weight.is_none() && inherited_weight == Some(weight));
        let theme_name = theme_name.to_string();
        let name = name.to_string();
        Button::new(
            SharedString::from(format!("syntax-weight-{name}-{label}")),
            label,
        )
        .label_size(LabelSize::Small)
        .style(if selected {
            ButtonStyle::Filled
        } else {
            ButtonStyle::Subtle
        })
        .on_click(move |_, window, cx| {
            let name = name.clone();
            let new_weight = (!selected).then_some(FontWeightContent(weight));
            update_theme_override(theme_name.clone(), window, cx, move |content| {
                update_syntax_style(content, &name, |highlight| {
                    highlight.font_weight = new_weight;
                });
            });
        })
        .into_any_element()
    });

    v_flex()
        .pb_2()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Label::new("Style")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .children(style_buttons),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Label::new("Weight")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .children(weight_buttons),
        )
}

fn render_setting_items(
    settings_window: &SettingsWindow,
    items: impl IntoIterator<Item = SettingsPageItem>,
    id_base: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Vec<AnyElement> {
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            item.render(settings_window, id_base + index, false, false, window, cx)
        })
        .collect()
}

fn render_fonts_section(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let items = crate::page_data::ui_font_section()
        .into_iter()
        .chain(crate::page_data::buffer_font_section())
        .chain(crate::page_data::agent_panel_font_section());

    v_flex()
        .gap_1()
        .child(SettingsSectionHeader::new("Fonts").no_padding(true))
        .children(render_setting_items(
            settings_window,
            items,
            FONTS_ITEM_ID_BASE,
            window,
            cx,
        ))
}

// Element id bases keep the ad-hoc `SettingItem`s on this page from colliding
// with each other in `window.with_id`.
const FONTS_ITEM_ID_BASE: usize = 10_000;
const AGENT_PANEL_ITEM_ID_BASE: usize = 20_000;
const READ_ALOUD_ITEM_ID_BASE: usize = 30_000;

/// The five agent-panel content groups, as `(settings key, display name)`.
pub(crate) const AGENT_PANEL_GROUPS: [(&str, &str); 5] = [
    ("assistant_prose", "Assistant Prose"),
    ("thinking", "Thinking"),
    ("tool_output", "Tool Output"),
    ("user_message", "User Message"),
    ("code_blocks", "Code Blocks"),
];

macro_rules! agent_panel_style_items {
    ($group:ident, $title:literal, $json_prefix:literal) => {
        [SettingsPageItem::SettingItem(SettingItem {
            title: concat!($title, " Font Size"),
            description: "Size in pixels. Values outside 8–32 are ignored.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some(concat!("agent_panel_styling.", $json_prefix, ".font_size")),
                pick: |settings_content| {
                    settings_content
                        .agent_panel_styling
                        .as_ref()?
                        .$group
                        .as_ref()?
                        .font_size
                        .as_ref()
                },
                write: |settings_content, value, _| {
                    settings_content
                        .agent_panel_styling
                        .get_or_insert_default()
                        .$group
                        .get_or_insert_default()
                        .font_size = value;
                },
            }),
            metadata: None,
            files: USER,
        })]
    };
}

fn agent_panel_font_items(group: &str) -> [SettingsPageItem; 1] {
    match group {
        "assistant_prose" => {
            agent_panel_style_items!(assistant_prose, "Assistant Prose", "assistant_prose")
        }
        "thinking" => agent_panel_style_items!(thinking, "Thinking", "thinking"),
        "tool_output" => agent_panel_style_items!(tool_output, "Tool Output", "tool_output"),
        "user_message" => agent_panel_style_items!(user_message, "User Message", "user_message"),
        _ => agent_panel_style_items!(code_blocks, "Code Blocks", "code_blocks"),
    }
}

/// Reads and writes for one string field of one agent-panel group. Kept as
/// function pointers so the group can be selected at runtime while the settings
/// path stays static.
struct AgentPanelStringAccess {
    read: fn(&settings::SettingsContent) -> Option<&String>,
    write: fn(&mut settings::SettingsContent, Option<String>),
}

macro_rules! agent_panel_string_access {
    ($group:ident, $field:ident) => {
        AgentPanelStringAccess {
            read: |settings_content| {
                settings_content
                    .agent_panel_styling
                    .as_ref()?
                    .$group
                    .as_ref()?
                    .$field
                    .as_ref()
            },
            write: |settings_content, value| {
                settings_content
                    .agent_panel_styling
                    .get_or_insert_default()
                    .$group
                    .get_or_insert_default()
                    .$field = value;
            },
        }
    };
}

fn agent_panel_color_access(group: &str, is_background: bool) -> AgentPanelStringAccess {
    match (group, is_background) {
        ("assistant_prose", false) => agent_panel_string_access!(assistant_prose, text_color),
        ("assistant_prose", true) => agent_panel_string_access!(assistant_prose, background),
        ("thinking", false) => agent_panel_string_access!(thinking, text_color),
        ("thinking", true) => agent_panel_string_access!(thinking, background),
        ("tool_output", false) => agent_panel_string_access!(tool_output, text_color),
        ("tool_output", true) => agent_panel_string_access!(tool_output, background),
        ("user_message", false) => agent_panel_string_access!(user_message, text_color),
        ("user_message", true) => agent_panel_string_access!(user_message, background),
        (_, false) => agent_panel_string_access!(code_blocks, text_color),
        (_, true) => agent_panel_string_access!(code_blocks, background),
    }
}

fn agent_panel_font_family_access(group: &str) -> AgentPanelStringAccess {
    match group {
        "assistant_prose" => agent_panel_string_access!(assistant_prose, font_family),
        "thinking" => agent_panel_string_access!(thinking, font_family),
        "tool_output" => agent_panel_string_access!(tool_output, font_family),
        "user_message" => agent_panel_string_access!(user_message, font_family),
        _ => agent_panel_string_access!(code_blocks, font_family),
    }
}

/// A font-family row for one of the fork's `Option<String>` font settings.
///
/// The built-in `FontFamilyName` renderer cannot be reused because these fields
/// are plain strings, but the picker itself is the same one, so the family list
/// still comes from the cache the settings window prefetches off the main
/// thread.
fn render_font_family_row(
    row_id: SharedString,
    label: SharedString,
    access: AgentPanelStringAccess,
    cx: &mut App,
) -> AnyElement {
    let read = access.read;
    let write = access.write;
    let current = settings::SettingsStore::global(cx)
        .get_value_from_file(SettingsUiFile::User.to_settings(), read)
        .1
        .cloned();
    let is_set = current.is_some();
    let current_font = SharedString::from(current.unwrap_or_default());
    let handle = ui::PopoverMenuHandle::default();

    h_flex()
        .id(SharedString::from(format!("row-{row_id}")))
        .w_full()
        .py_1()
        .gap_2()
        .items_center()
        .justify_between()
        .child(Label::new(label.clone()).size(LabelSize::Small))
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    PopoverMenu::new(SharedString::from(format!("font-picker-{row_id}")))
                        .trigger(crate::wire_picker_trigger_a11y(
                            crate::render_picker_trigger_button(
                                SharedString::from(format!("font-trigger-{row_id}")),
                                if current_font.is_empty() {
                                    "Default".into()
                                } else {
                                    current_font.clone()
                                },
                            )
                            .aria_label(label),
                            handle.clone(),
                        ))
                        .menu({
                            move |window, cx| {
                                let current_font = current_font.clone();
                                Some(cx.new(move |cx| {
                                    crate::components::font_picker(
                                        current_font,
                                        move |font_name, window, cx| {
                                            let font_name = font_name.to_string();
                                            update_settings_file(
                                                SettingsUiFile::User,
                                                Some("agent_panel_styling"),
                                                window,
                                                cx,
                                                move |settings_content, _| {
                                                    write(settings_content, Some(font_name));
                                                },
                                            )
                                            .log_err();
                                        },
                                        window,
                                        cx,
                                    )
                                }))
                            }
                        })
                        .anchor(gpui::Anchor::TopLeft)
                        .with_handle(handle),
                )
                .when(is_set, |this| {
                    this.child(
                        IconButton::new(
                            SharedString::from(format!("reset-{row_id}")),
                            IconName::RotateCcw,
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .aria_label("Reset to default font")
                        .tooltip(Tooltip::text("Reset to default font"))
                        .on_click(move |_, window, cx| {
                            update_settings_file(
                                SettingsUiFile::User,
                                Some("agent_panel_styling"),
                                window,
                                cx,
                                move |settings_content, _| write(settings_content, None),
                            )
                            .log_err();
                        }),
                    )
                }),
        )
        .into_any_element()
}

fn render_agent_panel_section(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let mut children = Vec::new();
    for (group_index, (group, title)) in AGENT_PANEL_GROUPS.into_iter().enumerate() {
        children.push(
            Label::new(SharedString::new_static(title))
                .size(LabelSize::Small)
                .into_any_element(),
        );
        children.push(render_font_family_row(
            SharedString::from(format!("agent:{group}:font_family")),
            "Font Family".into(),
            agent_panel_font_family_access(group),
            cx,
        ));
        children.extend(render_setting_items(
            settings_window,
            agent_panel_font_items(group),
            AGENT_PANEL_ITEM_ID_BASE + group_index * 8,
            window,
            cx,
        ));
        for (is_background, label) in [(false, "Text Color"), (true, "Background")] {
            children.push(render_settings_color_row(
                settings_window,
                SharedString::from(format!("agent:{group}:{label}")),
                SharedString::new_static(label),
                agent_panel_color_access(group, is_background),
                cx,
            ));
        }
    }

    v_flex()
        .gap_1()
        .child(SettingsSectionHeader::new("Agent Panel Content").no_padding(true))
        .children(children)
}

/// A color row backed by a plain `Option<String>` settings field (the fork's
/// own color settings) rather than a theme token.
fn render_settings_color_row(
    settings_window: &SettingsWindow,
    row_id: EditingRowId,
    label: SharedString,
    access: AgentPanelStringAccess,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let read = access.read;
    let write = access.write;
    let override_value = settings::SettingsStore::global(cx)
        .get_value_from_file(SettingsUiFile::User.to_settings(), read)
        .1
        .cloned();
    let fallback = cx.theme().colors().text;
    let effective = parse_color_or(override_value.as_deref(), fallback);

    let commit = Rc::new(
        move |value: Option<String>, window: &mut Window, cx: &mut App| {
            update_settings_file(
                SettingsUiFile::User,
                Some("agent_panel_styling"),
                window,
                cx,
                move |settings_content, _| write(settings_content, value),
            )
            .log_err();
        },
    ) as Rc<dyn Fn(Option<String>, &mut Window, &mut App)>;

    render_color_row(
        settings_window,
        ColorRow {
            row_id,
            label,
            description: None,
            override_value,
            effective,
            commit,
        },
        cx,
    )
}

fn read_aloud_items() -> [SettingsPageItem; 5] {
    [
        SettingsPageItem::SettingItem(SettingItem {
            title: "Enabled",
            description: "Turn read-aloud playback on for agent responses.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("read_aloud.enabled"),
                pick: |settings_content| settings_content.read_aloud.as_ref()?.enabled.as_ref(),
                write: |settings_content, value, _| {
                    settings_content.read_aloud.get_or_insert_default().enabled = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Auto Play",
            description: "Start speaking as soon as the agent replies.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("read_aloud.auto_play"),
                pick: |settings_content| settings_content.read_aloud.as_ref()?.auto_play.as_ref(),
                write: |settings_content, value, _| {
                    settings_content
                        .read_aloud
                        .get_or_insert_default()
                        .auto_play = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Click to Seek",
            description: "Click a word in the response to jump playback there.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("read_aloud.click_to_seek"),
                pick: |settings_content| {
                    settings_content.read_aloud.as_ref()?.click_to_seek.as_ref()
                },
                write: |settings_content, value, _| {
                    settings_content
                        .read_aloud
                        .get_or_insert_default()
                        .click_to_seek = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Speaking Rate",
            description: "Playback speed multiplier.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("read_aloud.speaking_rate"),
                pick: |settings_content| {
                    settings_content.read_aloud.as_ref()?.speaking_rate.as_ref()
                },
                write: |settings_content, value, _| {
                    settings_content
                        .read_aloud
                        .get_or_insert_default()
                        .speaking_rate = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Voice",
            description: "Voice id passed to the speech provider.",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("read_aloud.voice_id"),
                pick: |settings_content| settings_content.read_aloud.as_ref()?.voice_id.as_ref(),
                write: |settings_content, value, _| {
                    settings_content.read_aloud.get_or_insert_default().voice_id = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
    ]
}

/// Reads the configured pill colors, padding to two entries so the gradient's
/// start and end can be edited independently.
pub(crate) fn pill_color_slots(configured: Option<&Vec<String>>) -> [Option<String>; 2] {
    let configured = configured.map(Vec::as_slice).unwrap_or_default();
    [configured.first().cloned(), configured.get(1).cloned()]
}

/// Produces the `pill_colors` value to persist after setting slot `slot` to
/// `value`. Trailing empty slots are dropped, and an entirely empty list
/// becomes `None` so the built-in palette is restored.
pub(crate) fn pill_colors_after_edit(
    configured: Option<&Vec<String>>,
    slot: usize,
    value: Option<String>,
) -> Option<Vec<String>> {
    let mut slots = pill_color_slots(configured);
    if let Some(entry) = slots.get_mut(slot) {
        *entry = value;
    }
    // A gradient end without a start is not representable, so promote it.
    if slots[0].is_none() {
        slots.swap(0, 1);
    }
    let colors: Vec<String> = slots.into_iter().flatten().collect();
    (!colors.is_empty()).then_some(colors)
}

fn render_read_aloud_section(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let configured = settings::SettingsStore::global(cx)
        .get_value_from_file(SettingsUiFile::User.to_settings(), |settings_content| {
            settings_content.read_aloud.as_ref()?.pill_colors.as_ref()
        })
        .1
        .cloned();
    let slots = pill_color_slots(configured.as_ref());
    let default_pill_colors = [cx.theme().colors().text_accent, cx.theme().status().info];

    let pill_rows = slots
        .into_iter()
        .enumerate()
        .map(|(slot, override_value)| {
            let effective = parse_color_or(override_value.as_deref(), default_pill_colors[slot]);
            let configured = configured.clone();
            let commit = Rc::new(
                move |value: Option<String>, window: &mut Window, cx: &mut App| {
                    let colors = pill_colors_after_edit(configured.as_ref(), slot, value);
                    update_settings_file(
                        SettingsUiFile::User,
                        Some("read_aloud.pill_colors"),
                        window,
                        cx,
                        move |settings_content, _| {
                            settings_content
                                .read_aloud
                                .get_or_insert_default()
                                .pill_colors = colors;
                        },
                    )
                    .log_err();
                },
            ) as Rc<dyn Fn(Option<String>, &mut Window, &mut App)>;

            render_color_row(
                settings_window,
                ColorRow {
                    row_id: SharedString::from(format!("read-aloud:pill:{slot}")),
                    label: if slot == 0 {
                        "Highlight Color".into()
                    } else {
                        "Gradient End (optional)".into()
                    },
                    description: (slot == 1)
                        .then(|| "Leave unset for a solid highlight instead of a gradient.".into()),
                    override_value,
                    effective,
                    commit,
                },
                cx,
            )
        })
        .collect::<Vec<_>>();

    v_flex()
        .gap_1()
        .child(SettingsSectionHeader::new("Read Aloud").no_padding(true))
        .children(render_setting_items(
            settings_window,
            read_aloud_items(),
            READ_ALOUD_ITEM_ID_BASE,
            window,
            cx,
        ))
        .children(pill_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_color_token_has_a_unique_key() {
        let mut seen = HashSet::new();
        for token in all_color_tokens() {
            assert!(
                seen.insert(token.key),
                "duplicate theme studio token key: {}",
                token.key
            );
        }
        assert_eq!(seen.len(), 185);
    }

    #[test]
    fn every_token_lands_in_a_known_group() {
        for token in all_color_tokens() {
            let group = group_for_token(token);
            assert!(
                TOKEN_GROUPS.contains(&group),
                "token {} mapped to unknown group {group}",
                token.key
            );
            assert_ne!(
                group, "Other",
                "token {} fell through to the catch-all group",
                token.key
            );
        }
    }

    #[test]
    fn every_token_is_reachable_by_searching_its_key() {
        for token in all_color_tokens() {
            let groups = grouped_color_tokens(token.key);
            let found = groups
                .iter()
                .flat_map(|(_, tokens)| tokens)
                .any(|candidate| candidate.key == token.key);
            assert!(found, "token {} is not reachable by search", token.key);
        }
    }

    #[test]
    fn search_is_case_insensitive_and_substring_based() {
        assert!(token_matches_query("editor.background", "BACKGROUND"));
        assert!(token_matches_query("editor.background", "  editor "));
        assert!(token_matches_query("editor.background", ""));
        assert!(!token_matches_query("editor.background", "terminal"));
    }

    #[test]
    fn empty_query_returns_every_token_grouped_in_page_order() {
        let groups = grouped_color_tokens("");
        let total: usize = groups.iter().map(|(_, tokens)| tokens.len()).sum();
        assert_eq!(total, all_color_tokens().count());

        let order: Vec<&str> = groups.iter().map(|(group, _)| *group).collect();
        let expected: Vec<&str> = TOKEN_GROUPS
            .iter()
            .copied()
            .filter(|group| order.contains(group))
            .collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn grouping_puts_representative_tokens_where_expected() {
        let group_of = |key: &str| {
            all_color_tokens()
                .find(|token| token.key == key)
                .map(group_for_token)
        };
        assert_eq!(group_of("editor.background"), Some("Editor"));
        assert_eq!(group_of("terminal.ansi.red"), Some("Terminal"));
        assert_eq!(group_of("border.focused"), Some("Borders"));
        assert_eq!(group_of("text.muted"), Some("Text & Icons"));
        assert_eq!(group_of("element.hover"), Some("Elements"));
        assert_eq!(group_of("panel.background"), Some("UI Chrome & Surfaces"));
        assert_eq!(
            group_of("minimap.thumb.border"),
            Some("Scrollbar & Minimap")
        );
        assert_eq!(group_of("vim.yank.background"), Some("Vim & Helix"));
        assert_eq!(group_of("version_control.added"), Some("Version Control"));
        // `deleted` exists in both flattened structs; the status one must not
        // be mistaken for the `version_control.deleted` color.
        assert_eq!(group_of("deleted"), Some("Status & Diagnostics"));
    }

    #[test]
    fn hex_round_trips_through_parse_and_format() {
        for input in ["#FF0000", "#123456", "#00FF0080", "#FFFFFF"] {
            let normalized = normalize_hex_input(input).expect("valid hex should normalize");
            assert_eq!(normalized, input);
            let parsed = theme::try_parse_color(&normalized).expect("normalized hex should parse");
            assert_eq!(format_color(parsed), input);
        }
    }

    #[test]
    fn hex_input_accepts_shorthand_and_missing_hash() {
        assert_eq!(normalize_hex_input("f00"), Some("#F00".to_string()));
        assert_eq!(
            normalize_hex_input(" #abcdef "),
            Some("#ABCDEF".to_string())
        );
        assert_eq!(
            normalize_hex_input("#abcdef").and_then(|hex| theme::try_parse_color(&hex).ok()),
            theme::try_parse_color("#ABCDEF").ok()
        );
    }

    #[test]
    fn hex_input_rejects_garbage_without_panicking() {
        for input in [
            "",
            "   ",
            "#",
            "#12345",
            "not a color",
            "#GGGGGG",
            "#1234567",
        ] {
            assert_eq!(
                normalize_hex_input(input),
                None,
                "expected {input:?} to be rejected"
            );
        }
    }

    #[test]
    fn writing_a_token_touches_only_that_token() {
        let token = all_color_tokens()
            .find(|token| token.key == "editor.background")
            .expect("editor.background exists");
        let mut style = ThemeStyleContent::default();
        (token.write)(&mut style, Some(ThemeColor::from("#123456")));

        assert_eq!(
            style.colors.editor_background.as_deref(),
            Some("#123456"),
            "the edited token should be set"
        );
        let others_written = all_color_tokens()
            .filter(|other| other.key != token.key)
            .filter(|other| (other.read)(&style).is_some())
            .count();
        assert_eq!(others_written, 0, "no other token should be written");
        assert!(style.syntax.is_empty());
        assert!(style.players.is_empty());
        assert!(style.accents.is_empty());
        assert!(style.window_background_appearance.is_none());
    }

    #[test]
    fn writing_a_token_serializes_to_exactly_one_json_key() {
        let token = all_color_tokens()
            .find(|token| token.key == "terminal.ansi.red")
            .expect("terminal.ansi.red exists");
        let mut style = ThemeStyleContent::default();
        (token.write)(&mut style, Some(ThemeColor::from("#ABCDEF")));

        let value = serde_json::to_value(&style).expect("style content serializes");
        let object = value.as_object().expect("style content is a JSON object");
        assert_eq!(
            object.keys().collect::<Vec<_>>(),
            vec!["terminal.ansi.red"],
            "only the edited token should appear in the serialized override"
        );
        assert_eq!(object["terminal.ansi.red"], serde_json::json!("#ABCDEF"));
    }

    /// The real write path: an edit must land at
    /// `theme_overrides.<theme>.<token>` and leave every other line of the
    /// user's settings file untouched.
    #[gpui::test]
    fn editing_a_token_produces_a_targeted_settings_edit(cx: &mut App) {
        let store = settings::SettingsStore::new(cx, &settings::default_settings());
        let token = all_color_tokens()
            .find(|token| token.key == "editor.background")
            .expect("editor.background exists");

        let existing = concat!(
            "{\n",
            "  \"theme\": \"One Dark\",\n",
            "  \"buffer_font_size\": 15,\n",
            "  \"read_aloud\": {\n",
            "    \"enabled\": true\n",
            "  }\n",
            "}\n",
        );

        let updated = store
            .new_text_for_update(existing.to_string(), |settings| {
                let entry = settings
                    .theme
                    .theme_overrides
                    .entry("One Dark".to_string())
                    .or_default();
                (token.write)(entry, Some(ThemeColor::from("#123456")));
            })
            .expect("the settings file updates");

        for preserved in [
            "\"theme\": \"One Dark\"",
            "\"buffer_font_size\": 15",
            "\"enabled\": true",
        ] {
            assert!(
                updated.contains(preserved),
                "expected {preserved} to survive the edit, got:\n{updated}"
            );
        }

        let value: serde_json::Value =
            settings::parse_json_with_comments(&updated).expect("the result is valid JSON");
        assert_eq!(
            value["theme_overrides"]["One Dark"]["editor.background"],
            serde_json::json!("#123456")
        );

        let unchanged = store
            .new_text_for_update(updated.clone(), |_| {})
            .expect("a no-op update succeeds");
        assert_eq!(unchanged, updated, "a no-op update rewrites nothing");
    }

    /// Resetting the last override for a theme removes the whole entry rather
    /// than leaving an empty object behind.
    #[gpui::test]
    fn resetting_all_removes_the_theme_entry(cx: &mut App) {
        let store = settings::SettingsStore::new(cx, &settings::default_settings());
        let existing = concat!(
            "{\n",
            "  \"buffer_font_size\": 15,\n",
            "  \"theme_overrides\": {\n",
            "    \"One Dark\": { \"editor.background\": \"#123456\" }\n",
            "  }\n",
            "}\n",
        );

        let updated = store
            .new_text_for_update(existing.to_string(), |settings| {
                settings.theme.theme_overrides.remove("One Dark");
            })
            .expect("the settings file updates");

        let value: serde_json::Value =
            settings::parse_json_with_comments(&updated).expect("the result is valid JSON");
        assert!(
            value["theme_overrides"]["One Dark"].is_null(),
            "the theme's overrides should be gone, got:\n{updated}"
        );
        assert!(updated.contains("\"buffer_font_size\": 15"));
    }

    #[test]
    fn resetting_a_token_clears_only_that_token() {
        let background = all_color_tokens()
            .find(|token| token.key == "editor.background")
            .expect("editor.background exists");
        let foreground = all_color_tokens()
            .find(|token| token.key == "editor.foreground")
            .expect("editor.foreground exists");

        let mut style = ThemeStyleContent::default();
        (background.write)(&mut style, Some(ThemeColor::from("#111111")));
        (foreground.write)(&mut style, Some(ThemeColor::from("#222222")));
        (background.write)(&mut style, None);

        assert!(style.colors.editor_background.is_none());
        assert_eq!(style.colors.editor_foreground.as_deref(), Some("#222222"));
    }

    #[test]
    fn syntax_edits_remove_the_entry_once_nothing_is_customized() {
        let mut style = ThemeStyleContent::default();
        update_syntax_style(&mut style, "keyword", |highlight| {
            highlight.color = Some(ThemeColor::from("#FF0000"));
        });
        assert!(style.syntax.contains_key("keyword"));

        update_syntax_style(&mut style, "keyword", |highlight| {
            highlight.font_weight = Some(FontWeightContent::BOLD);
        });
        update_syntax_style(&mut style, "keyword", |highlight| {
            highlight.color = None;
        });
        assert!(
            style.syntax.contains_key("keyword"),
            "the entry survives while a weight override remains"
        );

        update_syntax_style(&mut style, "keyword", |highlight| {
            highlight.font_weight = None;
        });
        assert!(
            !style.syntax.contains_key("keyword"),
            "the entry is dropped once it carries no customization"
        );
    }

    #[test]
    fn override_count_covers_colors_status_and_syntax() {
        let mut style = ThemeStyleContent::default();
        assert_eq!(override_count(Some(&style)), 0);
        assert_eq!(override_count(None), 0);

        style.colors.editor_background = Some(ThemeColor::from("#111111"));
        style.status.error = Some(ThemeColor::from("#FF0000"));
        update_syntax_style(&mut style, "keyword", |highlight| {
            highlight.color = Some(ThemeColor::from("#00FF00"));
        });
        assert_eq!(override_count(Some(&style)), 3);
    }

    #[test]
    fn pill_color_slots_pad_to_two_entries() {
        assert_eq!(pill_color_slots(None), [None, None]);
        assert_eq!(
            pill_color_slots(Some(&vec!["#A855F7".to_string()])),
            [Some("#A855F7".to_string()), None]
        );
        assert_eq!(
            pill_color_slots(Some(&vec![
                "#A855F7".to_string(),
                "#EC4899".to_string(),
                "#000000".to_string(),
            ])),
            [Some("#A855F7".to_string()), Some("#EC4899".to_string())]
        );
    }

    #[test]
    fn editing_pill_colors_keeps_the_list_well_formed() {
        let start = vec!["#A855F7".to_string()];
        assert_eq!(
            pill_colors_after_edit(Some(&start), 1, Some("#EC4899".to_string())),
            Some(vec!["#A855F7".to_string(), "#EC4899".to_string()])
        );

        let pair = vec!["#A855F7".to_string(), "#EC4899".to_string()];
        assert_eq!(
            pill_colors_after_edit(Some(&pair), 1, None),
            Some(vec!["#A855F7".to_string()]),
            "clearing the gradient end leaves a solid color"
        );
        assert_eq!(
            pill_colors_after_edit(Some(&pair), 0, None),
            Some(vec!["#EC4899".to_string()]),
            "clearing the start promotes the end so the list stays a valid solid color"
        );
        assert_eq!(
            pill_colors_after_edit(Some(&start), 0, None),
            None,
            "clearing the only color restores the built-in palette"
        );
        assert_eq!(
            pill_colors_after_edit(None, 0, Some("#123456".to_string())),
            Some(vec!["#123456".to_string()])
        );
    }
}
