pub mod config;
pub mod engine;
pub mod media;
pub mod model;
pub mod platform;
pub mod protocol;
pub mod tools;

pub fn application_version() -> &'static str {
    option_env!("INANNA_VERSION")
        .filter(|version| !version.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}
