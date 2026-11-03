//! The native download manager (`kit-gui`): `model` holds what is shown and decided, `backend`
//! talks to the catalog, the installed state and the `kit` process. The window itself is
//! `app`, only built with the `gui` feature.

pub mod backend;
pub mod model;

#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
pub mod theme;
