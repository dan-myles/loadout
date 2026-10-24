use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported operating system {0:?}; loadout runs on macos, linux, and windows")]
    UnsupportedOs(&'static str),
    #[error("unsupported CPU architecture {0:?}; loadout runs on x86_64 and aarch64")]
    UnsupportedArch(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

impl Os {
    pub const ALL: [Os; 3] = [Os::Macos, Os::Linux, Os::Windows];

    pub fn current() -> Result<Self, Error> {
        match std::env::consts::OS {
            "macos" => Ok(Os::Macos),
            "linux" => Ok(Os::Linux),
            "windows" => Ok(Os::Windows),
            other => Err(Error::UnsupportedOs(other)),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Os::Macos => "macos",
            Os::Linux => "linux",
            Os::Windows => "windows",
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn current() -> Result<Self, Error> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(Arch::X86_64),
            "aarch64" => Ok(Arch::Aarch64),
            other => Err(Error::UnsupportedArch(other)),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
