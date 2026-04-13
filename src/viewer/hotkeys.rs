use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use gtk::gdk;
use gtk4 as gtk;

#[derive(Clone, Debug)]
pub(super) struct ViewerHotkeys {
    toggle_fullscreen: Hotkey,
    release_cursor: Hotkey,
    rotate_dmabuf_view: Hotkey,
    toggle_dmabuf_flip: Hotkey,
    reset_dmabuf_transform: Hotkey,
}

impl Default for ViewerHotkeys {
    fn default() -> Self {
        Self {
            toggle_fullscreen: Hotkey::key(gdk::Key::F11, gdk::ModifierType::empty()),
            release_cursor: Hotkey::modifiers(
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK,
            ),
            rotate_dmabuf_view: Hotkey::key(
                gdk::Key::r,
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK,
            ),
            toggle_dmabuf_flip: Hotkey::key(
                gdk::Key::f,
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK,
            ),
            reset_dmabuf_transform: Hotkey::key(
                gdk::Key::_0,
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK,
            ),
        }
    }
}

impl ViewerHotkeys {
    /// Parse virt-viewer style `action=accelerator` overrides while keeping the
    /// current defaults for any action the user did not mention.
    pub(super) fn parse(spec: Option<&str>) -> Result<Self> {
        let mut hotkeys = Self::default();
        let Some(spec) = spec.map(str::trim) else {
            return Ok(hotkeys);
        };
        if spec.is_empty() {
            return Ok(hotkeys);
        }

        let mut seen_actions = HashSet::new();
        for assignment in spec.split(',') {
            let assignment = assignment.trim();
            if assignment.is_empty() {
                bail!("`--hotkeys` contains an empty assignment");
            }

            let (action, binding) = assignment.split_once('=').with_context(|| {
                format!("invalid hotkey override `{assignment}`; expected `action=accelerator`")
            })?;

            let action = normalize_action_name(action);
            if !seen_actions.insert(action.clone()) {
                bail!("hotkey action `{action}` was specified more than once");
            }

            let binding = parse_binding(binding)
                .with_context(|| format!("invalid hotkey for action `{action}`"))?;
            hotkeys.set_binding(&action, binding)?;
        }

        hotkeys.validate_conflicts()?;
        Ok(hotkeys)
    }

    pub(super) fn toggle_fullscreen(&self) -> &Hotkey {
        &self.toggle_fullscreen
    }

    pub(super) fn release_cursor(&self) -> &Hotkey {
        &self.release_cursor
    }

    pub(super) fn rotate_dmabuf_view(&self) -> &Hotkey {
        &self.rotate_dmabuf_view
    }

    pub(super) fn toggle_dmabuf_flip(&self) -> &Hotkey {
        &self.toggle_dmabuf_flip
    }

    pub(super) fn reset_dmabuf_transform(&self) -> &Hotkey {
        &self.reset_dmabuf_transform
    }

    pub(super) fn shortcuts_for_dialog(&self) -> Vec<(&'static str, String)> {
        [
            ("Release keyboard and mouse", &self.release_cursor),
            ("Toggle fullscreen", &self.toggle_fullscreen),
            ("Rotate DMABUF view", &self.rotate_dmabuf_view),
            ("Toggle DMABUF vertical flip", &self.toggle_dmabuf_flip),
            ("Reset DMABUF transform", &self.reset_dmabuf_transform),
        ]
        .into_iter()
        .filter_map(|(title, binding)| binding.accelerator().map(|value| (title, value.to_owned())))
        .collect()
    }

    fn set_binding(&mut self, action: &str, binding: Hotkey) -> Result<()> {
        match action {
            "toggle-fullscreen" => self.toggle_fullscreen = binding,
            "release-cursor" | "release-grab" => self.release_cursor = binding,
            "rotate-dmabuf-view" => self.rotate_dmabuf_view = binding,
            "toggle-dmabuf-flip" => self.toggle_dmabuf_flip = binding,
            "reset-dmabuf-transform" => self.reset_dmabuf_transform = binding,
            _ => bail!(
                "unknown hotkey action `{action}`; supported actions are: \
toggle-fullscreen, release-cursor, rotate-dmabuf-view, \
toggle-dmabuf-flip, reset-dmabuf-transform"
            ),
        }

        Ok(())
    }

    fn validate_conflicts(&self) -> Result<()> {
        let mut seen = HashMap::new();
        for (action, binding) in [
            ("toggle-fullscreen", &self.toggle_fullscreen),
            ("release-cursor", &self.release_cursor),
            ("rotate-dmabuf-view", &self.rotate_dmabuf_view),
            ("toggle-dmabuf-flip", &self.toggle_dmabuf_flip),
            ("reset-dmabuf-transform", &self.reset_dmabuf_transform),
        ] {
            let Some(conflict_key) = binding.conflict_key() else {
                continue;
            };

            if let Some(previous_action) = seen.insert(conflict_key, action) {
                bail!(
                    "hotkey conflict: `{action}` and `{previous_action}` are both bound to `{}`",
                    binding.describe()
                );
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(super) enum Hotkey {
    Disabled,
    Key {
        key: gdk::Key,
        modifiers: gdk::ModifierType,
        accelerator: String,
    },
    Modifiers {
        modifiers: gdk::ModifierType,
        accelerator: String,
    },
}

impl Hotkey {
    fn key(key: gdk::Key, modifiers: gdk::ModifierType) -> Self {
        let key = normalize_key(key);
        let modifiers = sanitize_modifiers(modifiers);
        Self::Key {
            accelerator: format!("{}{}", accelerator_prefix(modifiers), key.name().unwrap()),
            key,
            modifiers,
        }
    }

    fn modifiers(modifiers: gdk::ModifierType) -> Self {
        let modifiers = sanitize_modifiers(modifiers);
        Self::Modifiers {
            accelerator: accelerator_prefix(modifiers),
            modifiers,
        }
    }

    /// Match a GTK key event against a configured viewer hotkey, including
    /// modifier-only bindings like `Ctrl+Alt`.
    pub(super) fn matches(&self, keyval: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        match self {
            Self::Disabled => false,
            Self::Key {
                key,
                modifiers: expected_modifiers,
                ..
            } => {
                normalize_key(keyval) == *key
                    && sanitize_modifiers(modifiers) == *expected_modifiers
            }
            Self::Modifiers {
                modifiers: expected_modifiers,
                ..
            } => {
                let Some(current_modifier) = modifier_mask_for_key(keyval) else {
                    return false;
                };

                let combined = sanitize_modifiers(modifiers) | current_modifier;
                combined == *expected_modifiers && expected_modifiers.contains(current_modifier)
            }
        }
    }

    pub(super) fn accelerator(&self) -> Option<&str> {
        match self {
            Self::Disabled => None,
            Self::Key { accelerator, .. } | Self::Modifiers { accelerator, .. } => {
                Some(accelerator.as_str())
            }
        }
    }

    fn describe(&self) -> &str {
        self.accelerator().unwrap_or("disabled")
    }

    fn conflict_key(&self) -> Option<String> {
        match self {
            Self::Disabled => None,
            Self::Key { accelerator, .. } => Some(format!("key:{accelerator}")),
            Self::Modifiers { accelerator, .. } => Some(format!("mods:{accelerator}")),
        }
    }
}

fn parse_binding(raw: &str) -> Result<Hotkey> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("hotkey value cannot be empty");
    }

    if matches!(
        raw.to_ascii_lowercase().as_str(),
        "off" | "none" | "disabled"
    ) {
        return Ok(Hotkey::Disabled);
    }

    let mut modifiers = gdk::ModifierType::empty();
    let mut key = None;

    for part in raw.split('+') {
        let part = part.trim();
        if part.is_empty() {
            bail!("hotkey `{raw}` contains an empty token");
        }

        if let Some(modifier) = parse_modifier(part) {
            modifiers |= modifier;
            continue;
        }

        let parsed_key =
            parse_key(part).with_context(|| format!("unknown key name `{part}` in `{raw}`"))?;
        if key.replace(parsed_key).is_some() {
            bail!("hotkey `{raw}` may contain only one non-modifier key");
        }
    }

    if key.is_none() && modifiers.is_empty() {
        bail!("hotkey `{raw}` does not contain any key or modifier");
    }

    Ok(match key {
        Some(key) => Hotkey::key(key, modifiers),
        None => Hotkey::modifiers(modifiers),
    })
}

fn normalize_action_name(action: &str) -> String {
    action.trim().to_ascii_lowercase().replace('_', "-")
}

fn parse_modifier(token: &str) -> Option<gdk::ModifierType> {
    match token.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "ctl" => Some(gdk::ModifierType::CONTROL_MASK),
        "alt" => Some(gdk::ModifierType::ALT_MASK),
        "shift" => Some(gdk::ModifierType::SHIFT_MASK),
        "super" | "win" | "windows" | "logo" => Some(gdk::ModifierType::SUPER_MASK),
        "meta" => Some(gdk::ModifierType::META_MASK),
        "hyper" => Some(gdk::ModifierType::HYPER_MASK),
        _ => None,
    }
}

fn parse_key(token: &str) -> Option<gdk::Key> {
    let lower = token.trim().to_ascii_lowercase();
    let canonical = match lower.as_str() {
        "enter" | "return" => "Return".to_owned(),
        "esc" | "escape" => "Escape".to_owned(),
        "space" | "spacebar" => "space".to_owned(),
        "backspace" => "BackSpace".to_owned(),
        "tab" => "Tab".to_owned(),
        "left" => "Left".to_owned(),
        "right" => "Right".to_owned(),
        "up" => "Up".to_owned(),
        "down" => "Down".to_owned(),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        "insert" | "ins" => "Insert".to_owned(),
        "delete" | "del" => "Delete".to_owned(),
        "pageup" | "page-up" => "Page_Up".to_owned(),
        "pagedown" | "page-down" => "Page_Down".to_owned(),
        "plus" => "plus".to_owned(),
        "minus" | "dash" => "minus".to_owned(),
        "equal" | "equals" => "equal".to_owned(),
        "comma" => "comma".to_owned(),
        "period" | "dot" => "period".to_owned(),
        "slash" => "slash".to_owned(),
        "backslash" => "backslash".to_owned(),
        "semicolon" => "semicolon".to_owned(),
        "apostrophe" | "quote" => "apostrophe".to_owned(),
        _ if lower.starts_with('f') && lower[1..].chars().all(|ch| ch.is_ascii_digit()) => {
            format!("F{}", &lower[1..])
        }
        _ => lower,
    };

    gdk::Key::from_name(&canonical).map(normalize_key)
}

fn normalize_key(key: gdk::Key) -> gdk::Key {
    key.to_lower()
}

fn sanitize_modifiers(modifiers: gdk::ModifierType) -> gdk::ModifierType {
    modifiers
        & (gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::META_MASK
            | gdk::ModifierType::HYPER_MASK)
}

fn modifier_mask_for_key(keyval: gdk::Key) -> Option<gdk::ModifierType> {
    match keyval {
        gdk::Key::Control_L | gdk::Key::Control_R => Some(gdk::ModifierType::CONTROL_MASK),
        gdk::Key::Alt_L | gdk::Key::Alt_R => Some(gdk::ModifierType::ALT_MASK),
        gdk::Key::Shift_L | gdk::Key::Shift_R => Some(gdk::ModifierType::SHIFT_MASK),
        gdk::Key::Super_L | gdk::Key::Super_R => Some(gdk::ModifierType::SUPER_MASK),
        gdk::Key::Meta_L | gdk::Key::Meta_R => Some(gdk::ModifierType::META_MASK),
        gdk::Key::Hyper_L | gdk::Key::Hyper_R => Some(gdk::ModifierType::HYPER_MASK),
        _ => None,
    }
}

fn accelerator_prefix(modifiers: gdk::ModifierType) -> String {
    let mut accelerator = String::new();
    for (mask, label) in [
        (gdk::ModifierType::CONTROL_MASK, "<Control>"),
        (gdk::ModifierType::ALT_MASK, "<Alt>"),
        (gdk::ModifierType::SHIFT_MASK, "<Shift>"),
        (gdk::ModifierType::SUPER_MASK, "<Super>"),
        (gdk::ModifierType::META_MASK, "<Meta>"),
        (gdk::ModifierType::HYPER_MASK, "<Hyper>"),
    ] {
        if modifiers.contains(mask) {
            accelerator.push_str(label);
        }
    }
    accelerator
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use gtk::gdk;
    use gtk4 as gtk;

    use super::ViewerHotkeys;

    #[test]
    fn default_release_cursor_binding_matches_modifier_chord() {
        let hotkeys = ViewerHotkeys::default();

        assert!(
            hotkeys
                .release_cursor()
                .matches(gdk::Key::Alt_L, gdk::ModifierType::CONTROL_MASK)
        );
        assert!(
            hotkeys
                .release_cursor()
                .matches(gdk::Key::Control_L, gdk::ModifierType::ALT_MASK)
        );
        assert!(
            !hotkeys
                .release_cursor()
                .matches(gdk::Key::Control_L, gdk::ModifierType::empty())
        );
    }

    #[test]
    fn hotkey_overrides_replace_defaults() -> Result<()> {
        let hotkeys = ViewerHotkeys::parse(Some(
            "toggle-fullscreen=ctrl+enter,release-cursor=ctrl+shift",
        ))?;

        assert!(
            hotkeys
                .toggle_fullscreen()
                .matches(gdk::Key::Return, gdk::ModifierType::CONTROL_MASK)
        );
        assert!(
            hotkeys
                .release_cursor()
                .matches(gdk::Key::Shift_L, gdk::ModifierType::CONTROL_MASK)
        );
        assert!(
            !hotkeys
                .toggle_fullscreen()
                .matches(gdk::Key::F11, gdk::ModifierType::empty())
        );

        Ok(())
    }

    #[test]
    fn fullscreen_shortcuts_dialog_only_lists_toggle_action() -> Result<()> {
        let hotkeys = ViewerHotkeys::parse(Some("toggle-fullscreen=ctrl+enter"))?;
        let shortcuts = hotkeys.shortcuts_for_dialog();

        assert!(
            shortcuts
                .iter()
                .any(|(title, _)| *title == "Toggle fullscreen")
        );
        assert!(
            !shortcuts
                .iter()
                .any(|(title, _)| *title == "Leave fullscreen")
        );

        Ok(())
    }

    #[test]
    fn duplicate_bindings_are_rejected() {
        let error = ViewerHotkeys::parse(Some("toggle-fullscreen=f11,release-cursor=f11"))
            .expect_err("conflicting bindings should be rejected");

        assert!(error.to_string().contains("hotkey conflict"));
    }

    #[test]
    fn duplicate_actions_are_rejected() {
        let error = ViewerHotkeys::parse(Some("toggle-fullscreen=f11,toggle_fullscreen=escape"))
            .expect_err("duplicate actions should be rejected");

        assert!(error.to_string().contains("specified more than once"));
    }

    #[test]
    fn unknown_actions_are_rejected() {
        let error =
            ViewerHotkeys::parse(Some("something-else=ctrl+enter")).expect_err("unknown action");

        assert!(error.to_string().contains("unknown hotkey action"));
    }
}
