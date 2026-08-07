//! Optional user styling for the agent panel's content types
//! (`agent_panel_styling` in settings). Every override is optional; anything
//! left unset keeps the theme-derived appearance unchanged.

use gpui::{Hsla, Pixels, SharedString, TextStyleRefinement, px};
use markdown::MarkdownStyle;
use settings::{AgentPanelContentStyle, RegisterSetting, Settings};

/// Configured font sizes are clamped to this range, in pixels.
pub const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 8.0..=32.0;

/// Resolved `agent_panel_styling` settings, one override group per content
/// type rendered in the agent panel.
#[derive(Clone, Debug, Default, PartialEq, RegisterSetting)]
pub struct AgentPanelStylingSettings {
    pub assistant_prose: ContentStyleOverrides,
    pub thinking: ContentStyleOverrides,
    pub tool_output: ContentStyleOverrides,
    pub user_message: ContentStyleOverrides,
    pub code_blocks: ContentStyleOverrides,
}

/// A content type's resolved overrides. `None` everywhere means "no visual
/// change": apply functions leave the corresponding style field untouched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContentStyleOverrides {
    pub font_family: Option<SharedString>,
    pub font_size: Option<Pixels>,
    pub text_color: Option<Hsla>,
    pub background: Option<Hsla>,
}

impl ContentStyleOverrides {
    /// Applies the overrides to a markdown style's base text and container.
    pub fn apply_to_markdown_style(&self, style: &mut MarkdownStyle) {
        self.apply_text_to_markdown_style(style);
        if let Some(background) = self.background {
            style.container_style.background = Some(background.into());
        }
    }

    /// Applies only the text overrides (font and color), for surfaces like
    /// tool-call labels whose background belongs to surrounding chrome.
    pub fn apply_text_to_markdown_style(&self, style: &mut MarkdownStyle) {
        if let Some(font_family) = &self.font_family {
            style.base_text_style.font_family = font_family.clone();
        }
        if let Some(font_size) = self.font_size {
            style.base_text_style.font_size = font_size.into();
        }
        if let Some(text_color) = self.text_color {
            style.base_text_style.color = text_color;
        }
    }

    /// Applies the overrides to a markdown style's fenced-code-block
    /// refinement, leaving inline code and prose alone.
    pub fn apply_to_code_blocks(&self, style: &mut MarkdownStyle) {
        if let Some(font_family) = &self.font_family {
            style.code_block.text.font_family = Some(font_family.clone());
        }
        if let Some(font_size) = self.font_size {
            style.code_block.text.font_size = Some(font_size.into());
        }
        if let Some(text_color) = self.text_color {
            style.code_block.text.color = Some(text_color);
        }
        if let Some(background) = self.background {
            style.code_block.background = Some(background.into());
        }
    }

    /// The text overrides as an editor refinement (user-message editors and
    /// tool-call diff editors). Background is not part of a text refinement;
    /// the call sites apply it to their own containers.
    pub fn text_style_refinement(&self) -> TextStyleRefinement {
        TextStyleRefinement {
            font_family: self.font_family.clone(),
            font_size: self.font_size.map(Into::into),
            color: self.text_color,
            ..Default::default()
        }
    }
}

impl Settings for AgentPanelStylingSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let styling = content.agent_panel_styling.as_ref();
        AgentPanelStylingSettings {
            assistant_prose: resolve_content_style(
                styling.and_then(|styling| styling.assistant_prose.as_ref()),
                "assistant_prose",
            ),
            thinking: resolve_content_style(
                styling.and_then(|styling| styling.thinking.as_ref()),
                "thinking",
            ),
            tool_output: resolve_content_style(
                styling.and_then(|styling| styling.tool_output.as_ref()),
                "tool_output",
            ),
            user_message: resolve_content_style(
                styling.and_then(|styling| styling.user_message.as_ref()),
                "user_message",
            ),
            code_blocks: resolve_content_style(
                styling.and_then(|styling| styling.code_blocks.as_ref()),
                "code_blocks",
            ),
        }
    }
}

/// Resolution only runs when settings (re)load, so an invalid value warns
/// once per bad edit rather than in any render path — the same policy as
/// `read_aloud.pill_colors`.
fn resolve_content_style(
    content: Option<&AgentPanelContentStyle>,
    group: &'static str,
) -> ContentStyleOverrides {
    let Some(content) = content else {
        return ContentStyleOverrides::default();
    };
    ContentStyleOverrides {
        font_family: content
            .font_family
            .as_deref()
            .map(str::trim)
            .filter(|family| !family.is_empty())
            .map(|family| SharedString::from(family.to_string())),
        font_size: content
            .font_size
            .and_then(|size| resolve_font_size(size, group)),
        text_color: resolve_color(content.text_color.as_deref(), group, "text_color"),
        background: resolve_color(content.background.as_deref(), group, "background"),
    }
}

fn resolve_font_size(size: f32, group: &'static str) -> Option<Pixels> {
    if !size.is_finite() {
        log::warn!(
            "agent_panel_styling.{group}.font_size {size:?} is not a finite number; ignoring it"
        );
        return None;
    }
    Some(px(size.clamp(
        *FONT_SIZE_RANGE.start(),
        *FONT_SIZE_RANGE.end(),
    )))
}

fn resolve_color(color: Option<&str>, group: &'static str, field: &'static str) -> Option<Hsla> {
    let color = color?;
    match read_aloud::parse_hex_color(color) {
        Some(parsed) => Some(parsed),
        None => {
            log::warn!(
                "agent_panel_styling.{group}.{field} {color:?} is not a valid hex color; \
                 keeping the theme color"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgb;
    use settings::{AgentPanelStylingContent, SettingsContent};

    fn settings_with(styling: Option<AgentPanelStylingContent>) -> AgentPanelStylingSettings {
        let content = SettingsContent {
            agent_panel_styling: styling,
            ..SettingsContent::default()
        };
        AgentPanelStylingSettings::from_settings(&content)
    }

    #[test]
    fn absent_block_resolves_to_no_overrides() {
        let settings = settings_with(None);
        assert_eq!(settings, AgentPanelStylingSettings::default());

        let empty: AgentPanelStylingContent = serde_json::from_str("{}").unwrap();
        assert_eq!(settings_with(Some(empty)), AgentPanelStylingSettings::default());
    }

    #[test]
    fn full_block_deserializes_and_resolves() {
        let content: AgentPanelStylingContent = serde_json::from_str(
            r##"{
                "assistant_prose": {
                    "font_family": "Iosevka",
                    "font_size": 15,
                    "text_color": "#AABBCC",
                    "background": "#112233"
                },
                "thinking": { "text_color": "#ABC" },
                "tool_output": { "font_size": 11 },
                "user_message": { "background": "#FFFFFF" },
                "code_blocks": { "font_family": "Zed Mono", "font_size": 13 }
            }"##,
        )
        .unwrap();
        let settings = settings_with(Some(content));

        assert_eq!(
            settings.assistant_prose,
            ContentStyleOverrides {
                font_family: Some("Iosevka".into()),
                font_size: Some(px(15.)),
                text_color: Some(rgb(0xAABBCC).into()),
                background: Some(rgb(0x112233).into()),
            }
        );
        assert_eq!(settings.thinking.text_color, Some(rgb(0xAABBCC).into()));
        assert_eq!(settings.tool_output.font_size, Some(px(11.)));
        assert_eq!(settings.user_message.background, Some(rgb(0xFFFFFF).into()));
        assert_eq!(settings.code_blocks.font_family, Some("Zed Mono".into()));
        assert_eq!(settings.code_blocks.font_size, Some(px(13.)));
    }

    #[test]
    fn partial_group_leaves_other_fields_none() {
        let content: AgentPanelStylingContent =
            serde_json::from_str(r##"{ "assistant_prose": { "font_size": 20 } }"##).unwrap();
        let settings = settings_with(Some(content));

        assert_eq!(settings.assistant_prose.font_size, Some(px(20.)));
        assert_eq!(settings.assistant_prose.font_family, None);
        assert_eq!(settings.assistant_prose.text_color, None);
        assert_eq!(settings.assistant_prose.background, None);
        assert_eq!(settings.thinking, ContentStyleOverrides::default());
    }

    #[test]
    fn font_sizes_clamp_to_the_sane_range() {
        let content: AgentPanelStylingContent = serde_json::from_str(
            r##"{
                "assistant_prose": { "font_size": 4 },
                "thinking": { "font_size": 100 },
                "tool_output": { "font_size": 8 }
            }"##,
        )
        .unwrap();
        let settings = settings_with(Some(content));

        assert_eq!(settings.assistant_prose.font_size, Some(px(8.)));
        assert_eq!(settings.thinking.font_size, Some(px(32.)));
        assert_eq!(settings.tool_output.font_size, Some(px(8.)));
    }

    #[test]
    fn non_finite_font_size_is_ignored() {
        assert_eq!(resolve_font_size(f32::NAN, "assistant_prose"), None);
        assert_eq!(resolve_font_size(f32::INFINITY, "assistant_prose"), None);
    }

    #[test]
    fn invalid_colors_fall_back_to_the_theme() {
        let content: AgentPanelStylingContent = serde_json::from_str(
            r##"{
                "assistant_prose": { "text_color": "not-a-color", "background": "#GGHHII" }
            }"##,
        )
        .unwrap();
        let settings = settings_with(Some(content));

        assert_eq!(settings.assistant_prose.text_color, None);
        assert_eq!(settings.assistant_prose.background, None);
    }

    #[test]
    fn blank_font_family_is_ignored() {
        let content: AgentPanelStylingContent =
            serde_json::from_str(r##"{ "assistant_prose": { "font_family": "   " } }"##).unwrap();
        let settings = settings_with(Some(content));
        assert_eq!(settings.assistant_prose.font_family, None);
    }

    #[test]
    fn markdown_style_application_touches_only_set_fields() {
        let mut style = MarkdownStyle::default();
        let untouched = MarkdownStyle::default();
        let overrides = ContentStyleOverrides {
            font_family: Some("Iosevka".into()),
            font_size: Some(px(15.)),
            text_color: Some(rgb(0xAABBCC).into()),
            background: None,
        };
        overrides.apply_to_markdown_style(&mut style);

        assert_eq!(style.base_text_style.font_family, SharedString::from("Iosevka"));
        assert_eq!(style.base_text_style.font_size, px(15.).into());
        assert_eq!(style.base_text_style.color, rgb(0xAABBCC).into());
        assert_eq!(style.container_style.background, untouched.container_style.background);
        assert_eq!(style.code_block, untouched.code_block);

        ContentStyleOverrides::default().apply_to_markdown_style(&mut style);
        assert_eq!(style.base_text_style.font_family, SharedString::from("Iosevka"));
    }

    #[test]
    fn code_block_application_refines_only_the_code_block() {
        let mut style = MarkdownStyle::default();
        let untouched = MarkdownStyle::default();
        let overrides = ContentStyleOverrides {
            font_family: Some("Zed Mono".into()),
            font_size: Some(px(13.)),
            text_color: Some(rgb(0x112233).into()),
            background: Some(rgb(0x001122).into()),
        };
        overrides.apply_to_code_blocks(&mut style);

        assert_eq!(style.code_block.text.font_family, Some("Zed Mono".into()));
        assert_eq!(style.code_block.text.font_size, Some(px(13.).into()));
        assert_eq!(style.code_block.text.color, Some(rgb(0x112233).into()));
        assert_eq!(style.code_block.background, Some(rgb(0x001122).into()));
        assert_eq!(style.base_text_style, untouched.base_text_style);
        assert_eq!(style.inline_code, untouched.inline_code);
    }

    #[test]
    fn text_style_refinement_carries_text_overrides_only() {
        let overrides = ContentStyleOverrides {
            font_family: Some("Iosevka".into()),
            font_size: Some(px(12.)),
            text_color: Some(rgb(0xAABBCC).into()),
            background: Some(rgb(0x112233).into()),
        };
        let refinement = overrides.text_style_refinement();
        assert_eq!(refinement.font_family, Some("Iosevka".into()));
        assert_eq!(refinement.font_size, Some(px(12.).into()));
        assert_eq!(refinement.color, Some(rgb(0xAABBCC).into()));
        assert_eq!(refinement.background_color, None);
    }
}
