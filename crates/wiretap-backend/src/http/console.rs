//! The gateway behind an ApplianceOS console, to the appliance chassis v0.8.0's
//! contract: the console proxies `/apps/<name>/…` to a Unix socket with the path
//! unchanged, and asserts its caller in
//! `Appliance-Identity: v1; id=…; user=…; role=…[; reauth=…]`.

use std::fmt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;

use axum::extract::connect_info::Connected;
use axum::http::HeaderMap;
use axum::serve::IncomingStream;
use chrono::{DateTime, Utc};
use tokio::net::UnixListener;
use wslib_appliance_types::console::{ConsoleIdentity, IDENTITY_HEADER};
use wslib_appliance_types::Role as ConsoleRole;
pub use wslib_auth::REAUTHENTICATION_WINDOW;

use crate::keys::Role;

/// The chassis's refusal of a stale proof, which a console matches on.
pub const STALE_PROOF: &str = "this needs the password confirmed";

/// Within the window and not ahead of `now`, as the chassis judges it.
pub fn proved_recently(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    at.is_some_and(|at| at <= now && now - at <= REAUTHENTICATION_WINDOW)
}

/// `None` when no header was sent; refused when one is malformed or sent twice.
pub fn asserted(headers: &HeaderMap) -> Result<Option<ConsoleIdentity>, String> {
    let malformed = |why: &str| format!("the Appliance-Identity header is malformed: {why}");
    let mut values = headers.get_all(IDENTITY_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(malformed("sent more than once"));
    }
    let value = value.to_str().map_err(|_| malformed("not visible ASCII"))?;
    value
        .parse::<ConsoleIdentity>()
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Configuring the app is the console's admin's; anyone else with a role in it
/// reads. Ingest is a device's, never a person's.
pub fn gateway_role(console: ConsoleRole) -> Role {
    match console {
        ConsoleRole::Admin | ConsoleRole::HostAdmin => Role::Admin,
        ConsoleRole::Viewer | ConsoleRole::Operator => Role::Read,
    }
}

/// The uid at the other end of an app-socket connection, read at accept.
#[derive(Debug, Clone, Copy)]
pub struct SocketPeer {
    pub uid: Option<u32>,
}

impl Connected<IncomingStream<'_, UnixListener>> for SocketPeer {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        Self {
            uid: stream.io().peer_cred().ok().map(|cred| cred.uid()),
        }
    }
}

impl fmt::Display for SocketPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.uid {
            Some(uid) => write!(f, "uid:{uid}"),
            None => f.write_str("uid:?"),
        }
    }
}

/// Over any socket already at `path`, such as an earlier run's. Group 0 may
/// connect: that is how a console that is root without capabilities reaches it.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn the_consoles_admins_administer_and_everyone_else_reads() {
        for (console, gateway) in [
            (ConsoleRole::Viewer, Role::Read),
            (ConsoleRole::Operator, Role::Read),
            (ConsoleRole::Admin, Role::Admin),
            (ConsoleRole::HostAdmin, Role::Admin),
        ] {
            assert_eq!(gateway_role(console), gateway, "{console}");
        }
    }

    #[test]
    fn a_proof_counts_within_the_window_and_not_before_or_after_it() {
        let now = Utc::now();
        let second = chrono::TimeDelta::seconds(1);
        assert!(proved_recently(Some(now), now));
        assert!(proved_recently(Some(now - REAUTHENTICATION_WINDOW), now));
        for at in [
            None,
            Some(now - REAUTHENTICATION_WINDOW - second),
            Some(now + second),
        ] {
            assert!(!proved_recently(at, now), "{at:?}");
        }
    }

    #[test]
    fn no_header_is_nobody_and_two_or_a_malformed_one_are_refused() {
        let mut headers = HeaderMap::new();
        assert_eq!(asserted(&headers), Ok(None));
        let identity = HeaderValue::from_static("v1; id=a; user=ops; role=admin");
        headers.append(IDENTITY_HEADER, identity.clone());
        assert!(matches!(asserted(&headers), Ok(Some(_))));
        headers.append(IDENTITY_HEADER, identity);
        assert!(asserted(&headers).is_err());

        let mut headers = HeaderMap::new();
        headers.append(
            IDENTITY_HEADER,
            HeaderValue::from_static("v1; id=a; user=a; role=ingest"),
        );
        assert!(asserted(&headers).is_err());
    }
}
