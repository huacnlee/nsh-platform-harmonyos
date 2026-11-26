//! Spoken and ranged accessibility values for ArkUI nodes and virtual providers.

use nsh_core::{Accessibility, AccessibilityCurrent, AccessibilityRole, Checked};

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
        AccessibilityRole::Textbox => "textbox",
        AccessibilityRole::TabPanel => "tabpanel",
        AccessibilityRole::Menuitem
        | AccessibilityRole::MenuItemCheckbox
        | AccessibilityRole::MenuItemRadio => "menuitem",
        _ => "text",
    }
}

struct StateWords {
    checked: &'static str,
    not_checked: &'static str,
    mixed: &'static str,
    pressed: &'static str,
    not_pressed: &'static str,
    pressed_mixed: &'static str,
    expanded: &'static str,
    collapsed: &'static str,
    busy: &'static str,
    invalid: &'static str,
    current_true: &'static str,
    current_page: &'static str,
    current_step: &'static str,
    current_location: &'static str,
    current_date: &'static str,
    current_time: &'static str,
}

fn state_words(locale: &str) -> StateWords {
    match locale
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "zh" => StateWords {
            checked: "已选中",
            not_checked: "未选中",
            mixed: "部分选中",
            pressed: "已按下",
            not_pressed: "未按下",
            pressed_mixed: "部分按下",
            expanded: "已展开",
            collapsed: "已折叠",
            busy: "忙碌",
            invalid: "无效",
            current_true: "当前项",
            current_page: "当前页面",
            current_step: "当前步骤",
            current_location: "当前位置",
            current_date: "当前日期",
            current_time: "当前时间",
        },
        "ja" => StateWords {
            checked: "選択済み",
            not_checked: "未選択",
            mixed: "一部選択",
            pressed: "押下中",
            not_pressed: "未押下",
            pressed_mixed: "一部押下",
            expanded: "展開中",
            collapsed: "折りたたみ中",
            busy: "処理中",
            invalid: "無効",
            current_true: "現在",
            current_page: "現在のページ",
            current_step: "現在のステップ",
            current_location: "現在の場所",
            current_date: "現在の日付",
            current_time: "現在の時刻",
        },
        "ko" => StateWords {
            checked: "선택됨",
            not_checked: "선택 안 됨",
            mixed: "일부 선택됨",
            pressed: "눌림",
            not_pressed: "눌리지 않음",
            pressed_mixed: "일부 눌림",
            expanded: "펼쳐짐",
            collapsed: "접힘",
            busy: "사용 중",
            invalid: "유효하지 않음",
            current_true: "현재",
            current_page: "현재 페이지",
            current_step: "현재 단계",
            current_location: "현재 위치",
            current_date: "현재 날짜",
            current_time: "현재 시간",
        },
        "es" => StateWords {
            checked: "seleccionado",
            not_checked: "no seleccionado",
            mixed: "mixto",
            pressed: "pulsado",
            not_pressed: "no pulsado",
            pressed_mixed: "mixto",
            expanded: "expandido",
            collapsed: "contraído",
            busy: "ocupado",
            invalid: "no válido",
            current_true: "actual",
            current_page: "página actual",
            current_step: "paso actual",
            current_location: "ubicación actual",
            current_date: "fecha actual",
            current_time: "hora actual",
        },
        "fr" => StateWords {
            checked: "coché",
            not_checked: "non coché",
            mixed: "mixte",
            pressed: "enfoncé",
            not_pressed: "non enfoncé",
            pressed_mixed: "mixte",
            expanded: "développé",
            collapsed: "réduit",
            busy: "occupé",
            invalid: "non valide",
            current_true: "actuel",
            current_page: "page actuelle",
            current_step: "étape actuelle",
            current_location: "emplacement actuel",
            current_date: "date actuelle",
            current_time: "heure actuelle",
        },
        "de" => StateWords {
            checked: "aktiviert",
            not_checked: "nicht aktiviert",
            mixed: "gemischt",
            pressed: "gedrückt",
            not_pressed: "nicht gedrückt",
            pressed_mixed: "gemischt",
            expanded: "erweitert",
            collapsed: "reduziert",
            busy: "beschäftigt",
            invalid: "ungültig",
            current_true: "aktuell",
            current_page: "aktuelle Seite",
            current_step: "aktueller Schritt",
            current_location: "aktueller Ort",
            current_date: "aktuelles Datum",
            current_time: "aktuelle Uhrzeit",
        },
        _ => StateWords {
            checked: "checked",
            not_checked: "not checked",
            mixed: "mixed",
            pressed: "pressed",
            not_pressed: "not pressed",
            pressed_mixed: "mixed",
            expanded: "expanded",
            collapsed: "collapsed",
            busy: "busy",
            invalid: "invalid",
            current_true: "current",
            current_page: "current page",
            current_step: "current step",
            current_location: "current location",
            current_date: "current date",
            current_time: "current time",
        },
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
    let words = state_words(locale);
    let mut parts: Vec<String> = Vec::new();
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
        Some(Checked::True) => parts.push(words.checked.into()),
        Some(Checked::False) => parts.push(words.not_checked.into()),
        Some(Checked::Mixed) => parts.push(words.mixed.into()),
        None => {}
    }
    match accessibility.state.pressed {
        Some(Checked::True) => parts.push(words.pressed.into()),
        Some(Checked::False) => parts.push(words.not_pressed.into()),
        Some(Checked::Mixed) => parts.push(words.pressed_mixed.into()),
        None => {}
    }
    match accessibility.state.expanded {
        Some(true) => parts.push(words.expanded.into()),
        Some(false) => parts.push(words.collapsed.into()),
        None => {}
    }
    if let Some(current) = accessibility.state.current {
        let word = match current {
            AccessibilityCurrent::True => words.current_true,
            AccessibilityCurrent::Page => words.current_page,
            AccessibilityCurrent::Step => words.current_step,
            AccessibilityCurrent::Location => words.current_location,
            AccessibilityCurrent::Date => words.current_date,
            AccessibilityCurrent::Time => words.current_time,
        };
        parts.push(word.into());
    }
    if accessibility.state.invalid {
        parts.push(words.invalid.into());
    }
    if accessibility.state.busy {
        parts.push(words.busy.into());
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
        assert_eq!(role_str(AccessibilityRole::Textbox), "textbox");
        assert_eq!(role_str(AccessibilityRole::TabPanel), "tabpanel");
        assert_eq!(role_str(AccessibilityRole::Menuitem), "menuitem");
        assert_eq!(role_str(AccessibilityRole::MenuItemCheckbox), "menuitem");
        assert_eq!(role_str(AccessibilityRole::MenuItemRadio), "menuitem");
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

    #[test]
    fn pressed_current_and_invalid_states_localize_and_clear() {
        let mut spec = Accessibility::default();
        spec.state.pressed = Some(Checked::True);
        assert_eq!(spoken_value(&spec).as_deref(), Some("pressed"));
        assert_eq!(
            spoken_value_for_locale(&spec, "zh-CN").as_deref(),
            Some("已按下")
        );

        spec.state.pressed = Some(Checked::False);
        assert_eq!(spoken_value(&spec).as_deref(), Some("not pressed"));
        assert_eq!(
            spoken_value_for_locale(&spec, "zh-CN").as_deref(),
            Some("未按下")
        );

        spec.state.pressed = Some(Checked::Mixed);
        assert_eq!(spoken_value(&spec).as_deref(), Some("mixed"));
        assert_eq!(
            spoken_value_for_locale(&spec, "zh-CN").as_deref(),
            Some("部分按下")
        );

        spec.state.pressed = None;
        for (variant, en, zh) in [
            (AccessibilityCurrent::True, "current", "当前项"),
            (AccessibilityCurrent::Page, "current page", "当前页面"),
            (AccessibilityCurrent::Step, "current step", "当前步骤"),
            (
                AccessibilityCurrent::Location,
                "current location",
                "当前位置",
            ),
            (AccessibilityCurrent::Date, "current date", "当前日期"),
            (AccessibilityCurrent::Time, "current time", "当前时间"),
        ] {
            spec.state.current = Some(variant);
            assert_eq!(spoken_value(&spec).as_deref(), Some(en));
            assert_eq!(spoken_value_for_locale(&spec, "zh").as_deref(), Some(zh));
        }

        spec.state.current = Some(AccessibilityCurrent::Step);
        spec.state.invalid = true;
        assert_eq!(
            spoken_value(&spec).as_deref(),
            Some("current step, invalid")
        );
        assert_eq!(
            spoken_value_for_locale(&spec, "zh-Hans").as_deref(),
            Some("当前步骤, 无效")
        );

        spec.state.current = None;
        spec.state.invalid = false;
        assert_eq!(spoken_value(&spec), None);
    }
}
