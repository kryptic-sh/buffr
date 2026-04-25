//! Config loading and parsing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: General,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct General {
    #[serde(default = "default_homepage")]
    pub homepage: String,
}

fn default_homepage() -> String {
    "about:blank".into()
}
