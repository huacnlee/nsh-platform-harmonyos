//! Compiled enhancements intersected with a supplied system API level.

/// Native Shell's minimum supported OHOS API level.
pub(crate) const MIN_API: u32 = if cfg!(feature = "api-26") {
    26
} else if cfg!(feature = "api-23") {
    23
} else {
    20
};

/// Capabilities available to the embedding host on this runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Capabilities {
    /// Actual system API level, independent of surface environment overrides.
    pub api_level: u32,
    /// Native Shell's minimum supported system API level.
    pub minimum_api_level: u32,
    /// API-23 support was included in this library.
    pub compiled_api23: bool,
    /// API-26 support was included in this library.
    pub compiled_api26: bool,
    /// API-23 enhancements may be used on this runtime.
    pub api23: bool,
    /// API-26 enhancements may be used on this runtime.
    pub api26: bool,
}

impl Capabilities {
    pub(crate) fn for_api(api_level: u32) -> Self {
        Self::with_compiled(
            api_level,
            cfg!(feature = "api-23"),
            cfg!(feature = "api-26"),
        )
    }

    fn with_compiled(api_level: u32, api23: bool, api26: bool) -> Self {
        Self {
            api_level,
            minimum_api_level: MIN_API,
            compiled_api23: api23,
            compiled_api26: api26,
            api23: api23 && api_level >= 23,
            api26: api26 && api_level >= 26,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enhancements_require_both_build_and_runtime_support() {
        assert_eq!(Capabilities::for_api(20).minimum_api_level, MIN_API);
        for api in [20, 22, 23, 25, 26, 27] {
            for (compiled23, compiled26) in [(false, false), (true, false), (true, true)] {
                let c = Capabilities::with_compiled(api, compiled23, compiled26);
                assert_eq!(c.api23, compiled23 && api >= 23);
                assert_eq!(c.api26, compiled26 && api >= 26);
                assert_eq!(c.minimum_api_level, MIN_API);
            }
        }
    }
}
