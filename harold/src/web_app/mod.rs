pub(crate) const DATASTAR_JS: &[u8] = include_bytes!("../../static/web_app/datastar-1.0.2.js");
pub(crate) const DATASTAR_SHA256: &str =
    "2837d87acf6ee0ba8e4e63765926c25a98d63883b02f88be194a86b81d3fd24a";

#[cfg(test)]
#[path = "assets_tests.rs"]
mod assets_tests;
