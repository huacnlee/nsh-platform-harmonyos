//! Maps CSS `fontFamily` lists to HarmonyOS font family names.
//!
//! Omitting `set_font_families` (an empty result) lets `OH_Drawing` and ArkUI
//! resolve the system default and active system theme font. Passing a generic
//! `"sans-serif"` alias would miss `/system/etc/fontconfig.json`'s `generic`
//! entries and override the system theme font.

/// Resolves a CSS `fontFamily` value into explicit HarmonyOS family names.
///
/// Returns an empty list when the system UI font wins first (such as
/// `"system-ui"`, `"sans-serif"`, or `"-apple-system"`), and stops at a
/// system-sans fallback so `OH_Drawing` and ArkUI fall back to the active
/// system/theme font naturally.
pub(crate) fn families(value: &str) -> Vec<&str> {
    use nsh_core::FamilyName;
    let mut out = Vec::new();
    for family in nsh_core::font_families(value) {
        match family {
            FamilyName::Named(name) => {
                if !name.contains('\0') {
                    out.push(name);
                }
            }
            FamilyName::Serif => out.push("serif"),
            FamilyName::Monospace => out.push("monospace"),
            // HarmonyOS has no distinct system rounded family; system sans,
            // ui-rounded, and unknown generic variants defer to the
            // FontCollection's default/theme font.
            _ => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::families;

    #[test]
    fn system_families_defer_to_default_theme_font() {
        for value in [
            "",
            "system-ui",
            "sans-serif",
            "ui-sans-serif",
            "-apple-system, BlinkMacSystemFont, sans-serif",
            "sans-serif, Pacifico",
            "ui-rounded",
        ] {
            assert!(
                families(value).is_empty(),
                "expected empty family list for {value:?}"
            );
        }
    }

    #[test]
    fn explicit_families_preserve_priority_until_system_fallback() {
        assert_eq!(families("Pacifico, sans-serif"), vec!["Pacifico"]);
        assert_eq!(
            families("Pacifico, 'Noto Sans SC', system-ui, serif"),
            vec!["Pacifico", "Noto Sans SC"]
        );
        assert_eq!(
            families("JetBrains Mono, monospace"),
            vec!["JetBrains Mono", "monospace"]
        );
        assert_eq!(families("serif"), vec!["serif"]);
        assert_eq!(families("HarmonyOS Sans"), vec!["HarmonyOS Sans"]);
        assert_eq!(families("bad\0name, serif"), vec!["serif"]);
    }
}
