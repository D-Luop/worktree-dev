//! Where a worktree runs: Windows, or inside a WSL distro. Serialized as `windows` / `wsl:<distro>`.
//! (Minimal stub of the model `feat/platform-select` owns; see docs/wsl-platform-contract.md.)

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Platform {
    #[default]
    Windows,
    Wsl(String),
}

impl Platform {
    pub fn is_windows(&self) -> bool {
        matches!(self, Platform::Windows)
    }

    pub fn distro(&self) -> Option<&str> {
        match self {
            Platform::Wsl(d) => Some(d),
            Platform::Windows => None,
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Platform::Windows => f.write_str("windows"),
            Platform::Wsl(d) => write!(f, "wsl:{d}"),
        }
    }
}

impl FromStr for Platform {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "" | "windows" => Ok(Platform::Windows),
            t => match t.strip_prefix("wsl:").map(str::trim) {
                Some(d) if !d.is_empty() && !d.contains(['/', '\\', ' ']) => Ok(Platform::Wsl(d.to_string())),
                _ => Err(format!("unknown platform '{t}' (windows | wsl:<distro>)")),
            },
        }
    }
}

impl Serialize for Platform {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Platform {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints() {
        assert_eq!("windows".parse::<Platform>().unwrap(), Platform::Windows);
        assert_eq!("".parse::<Platform>().unwrap(), Platform::Windows);
        assert_eq!("wsl:Ubuntu-24.04".parse::<Platform>().unwrap(), Platform::Wsl("Ubuntu-24.04".into()));
        assert_eq!(Platform::Wsl("Ubuntu".into()).to_string(), "wsl:Ubuntu");
        for bad in ["wsl", "wsl:", "wsl:a/b", "linux"] {
            assert!(bad.parse::<Platform>().is_err(), "{bad}");
        }
    }

    #[test]
    fn serde_as_string() {
        let p = Platform::Wsl("Debian".into());
        assert_eq!(serde_json::to_string(&p).unwrap(), "\"wsl:Debian\"");
        assert_eq!(serde_json::from_str::<Platform>("\"windows\"").unwrap(), Platform::Windows);
    }
}
