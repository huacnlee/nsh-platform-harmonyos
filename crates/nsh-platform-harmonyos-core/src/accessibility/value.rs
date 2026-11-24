//! Spoken and ranged accessibility values for ArkUI nodes and virtual providers.

use nsh_core::{Accessibility, AccessibilityRole, Checked};

/// Canonical ArkUI / accessibility-provider role name shared across ArkTS
/// wrappers and the native virtual provider.
pub(crate) fn role_str(role: AccessibilityRole) -> &'static str {
    match role {
        AccessibilityRole::None => "",
        AccessibilityRole::Button
        | AccessibilityRole::ImageButton
        | AccessibilityRole::KeyboardKey
        | AccessibilityRole::Option => "button",
        AccessibilityRole::Checkbox => "checkbox",
        AccessibilityRole::Switch => "switch",
        AccessibilityRole::Radio => "radio",
        AccessibilityRole::Adjustable | AccessibilityRole::Scrollbar => "slider",
        AccessibilityRole::Image => "image",
        AccessibilityRole::Link => "link",
        AccessibilityRole::List | AccessibilityRole::Listbox => "list",
        AccessibilityRole::Grid => "grid",
        AccessibilityRole::Progressbar => "progress",
        AccessibilityRole::Search => "search",
        AccessibilityRole::Menuitem => "menuitem",
        _ => "text",
    }
}

fn state_words(
    locale: &str,
) -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    match locale
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "zh" => ("已选中", "未选中", "部分选中", "已展开", "已折叠", "忙碌"),
        "ja" => (
            "選択済み",
            "未選択",
            "一部選択",
            "展開中",
            "折りたたみ中",
            "処理中",
        ),
        "ko" => (
            "선택됨",
            "선택 안 됨",
            "일부 선택됨",
            "펼쳐짐",
            "접힘",
            "사용 중",
        ),
        "es" => (
            "seleccionado",
            "no seleccionado",
            "mixto",
            "expandido",
            "contraído",
            "ocupado",
        ),
        "fr" => (
            "coché",
            "non coché",
            "mixte",
            "développé",
            "réduit",
            "occupé",
        ),
        "de" => (
            "aktiviert",
            "nicht aktiviert",
            "gemischt",
            "erweitert",
            "reduziert",
            "beschäftigt",
        ),
        _ => (
            "checked",
            "not checked",
            "mixed",
            "expanded",
            "collapsed",
            "busy",
        ),
    }
}

/// Complete numeric range (`(min, max, current)`) accepted by ArkUI's
/// `NODE_ACCESSIBILITY_VALUE` setter, which rejects partial ranges or
/// out-of-bounds `current` with `ARKUI_ERROR_CODE_PARAM_INVALID`.
pub(crate) fn range_value(accessibility: &Accessibility) -> Option<(i32, i32, i32)> {
    let value = accessibility.value.as_ref()?;
    let (min, max, now) = (value.min?, value.max?, value.now?);
    if !min.is_finite() || !max.is_finite() || !now.is_finite() || max < min {
        return None;
    }
    let min = min.floor() as i32;
    let max = (max.ceil() as i32).max(min);
    let current = (now.round() as i32).clamp(min, max);
    Some((min, max, current))
}

#[cfg(test)]
pub(crate) fn spoken_value(accessibility: &Accessibility) -> Option<String> {
    spoken_value_for_locale(accessibility, "en")
}

pub(crate) fn spoken_value_for_locale(
    accessibility: &Accessibility,
    locale: &str,
) -> Option<String> {
    let (checked, not_checked, mixed, expanded, collapsed, busy) = state_words(locale);
    let mut parts = Vec::new();
    if let Some(value) = &accessibility.value {
        if let Some(text) = &value.text {
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        } else if let Some(now) = value.now {
            parts.push(match (value.min, value.max) {
                (Some(min), Some(max)) if max > min => {
                    format!("{}%", (((now - min) / (max - min)) * 100.0).round())
                }
                _ => now.to_string(),
            });
        }
    }
    match accessibility.state.checked {
        Some(Checked::True) => parts.push(checked.into()),
        Some(Checked::False) => parts.push(not_checked.into()),
        Some(Checked::Mixed) => parts.push(mixed.into()),
        None => {}
    }
    match accessibility.state.expanded {
        Some(true) => parts.push(expanded.into()),
        Some(false) => parts.push(collapsed.into()),
        None => {}
    }
    if accessibility.state.busy {
        parts.push(busy.into());
    }
    if accessibility.state.has_popup {
        // Use the app's localized popup action, never a built-in control label.
        if let Some(label) = accessibility
            .actions
            .iter()
            .find(|action| action.name.as_ref() == "activate")
            .and_then(|action| action.label.as_deref())
            .filter(|label| !label.is_empty())
        {
            parts.push(label.to_owned());
        }
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(crate) fn owns_semantics(spec: &Accessibility) -> bool {
    wrapper_semantics(spec, cfg!(feature = "api-23"), cfg!(feature = "api-26"))
}

fn wrapper_semantics(spec: &Accessibility, api23: bool, api26: bool) -> bool {
    spec.state.checked.is_some()
        || (!api23 && spec.state.selected)
        || (api26
            && spec.actions.iter().any(|action| {
                !matches!(
                    action.name.as_ref(),
                    "activate" | "longpress" | "copy" | "cut" | "paste" | "increment" | "decrement"
                )
            }))
}

#[cfg(test)]
mod tests {
    use nsh_core::{AccessibilityState, AccessibilityValue};

    use super::*;

    #[test]
    fn api20_selected_nodes_keep_wrapper_semantics_without_dead_adjustable_wrapper() {
        let mut spec = Accessibility::default();
        assert!(!owns_semantics(&spec));
        spec.state.selected = true;
        assert!(wrapper_semantics(&spec, false, false));
        assert!(!wrapper_semantics(&spec, true, false));
        spec.state.selected = false;
        spec.actions.push(nsh_core::AccessibilityActionDef {
            name: "increment".into(),
            label: None,
        });
        // ArkTS wrapper has no increment/decrement dispatch on API 20; avoid wrapping it.
        assert!(!wrapper_semantics(&spec, false, false));
        assert!(!wrapper_semantics(&spec, true, false));
    }

    #[test]
    fn localized_state_words_and_role_mapping_cover_all_roles() {
        let accessibility = Accessibility {
            state: AccessibilityState {
                checked: Some(Checked::Mixed),
                expanded: Some(true),
                busy: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            spoken_value_for_locale(&accessibility, "zh-Hans-CN").as_deref(),
            Some("部分选中, 已展开, 忙碌")
        );
        assert_eq!(role_str(AccessibilityRole::Link), "link");
        assert_eq!(role_str(AccessibilityRole::Image), "image");
        assert_eq!(role_str(AccessibilityRole::Grid), "grid");
        assert_eq!(role_str(AccessibilityRole::Progressbar), "progress");
        assert_eq!(role_str(AccessibilityRole::Search), "search");
        assert_eq!(role_str(AccessibilityRole::Menuitem), "menuitem");
        assert_eq!(role_str(AccessibilityRole::Listbox), "list");
        assert_eq!(role_str(AccessibilityRole::Option), "button");
    }

    #[test]
    fn popup_action_uses_the_apps_label_and_clears_with_state() {
        let mut spec = Accessibility::default();
        spec.state.has_popup = true;
        spec.actions.push(nsh_core::AccessibilityActionDef {
            name: "activate".into(),
            label: Some("Choose a date".into()),
        });
        assert_eq!(spoken_value(&spec).as_deref(), Some("Choose a date"));
        spec.state.has_popup = false;
        assert_eq!(spoken_value(&spec), None);
        spec.state.has_popup = true;
        spec.actions.clear();
        assert_eq!(spoken_value(&spec), None);
    }

    #[test]
    fn explicit_value_keeps_mixed_expanded_and_busy_states() {
        let accessibility = Accessibility {
            value: Some(AccessibilityValue {
                text: Some("2 items".into()),
                now: Some(50.0),
                ..Default::default()
            }),
            state: AccessibilityState {
                checked: Some(Checked::Mixed),
                expanded: Some(true),
                busy: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            spoken_value(&accessibility).as_deref(),
            Some("2 items, mixed, expanded, busy")
        );
    }

    #[test]
    fn ranges_follow_the_ios_percentage_convention() {
        let accessibility = Accessibility {
            value: Some(AccessibilityValue {
                min: Some(20.0),
                max: Some(40.0),
                now: Some(25.0),
                text: None,
            }),
            ..Default::default()
        };
        assert_eq!(spoken_value(&accessibility).as_deref(), Some("25%"));
    }

    #[test]
    fn empty_value_still_exposes_state_and_clearing_removes_it() {
        let accessibility = Accessibility {
            value: Some(AccessibilityValue {
                text: Some("".into()),
                ..Default::default()
            }),
            state: AccessibilityState {
                checked: Some(Checked::False),
                expanded: Some(false),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            spoken_value(&accessibility).as_deref(),
            Some("not checked, collapsed")
        );
        assert_eq!(spoken_value(&Accessibility::default()), None);
    }

    #[test]
    fn range_value_requires_complete_finite_bounds_and_clamps_current() {
        let indeterminate = Accessibility {
            value: Some(AccessibilityValue {
                min: Some(0.0),
                max: Some(100.0),
                now: None,
                text: None,
            }),
            ..Default::default()
        };
        assert_eq!(range_value(&indeterminate), None);

        let without_bounds = Accessibility {
            value: Some(AccessibilityValue {
                min: None,
                max: None,
                now: Some(50.0),
                text: None,
            }),
            ..Default::default()
        };
        assert_eq!(range_value(&without_bounds), None);

        let complete = Accessibility {
            value: Some(AccessibilityValue {
                min: Some(0.0),
                max: Some(100.0),
                now: Some(42.4),
                text: None,
            }),
            ..Default::default()
        };
        assert_eq!(range_value(&complete), Some((0, 100, 42)));

        let out_of_bounds = Accessibility {
            value: Some(AccessibilityValue {
                min: Some(10.0),
                max: Some(20.0),
                now: Some(25.0),
                text: None,
            }),
            ..Default::default()
        };
        assert_eq!(range_value(&out_of_bounds), Some((10, 20, 20)));
    }
}
