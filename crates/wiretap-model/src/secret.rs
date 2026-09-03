//! A string that does not print itself.

use std::fmt;

/// A credential — an API key, a shared token.
///
/// `Debug` and `Display` render `set`/`unset`, never the value, so a stray
/// `{:?}` on a config struct or a `tracing` field cannot leak it. Reaching the
/// plaintext takes an explicit [`Secret::expose`], which greps cleanly.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The plaintext. Every call site is a place a secret can escape.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() { "unset" } else { "set" })
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neither_debug_nor_display_reveals_the_value() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "set");
        assert_eq!(format!("{s}"), "set");
        assert!(!format!("{s:?} {s}").contains("hunter2"));
        assert_eq!(s.expose(), "hunter2");
    }

    /// The redaction has to survive being nested in a derived Debug, which is
    /// the way it would actually leak.
    #[test]
    fn redaction_survives_a_derived_debug() {
        #[derive(Debug)]
        struct Holder {
            key: Secret,
        }
        let h = Holder {
            key: Secret::new("hunter2"),
        };
        let out = format!("{h:?}");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("set"), "{out}");
        assert_eq!(
            h.key.expose(),
            "hunter2",
            "still reachable, but only deliberately"
        );
    }

    #[test]
    fn an_empty_secret_reads_as_unset() {
        assert_eq!(format!("{:?}", Secret::default()), "unset");
        assert!(Secret::default().is_empty());
    }
}
