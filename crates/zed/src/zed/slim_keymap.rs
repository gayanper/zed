use gpui::{App, KeyBinding};
use settings::{KeybindSource, KeymapFile, KeymapFileLoadResult, SettingsAssets};
use util::asset_str;

// Built-in keymaps are shared with upstream, so bindings for features compiled
// out of a slim build are dropped here instead of being edited out of the JSON
// files, which would conflict on every upstream merge. Entries ending in `::`
// match a whole namespace; other entries match a single action.
// The collab_ui namespaces already fail to load in a slim build; they are
// listed to document intent and to keep the rename test meaningful.
const COLLAB_ACTIONS: &[&str] = &[
    "channel_modal::",
    "collab_panel::",
    "workspace::FollowNextCollaborator",
];

const ZED_CLOUD_ACTIONS: &[&str] = &["edit_prediction::RatePredictions", "zeta::"];

// Only the conversation view of native/ACP threads goes away; panel, sidebar
// and thread-list actions still serve terminal threads. Model selection actions
// stay because the inline assistant binds them too.
const AGENT_THREAD_ACTIONS: &[&str] = &[
    "agent::AddSelectionToThread",
    "agent::AllowAlways",
    "agent::AllowOnce",
    "agent::Chat",
    "agent::ChatWithFollow",
    "agent::ClearMessageQueue",
    "agent::CycleModeSelector",
    "agent::CycleThinkingEffort",
    "agent::EditFirstQueuedMessage",
    "agent::ExpandMessageEditor",
    "agent::Keep",
    "agent::KeepAll",
    "agent::ManageProfiles",
    "agent::OpenAddContextMenu",
    "agent::OpenAgentDiff",
    "agent::OpenPermissionDropdown",
    "agent::PasteRaw",
    "agent::Reject",
    "agent::RejectAll",
    "agent::RejectOnce",
    "agent::RemoveFirstQueuedMessage",
    "agent::ScrollOutputLineDown",
    "agent::ScrollOutputLineUp",
    "agent::ScrollOutputPageDown",
    "agent::ScrollOutputPageUp",
    "agent::ScrollOutputToBottom",
    "agent::ScrollOutputToNextMessage",
    "agent::ScrollOutputToPreviousMessage",
    "agent::ScrollOutputToTop",
    "agent::SendImmediately",
    "agent::SendNextQueuedMessage",
    "agent::ToggleFastMode",
    "agent::ToggleProfileSelector",
    "agent::ToggleSteerFirstQueuedMessage",
    "agent::ToggleThinkingEffortMenu",
    "agent::ToggleThinkingMode",
    "agent::UndoLastReject",
];

const IS_SLIM: bool = !cfg!(feature = "collab")
    || !cfg!(feature = "zed-cloud")
    || !cfg!(feature = "agent-threads")
    || !cfg!(feature = "copilot")
    || !cfg!(feature = "hosted-llms");

/// Loads a built-in keymap. A slim build does not link some crates whose
/// actions the keymaps reference (e.g. `collab_panel::`), so their bindings fail
/// to load; those are tolerated there, while the full build stays strict.
pub(crate) fn load_builtin_keymap(
    asset_path: &str,
    source: KeybindSource,
    cx: &App,
) -> Vec<KeyBinding> {
    if !IS_SLIM {
        return KeymapFile::load_asset(asset_path, Some(source), cx).unwrap();
    }
    let mut bindings = match KeymapFile::load(asset_str::<SettingsAssets>(asset_path).as_ref(), cx)
    {
        KeymapFileLoadResult::Success { key_bindings } => key_bindings,
        KeymapFileLoadResult::SomeFailedToLoad {
            key_bindings,
            error_message,
        } => {
            log::warn!("Skipped bindings in built-in keymap \"{asset_path}\": {error_message}");
            key_bindings
        }
        KeymapFileLoadResult::JsonParseFailure { error } => {
            panic!("JSON parse error in built-in keymap \"{asset_path}\": {error}")
        }
    };
    for binding in &mut bindings {
        binding.set_meta(source.meta());
    }
    filter_slim_bindings(bindings)
}

fn is_excluded(action_name: &str) -> bool {
    let groups = [
        (!cfg!(feature = "collab"), COLLAB_ACTIONS),
        (!cfg!(feature = "zed-cloud"), ZED_CLOUD_ACTIONS),
        (!cfg!(feature = "agent-threads"), AGENT_THREAD_ACTIONS),
    ];
    groups
        .iter()
        .filter(|(excluded, _)| *excluded)
        .flat_map(|(_, entries)| entries.iter())
        .any(|entry| {
            if entry.ends_with("::") {
                action_name.starts_with(entry)
            } else {
                action_name == *entry
            }
        })
}

fn filter_slim_bindings(bindings: Vec<KeyBinding>) -> Vec<KeyBinding> {
    bindings
        .into_iter()
        .filter(|binding| !is_excluded(binding.action().name()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches upstream renames that would silently stop an entry from matching.
    #[test]
    fn every_entry_matches_a_builtin_binding() {
        let keymaps = [
            include_str!("../../../../assets/keymaps/default-macos.json"),
            include_str!("../../../../assets/keymaps/default-linux.json"),
            include_str!("../../../../assets/keymaps/default-windows.json"),
        ]
        .concat();
        for entry in COLLAB_ACTIONS
            .iter()
            .chain(ZED_CLOUD_ACTIONS)
            .chain(AGENT_THREAD_ACTIONS)
        {
            let pattern = if entry.ends_with("::") {
                format!("\"{entry}")
            } else {
                format!("\"{entry}\"")
            };
            assert!(
                keymaps.contains(&pattern),
                "{entry} is not bound in any built-in keymap"
            );
        }
    }
}
