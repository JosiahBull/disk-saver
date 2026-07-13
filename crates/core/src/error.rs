//! Error taxonomy for the engine and adapters.
//!
//! Two error types live here:
//!
//! * [`AdapterError`] — returned by [`Adapter`](crate::Adapter) methods. It
//!   distinguishes *expected* conditions ([`AdapterError::Unavailable`], e.g. the
//!   docker daemon is down) from *genuine* failures ([`AdapterError::Failed`]).
//! * [`ConfigError`] — returned while loading, parsing, or validating the
//!   configuration file.
//!
//! [`parse_adapter_config`] is the small helper every adapter factory uses to
//! turn its opaque `[adapters.<name>]` table into a typed config struct.

/// An error surfaced by an [`Adapter`](crate::Adapter) phase.
///
/// The engine treats the two variants very differently: [`Unavailable`] is an
/// expected, retry-next-run condition (the adapter is simply skipped for this
/// run), whereas [`Failed`] is a genuine failure that is logged, reflected in
/// the run report, and contributes to the process exit code.
///
/// [`Unavailable`]: AdapterError::Unavailable
/// [`Failed`]: AdapterError::Failed
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// An external dependency is missing or unreachable (daemon down, directory
    /// unreadable, binary not on `PATH`). Skip this run and retry next time.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// A genuine failure. Logged at error level; the adapter is skipped, the run
    /// continues, and the process exits with code 2.
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

impl AdapterError {
    /// Build an [`AdapterError::Unavailable`] from any string-like message.
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::Unavailable(msg.into())
    }
}

/// An error while loading, parsing, or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file could not be read (other than "not found", which yields
    /// [`Config::defaults`](crate::Config::defaults)).
    // NOTE: `PathBuf` does not implement `Display`, so the frozen contract's
    // `{path}` is rendered via `path.display()` — same message, and the field
    // name/type are preserved exactly.
    #[error("reading config {}: {source}", path.display())]
    Read {
        /// The path we tried to read.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The config text was not valid TOML.
    #[error("parsing config: {0}")]
    Parse(String),
    /// The config parsed but is structurally or semantically invalid.
    #[error("invalid config: {0}")]
    Invalid(String),
    /// An adapter's own section failed to deserialize into its typed config.
    #[error("adapter '{adapter}': {message}")]
    Adapter {
        /// The offending adapter's name.
        adapter: String,
        /// A description of what went wrong.
        message: String,
    },
}

impl ConfigError {
    /// Build a [`ConfigError::Invalid`] from any string-like message.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
}

/// Deserialize an adapter's opaque table into its typed config `T`.
///
/// `raw == None` (the section was absent) yields `T::default()`. Any
/// deserialization error is mapped to [`ConfigError::Adapter`] so the user sees
/// which adapter's config is broken. Used by every adapter factory.
pub fn parse_adapter_config<T>(adapter: &str, raw: Option<toml::Value>) -> Result<T, ConfigError>
where
    T: serde::de::DeserializeOwned + Default,
{
    match raw {
        None => Ok(T::default()),
        Some(value) => {
            <T as serde::de::Deserialize>::deserialize(value).map_err(|e: toml::de::Error| {
                ConfigError::Adapter {
                    adapter: adapter.to_owned(),
                    message: e.to_string(),
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, PartialEq, Deserialize)]
    #[serde(default)]
    struct Demo {
        a: u32,
        b: String,
    }

    #[test]
    fn unavailable_constructor() {
        let e = AdapterError::unavailable("daemon down");
        assert_eq!(e.to_string(), "unavailable: daemon down");
    }

    #[test]
    fn failed_is_transparent() {
        let e: AdapterError = anyhow::anyhow!("boom").into();
        assert_eq!(e.to_string(), "boom");
        assert!(matches!(e, AdapterError::Failed(_)));
    }

    #[test]
    fn config_invalid_constructor() {
        let e = ConfigError::invalid("bad");
        assert_eq!(e.to_string(), "invalid config: bad");
    }

    #[test]
    fn config_read_display_uses_path() {
        let e = ConfigError::Read {
            path: "/etc/x.toml".into(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope"),
        };
        assert!(e.to_string().starts_with("reading config /etc/x.toml: "));
    }

    #[test]
    fn config_adapter_display() {
        let e = ConfigError::Adapter {
            adapter: "docker".into(),
            message: "unknown field `foo`".into(),
        };
        assert_eq!(e.to_string(), "adapter 'docker': unknown field `foo`");
    }

    #[test]
    fn parse_adapter_config_none_is_default() {
        let got: Demo = parse_adapter_config("demo", None).unwrap();
        assert_eq!(got, Demo::default());
    }

    #[test]
    fn parse_adapter_config_some_deserializes() {
        let mut table = toml::value::Table::new();
        table.insert("a".into(), toml::Value::Integer(7));
        table.insert("b".into(), toml::Value::String("hi".into()));
        let got: Demo = parse_adapter_config("demo", Some(toml::Value::Table(table))).unwrap();
        assert_eq!(
            got,
            Demo {
                a: 7,
                b: "hi".into()
            }
        );
    }

    #[test]
    fn parse_adapter_config_bad_maps_to_adapter_error() {
        let mut table = toml::value::Table::new();
        // `a` should be an integer; a string here is a type error.
        table.insert("a".into(), toml::Value::String("not a number".into()));
        let err =
            parse_adapter_config::<Demo>("demo", Some(toml::Value::Table(table))).unwrap_err();
        match err {
            ConfigError::Adapter { adapter, .. } => assert_eq!(adapter, "demo"),
            other => panic!("expected Adapter error, got {other:?}"),
        }
    }
}
