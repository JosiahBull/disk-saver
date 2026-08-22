//! [`Configured`] — a config value together with *where it came from*.
//!
//! Deserialization erases the difference between a value the user wrote and one
//! that merely happens to equal the default: after `serde` is done, `min_age =
//! "1h"` and a defaulted 1h are the same bytes. That difference is exactly what
//! decides who gives way when two settings contradict each other, so adapters
//! that need it end up reaching around serde to inspect the raw TOML table
//! before it is consumed — a rule about statement order that nothing enforces.
//!
//! [`Configured`] keeps the provenance in the value instead, so the question is
//! answered by the type system and the raw table is never needed.

use std::fmt;
use std::time::Duration;

use crate::adapter::ConfigCx;
use crate::error::ConfigError;

/// A config value and where it came from.
///
/// Which of the three states a value is in decides who gives way when two
/// settings conflict. An adapter's own default yields — a config saying
/// `max_age = "0s"` means "collect this the moment it is idle", and a default
/// floor of ours has no business turning that into a hard error. A value the
/// *user* wrote does not yield: a self-contradictory pair is a real mistake,
/// and is reported.
///
/// See [`Configured::capped_at`], which is that rule written down once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Configured<T> {
    /// The config named this value.
    Set(T),
    /// The config was silent; this is a default chosen on its behalf.
    Default(T),
    /// The config was silent and no default has been applied.
    Unset,
}

impl<T> Default for Configured<T> {
    /// [`Unset`](Configured::Unset) — which is what a container-level
    /// `#[serde(default)]` uses for a key the config never mentioned, unless the
    /// struct's own `Default` impl names one (see the module docs).
    fn default() -> Self {
        Configured::Unset
    }
}

impl<T> From<Option<T>> for Configured<T> {
    /// Bridge from a plain optional field: `Some` is a value the config named,
    /// `None` is silence. Used where the field lives in a struct that cannot
    /// depend on this crate — [`disk_saver_scan::FsConfig`] most of all.
    ///
    /// [`disk_saver_scan::FsConfig`]: https://docs.rs/disk-saver-scan
    fn from(opt: Option<T>) -> Self {
        match opt {
            Some(value) => Configured::Set(value),
            None => Configured::Unset,
        }
    }
}

impl<T> Configured<T> {
    /// The value, if there is one.
    pub fn get(&self) -> Option<&T> {
        match self {
            Configured::Set(v) | Configured::Default(v) => Some(v),
            Configured::Unset => None,
        }
    }

    /// The value, consuming `self`.
    pub fn into_value(self) -> Option<T> {
        match self {
            Configured::Set(v) | Configured::Default(v) => Some(v),
            Configured::Unset => None,
        }
    }

    /// The value, or `fallback` when [`Unset`](Configured::Unset).
    pub fn unwrap_or(self, fallback: T) -> T {
        self.into_value().unwrap_or(fallback)
    }

    /// Whether the *config* named this value — as opposed to a default, or
    /// nothing at all.
    pub fn is_set(&self) -> bool {
        matches!(self, Configured::Set(_))
    }

    /// Apply `f` to the value, preserving its provenance.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Configured<U> {
        match self {
            Configured::Set(v) => Configured::Set(f(v)),
            Configured::Default(v) => Configured::Default(f(v)),
            Configured::Unset => Configured::Unset,
        }
    }

    /// Supply a default where there is none. A value that is already
    /// [`Set`](Configured::Set) or [`Default`](Configured::Default) is untouched.
    pub fn or_default(self, default: T) -> Self {
        match self {
            Configured::Unset => Configured::Default(default),
            other => other,
        }
    }

    /// Substitute *this adapter's* default for whatever default is already
    /// there, leaving a value the user [`Set`](Configured::Set) alone.
    ///
    /// This is how an adapter opts out of a default belonging to a shared config
    /// struct it flattens. `git-gc` is the case in point: it embeds the same
    /// `FsConfig` the build-artifact adapters use, whose 1h scavenge floor suits
    /// deleting something rebuildable. `git gc` instead *repacks a live repo* —
    /// doing that to something touched an hour ago is work the next commit
    /// undoes, and it competes with the developer for IO — so it wants a longer
    /// floor of its own, without overriding a floor the user asked for.
    pub fn with_default(self, default: T) -> Self {
        match self {
            Configured::Set(v) => Configured::Set(v),
            _ => Configured::Default(default),
        }
    }
}

impl<T: Ord + Copy + fmt::Debug> Configured<T> {
    /// Cap the value at `ceiling`, honouring its provenance.
    ///
    /// A [`Default`](Configured::Default) gives way and is lowered to `ceiling`;
    /// a value the config [`Set`](Configured::Set) does not, and one above
    /// `ceiling` is reported as the user contradicting themselves;
    /// [`Unset`](Configured::Unset) passes through. `key` and `ceiling_key` name
    /// the two config keys in that error.
    pub fn capped_at(
        self,
        ceiling: T,
        cx: &ConfigCx,
        key: &str,
        ceiling_key: &str,
    ) -> Result<Self, ConfigError> {
        match self {
            Configured::Default(v) => Ok(Configured::Default(v.min(ceiling))),
            Configured::Set(v) if v > ceiling => Err(cx.err(format!(
                "{key} ({v:?}) must not exceed {ceiling_key} ({ceiling:?})"
            ))),
            other => Ok(other),
        }
    }
}

impl<'de, T> serde::Deserialize<'de> for Configured<T>
where
    T: serde::Deserialize<'de>,
{
    /// A key that is present is a value the config [`Set`](Configured::Set).
    /// Absence never reaches here — the container's `#[serde(default)]` handles
    /// it, which is why an adapter can seed its own default by writing
    /// `Configured::Default(…)` in its `Default` impl.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Configured::Set)
    }
}

/// `#[serde(with = "disk_saver_core::configured::humantime")]` for a
/// `Configured<Duration>` field written in humantime syntax (`"15m"`, `"7d"`).
///
/// The plain [`Deserialize`](serde::Deserialize) impl above would want a raw
/// number of seconds; this routes through `humantime_serde` the way every other
/// duration in the config does.
pub mod humantime {
    use super::{Configured, Duration};

    /// Deserialize `"15m"` into [`Configured::Set`].
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Configured<Duration>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        humantime_serde::deserialize(deserializer).map(Configured::Set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn cx() -> ConfigCx {
        ConfigCx::new("demo")
    }

    #[test]
    fn accessors_see_through_both_valued_states() {
        assert_eq!(Configured::Set(3).get(), Some(&3));
        assert_eq!(Configured::Default(3).get(), Some(&3));
        assert_eq!(Configured::<u8>::Unset.get(), None);

        assert!(Configured::Set(3).is_set());
        assert!(!Configured::Default(3).is_set());
        assert!(!Configured::<u8>::Unset.is_set());

        assert_eq!(Configured::Default(3).unwrap_or(9), 3);
        assert_eq!(Configured::Unset.unwrap_or(9), 9);
    }

    #[test]
    fn map_preserves_provenance() {
        assert_eq!(Configured::Set(2).map(|v| v * 2), Configured::Set(4));
        assert_eq!(
            Configured::Default(2).map(|v| v * 2),
            Configured::Default(4)
        );
        assert_eq!(Configured::<u8>::Unset.map(|v| v * 2), Configured::Unset);
    }

    #[test]
    fn or_default_fills_only_unset() {
        assert_eq!(Configured::Unset.or_default(7), Configured::Default(7));
        assert_eq!(Configured::Default(3).or_default(7), Configured::Default(3));
        assert_eq!(Configured::Set(3).or_default(7), Configured::Set(3));
    }

    #[test]
    fn with_default_replaces_a_default_but_never_a_set_value() {
        assert_eq!(Configured::Unset.with_default(7), Configured::Default(7));
        // The distinction from `or_default`: an existing default gives way.
        assert_eq!(
            Configured::Default(3).with_default(7),
            Configured::Default(7)
        );
        assert_eq!(Configured::Set(3).with_default(7), Configured::Set(3));
    }

    #[test]
    fn capped_at_lowers_our_default_but_not_their_value() {
        let cx = cx();
        let cap = |v: Configured<u8>| v.capped_at(4, &cx, "floor", "ceiling").unwrap();
        assert_eq!(cap(Configured::Default(10)), Configured::Default(4));
        assert_eq!(cap(Configured::Default(2)), Configured::Default(2));
        assert_eq!(cap(Configured::Set(3)), Configured::Set(3));
        // Equal is fine: the invariant is `<=`.
        assert_eq!(cap(Configured::Set(4)), Configured::Set(4));
        assert_eq!(cap(Configured::Unset), Configured::Unset);
    }

    #[test]
    fn capped_at_reports_a_value_the_user_set_too_high() {
        let err = Configured::Set(9)
            .capped_at(4, &cx(), "floor", "ceiling")
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "adapter 'demo': floor (9) must not exceed ceiling (4)"
        );
    }

    #[derive(Debug, Deserialize)]
    #[serde(default)]
    struct Demo {
        #[serde(with = "super::humantime")]
        span: Configured<Duration>,
        plain: Configured<u32>,
    }

    impl Default for Demo {
        fn default() -> Self {
            // The adapter seeds its own default here; absence lands on it.
            Demo {
                span: Configured::Default(Duration::from_secs(900)),
                plain: Configured::Unset,
            }
        }
    }

    #[test]
    fn a_present_key_deserializes_as_set() {
        let got: Demo = toml::from_str("span = \"15m\"\nplain = 4\n").unwrap();
        assert_eq!(got.span, Configured::Set(Duration::from_secs(900)));
        assert_eq!(got.plain, Configured::Set(4));
    }

    #[test]
    fn an_absent_key_takes_the_containers_default() {
        let got: Demo = toml::from_str("").unwrap();
        // Same 15 minutes as the `Set` case above, and distinguishable from it.
        assert_eq!(got.span, Configured::Default(Duration::from_secs(900)));
        assert!(!got.span.is_set());
        assert_eq!(got.plain, Configured::Unset);
    }
}
