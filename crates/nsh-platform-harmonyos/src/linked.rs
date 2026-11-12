//! The app linked into the native library: the runner (`cdylib`) the
//! command line writes for each app defines it with [`crate::app!`], and the
//! surface starts it with `NshSurface.runApp()`.

use nsh_core::element::{Registry, Root};

/// What a runner links: the UI sets the app serves and, for an app written
/// in Rust, its root (NSH's generated `nsh_rust_app`).
pub struct LinkedApp {
    /// The UI sets: `nsh/base`'s, then the app's crates'.
    pub ui_sets: fn() -> Vec<Registry>,
    /// The root component of an app written in Rust; `None` for a TSX app,
    /// whose bundle the host loads (`loadResource`, `runPackage`).
    pub root: Option<fn() -> Root>,
}

unsafe extern "Rust" {
    /// Defined by the runner, through [`crate::app!`].
    safe fn __nsh_platform_harmonyos_app() -> LinkedApp;
}

/// The UI sets of the linked app.
pub(crate) fn ui_sets() -> Vec<Registry> {
    (__nsh_platform_harmonyos_app().ui_sets)()
}

/// Whether the linked app is written in Rust.
pub(crate) fn has_root() -> bool {
    __nsh_platform_harmonyos_app().root.is_some()
}

/// The root of the linked app written in Rust, if it is one.
pub(crate) fn root() -> Option<Root> {
    __nsh_platform_harmonyos_app().root.map(|root| root())
}

/// Defines the app a runner links (once, in its `cdylib`):
///
/// ```ignore
/// // An app written in Rust: NSH's generated `nsh_rust_app`.
/// nsh_platform_harmonyos::app!(nsh_rust_app::ui_sets, nsh_rust_app::root);
/// // A TSX app, whose bundle the host loads: nsh/base's UI set.
/// nsh_platform_harmonyos::app!();
/// // A TSX app with Rust crates of its own (`nsh.json` `"rust"` without
/// // `"root"`): nsh/base's UI set, then each crate's (`#[native_module]`'s
/// // `registry()`), for the bundle the host loads to import.
/// nsh_platform_harmonyos::app!(sets = [app_0::registry, app_1::registry]);
/// ```
#[macro_export]
macro_rules! app {
    (@define $ui_sets:expr, $root:expr) => {
        #[unsafe(no_mangle)]
        #[doc(hidden)]
        pub fn __nsh_platform_harmonyos_app() -> $crate::LinkedApp {
            // Keeps the backend's N-API registrations in the library.
            $crate::__private::link();
            $crate::LinkedApp {
                ui_sets: $ui_sets,
                root: $root,
            }
        }
    };
    () => {
        $crate::app!(@define || ::std::vec![$crate::__private::nsh_base_registry()], None);
    };
    (sets = [$($registry:path),* $(,)?]) => {
        $crate::app!(@define || ::std::vec![$crate::__private::nsh_base_registry(), $($registry()),*], None);
    };
    ($ui_sets:path, $root:path) => {
        $crate::app!(@define $ui_sets, Some($root));
    };
}
