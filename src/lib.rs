//! Emergency Offline Kit: downloads and installs a curated set of offline knowledge and
//! entertainment. See `ROADMAP.md` and `docs/decisions.md`.

pub mod ai;
pub mod catalog;
pub mod cli;
pub mod cli_net;
pub mod diskspace;
pub mod geocode;
pub mod hw;
pub mod plan;

// Placeholders for later phases; each gets its content in the phase named in ROADMAP.md.
pub mod download;
pub mod install;
pub mod netcfg;
pub mod services;
pub mod ui;
