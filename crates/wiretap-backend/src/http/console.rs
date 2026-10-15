//! The gateway behind an ApplianceOS console, to the appliance chassis v0.8.0's
//! contract: the console proxies `/apps/<name>/…` to a Unix socket with the path
//! unchanged, and asserts its caller in
//! `Appliance-Identity: v1; id=…; user=…; role=…[; reauth=…]`.

use std::fmt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use axum::extract::connect_info::Connected;
use axum::http::HeaderMap;
use axum::serve::IncomingStream;
use chrono::{DateTime, Utc};
use tokio::net::UnixListener;

use crate::keys::Role;

pub const IDENTITY_HEADER: &str = "appliance-identity";

const MAX_FIELD: usize = 256;

/// The chassis's own, mirrored so the gateway stays free of it.
pub const REAUTHENTICATION_WINDOW: Duration = Duration::from_secs(5 * 60);

/// The chassis's refusal of a stale proof, which a console matches on.
pub const STALE_PROOF: &str = "this needs the password confirmed";

/// The caller a console asserts, in the gateway's roles.
#[derive(Debug, PartialEq, Eq)]
pub struct Asserted {
    pub username: String,
    pub role: Role,
    pub reauthenticated_at: Option<DateTime<Utc>>,
}

/// Within the window and not ahead of `now`, as the chassis judges it.
pub fn proved_recently(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    at.is_some_and(|at| {
        (now - at)
            .to_std()
            .is_ok_and(|age| age <= REAUTHENTICATION_WINDOW)
    })
}

/// `None` when no header was sent; refused when one is malformed or sent twice.
pub fn asserted(headers: &HeaderMap) -> Result<Option<Asserted>, &'static str> {
    let mut values = headers.get_all(IDENTITY_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("sent more than once");
    }
    value
        .to_str()
        .map_err(|_| "not visible ASCII")?
        .parse()
        .map(Some)
}

impl FromStr for Asserted {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split(';').map(|part| part.trim_matches([' ', '\t']));
        if parts.next() != Some("v1") {
            return Err("not version v1");
        }
        let (mut id, mut user, mut role, mut reauth) = (None, None, None, None);
        for part in parts {
            let (key, raw) = part.split_once('=').ok_or("a part has no value")?;
            let slot = match key {
                "id" => &mut id,
                "user" => &mut user,
                "role" => &mut role,
                "reauth" => &mut reauth,
                _ => return Err("an unknown key"),
            };
            if slot.replace(raw).is_some() {
                return Err("a repeated key");
            }
        }
        let reauthenticated_at = reauth
            .map(|raw| {
                raw.bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| raw.parse().ok())
                    .flatten()
                    .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
                    .ok_or("reauth is not a time in unix seconds")
            })
            .transpose()?;
        decoded(id.ok_or("no id")?)?;
        Ok(Self {
            username: decoded(user.ok_or("no user")?)?,
            role: gateway_role(role.ok_or("no role")?).ok_or("an unknown role")?,
            reauthenticated_at,
        })
    }
}

/// Configuring the app is the console's admin's; anyone else with a role in it
/// reads. Ingest is a device's, never a person's.
fn gateway_role(console: &str) -> Option<Role> {
    match console {
        "admin" | "hostadmin" => Some(Role::Admin),
        "viewer" | "operator" => Some(Role::Read),
        _ => None,
    }
}

fn decoded(raw: &str) -> Result<String, &'static str> {
    const BAD: &str = "a field is not percent-encoded UTF-8";
    let mut bytes = Vec::with_capacity(raw.len());
    let mut rest = raw.as_bytes();
    while let Some((&byte, after)) = rest.split_first() {
        rest = after;
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            bytes.push(byte);
            continue;
        }
        let hex = rest
            .get(..2)
            .filter(|hex| byte == b'%' && hex.iter().all(u8::is_ascii_hexdigit))
            .ok_or(BAD)?;
        let hex = std::str::from_utf8(hex).map_err(|_| BAD)?;
        bytes.push(u8::from_str_radix(hex, 16).map_err(|_| BAD)?);
        rest = &rest[2..];
    }
    let field = String::from_utf8(bytes).map_err(|_| BAD)?;
    if field.is_empty() || field.len() > MAX_FIELD {
        return Err("a field is empty or too long");
    }
    if field.chars().any(char::is_control) || field.trim() != field {
        return Err("a field holds a control character or edge whitespace");
    }
    Ok(field)
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

    fn parse(value: &str) -> Result<Asserted, &'static str> {
        value.parse()
    }

    #[test]
    fn an_identity_reads_with_its_user_decoded_and_its_role_mapped() {
        assert_eq!(
            parse("v1; id=4f1c-9a; user=Zo%C3%AB%20O%27Brien; role=operator; reauth=1760000000"),
            Ok(Asserted {
                username: "Zoë O'Brien".into(),
                role: Role::Read,
                reauthenticated_at: DateTime::from_timestamp(1_760_000_000, 0),
            })
        );
        assert_eq!(
            parse("v1;role=admin ;\tuser=a;id=b").map(|a| a.username),
            Ok("a".into())
        );
    }

    #[test]
    fn the_consoles_admins_administer_and_everyone_else_reads() {
        for (console, gateway) in [
            ("viewer", Role::Read),
            ("operator", Role::Read),
            ("admin", Role::Admin),
            ("hostadmin", Role::Admin),
        ] {
            let read = parse(&format!("v1; id=a; user=a; role={console}")).unwrap();
            assert_eq!(read.role, gateway, "{console}");
        }
    }

    #[test]
    fn anything_else_is_malformed() {
        for value in [
            "",
            "v1",
            "v2; id=a; user=a; role=admin",
            "id=a; user=a; role=admin",
            "v1; id=a; user=a",
            "v1; id=a; role=admin",
            "v1; user=a; role=admin",
            "v1; id=a; user=a; role=root",
            "v1; id=a; user=a; role=Admin",
            "v1; id=a; user=a; role=ingest",
            "v1; id=a; id=b; user=a; role=admin",
            "v1; id=a; user=a; role=admin; extra=1",
            "v1; id=a; user=a; role=admin; reauth",
            "v1; id=a; user=a; role=admin; reauth=",
            "v1; id=a; user=a; role=admin; reauth=-5",
            "v1; id=a; user=a; role=admin; reauth=1.5",
            "v1; id=a; user=a; role=admin; reauth=99999999999999999999",
            "v1; id=a; user=a; role=admin; reauth=9223372036854775807",
            "v1; id=; user=a; role=admin",
            "v1; id=a b; user=a; role=admin",
            "v1; id=%4; user=a; role=admin",
            "v1; id=%+1; user=a; role=admin",
            "v1; id=%zz; user=a; role=admin",
            "v1; id=%FF; user=a; role=admin",
            "v1; id=%0A; user=a; role=admin",
            "v1; id=é; user=a; role=admin",
            "v1; id=a; user=%20admin; role=admin",
            &format!("v1; id={}; user=a; role=admin", "a".repeat(MAX_FIELD + 1)),
        ] {
            assert!(parse(value).is_err(), "{value:?} was accepted");
        }
    }

    #[test]
    fn a_proof_counts_within_the_window_and_not_before_or_after_it() {
        let now = Utc::now();
        let window = chrono::TimeDelta::from_std(REAUTHENTICATION_WINDOW).unwrap();
        let second = chrono::TimeDelta::seconds(1);
        assert!(proved_recently(Some(now), now));
        assert!(proved_recently(Some(now - window), now));
        for at in [None, Some(now - window - second), Some(now + second)] {
            assert!(!proved_recently(at, now), "{at:?}");
        }
    }

    #[test]
    fn no_header_is_nobody_and_two_are_refused() {
        let mut headers = HeaderMap::new();
        assert_eq!(asserted(&headers), Ok(None));
        let identity = axum::http::HeaderValue::from_static("v1; id=a; user=ops; role=admin");
        headers.append(IDENTITY_HEADER, identity.clone());
        assert!(matches!(asserted(&headers), Ok(Some(_))));
        headers.append(IDENTITY_HEADER, identity);
        assert!(asserted(&headers).is_err());
    }
}
