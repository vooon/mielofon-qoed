//! Console log format selection for the shared telemetry install.

use serde::Deserialize;

/// Console output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleFormat {
    /// Structured JSON lines (log-shipper friendly).
    Json,
    /// `logfmt` style: `level=info target=... message="..."`.
    #[default]
    Logfmt,
}

/// Error returned when parsing a console format name.
#[derive(Debug, thiserror::Error)]
#[error("unknown console log format {0:?}; expected \"json\" or \"logfmt\"")]
pub struct ParseFormatError(String);

impl ConsoleFormat {
    /// Parse a `[log] format` value (case-insensitive).
    pub fn parse(s: &str) -> Result<ConsoleFormat, ParseFormatError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" => Ok(ConsoleFormat::Json),
            "logfmt" => Ok(ConsoleFormat::Logfmt),
            other => Err(ParseFormatError(other.to_string())),
        }
    }
}

impl std::str::FromStr for ConsoleFormat {
    type Err = ParseFormatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ConsoleFormat::parse(s)
    }
}

/// Deserialize a console format from a TOML string attribute.
impl<'de> Deserialize<'de> for ConsoleFormat {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        ConsoleFormat::parse(&s).map_err(serde::de::Error::custom)
    }
}
