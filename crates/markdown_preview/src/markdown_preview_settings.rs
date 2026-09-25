use gpui::Pixels;
use settings::{IntoGpui, MarkdownPreviewClickModifier, RegisterSetting, Settings};

/// The settings for the markdown preview.
#[derive(Clone, Copy, Debug, Default, RegisterSetting)]
pub struct MarkdownPreviewSettings {
    /// Whether to automatically open Markdown files in the preview.
    pub open_markdown_files_in_preview: bool,
    /// The maximum width of the rendered markdown content, or `None` to render
    /// content edge to edge.
    pub max_width: Option<Pixels>,
    /// The modifier key that triggers click-to-source navigation.
    pub click_to_source_modifier: MarkdownPreviewClickModifier,
}

impl Settings for MarkdownPreviewSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content.markdown_preview.clone().unwrap_or_default();
        let max_width = if content.limit_content_width.unwrap_or(true) {
            content.max_width.map(IntoGpui::into_gpui)
        } else {
            None
        };
        Self {
            open_markdown_files_in_preview: content.open_markdown_files_in_preview.unwrap_or(false),
            max_width,
            click_to_source_modifier: content.click_to_source_modifier.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn modifiers_with(control: bool, alt: bool, platform: bool, shift: bool) -> Modifiers {
        Modifiers {
            control,
            alt,
            shift,
            platform,
            function: false,
        }
    }

    #[test]
    fn click_modifier_defaults_to_alt() {
        assert_eq!(
            MarkdownPreviewClickModifier::default(),
            MarkdownPreviewClickModifier::Alt
        );
    }

    #[test]
    fn click_modifier_matches_configured_key() {
        let alt = modifiers_with(false, true, false, false);
        let ctrl = modifiers_with(true, false, false, false);
        let cmd = modifiers_with(false, false, true, false);
        let plain = modifiers_with(false, false, false, false);

        assert!(MarkdownPreviewClickModifier::Alt.matches(&alt));
        assert!(!MarkdownPreviewClickModifier::Alt.matches(&ctrl));
        assert!(!MarkdownPreviewClickModifier::Alt.matches(&plain));

        assert!(MarkdownPreviewClickModifier::Ctrl.matches(&ctrl));
        assert!(!MarkdownPreviewClickModifier::Ctrl.matches(&alt));

        assert!(MarkdownPreviewClickModifier::Cmd.matches(&cmd));
        assert!(!MarkdownPreviewClickModifier::Cmd.matches(&ctrl));

        assert!(!MarkdownPreviewClickModifier::None.matches(&alt));
        assert!(!MarkdownPreviewClickModifier::None.matches(&plain));
    }

    #[test]
    fn click_modifier_parses_from_settings_content() {
        let content: settings::SettingsContent = serde_json::from_value(serde_json::json!({
            "markdown_preview": {"click_to_source_modifier": "cmd"}
        }))
        .unwrap();
        let settings = MarkdownPreviewSettings::from_settings(&content);
        assert_eq!(
            settings.click_to_source_modifier,
            MarkdownPreviewClickModifier::Cmd
        );

        let content: settings::SettingsContent =
            serde_json::from_value(serde_json::json!({"markdown_preview": {}})).unwrap();
        let settings = MarkdownPreviewSettings::from_settings(&content);
        assert_eq!(
            settings.click_to_source_modifier,
            MarkdownPreviewClickModifier::Alt
        );
    }
}
