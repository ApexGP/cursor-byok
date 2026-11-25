//! Integrates local Cursor account state.
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;
use sqlx::{Connection, Row, SqliteConnection};

use crate::{Error, Result};

const EMAIL: &str = "cursor@ai.com";
const SIGN_UP_TYPE: &str = "Google";
pub(crate) const SUBJECT: &str = "cursor-local-user";
const MEMBERSHIP_TYPE: &str = "ultra";
const SUBSCRIPTION_STATUS: &str = "active";

pub async fn inject_if_missing() -> Result<()> {
    inject_if_missing_at(&state_db_path()?).await
}

pub(crate) fn state_db_path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    match std::env::consts::OS {
        "macos" => {
            Ok(home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"))
        }
        "windows" => Ok(std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Cursor/User/globalStorage/state.vscdb")),
        "linux" => Ok(std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("Cursor/User/globalStorage/state.vscdb")),
        platform => Err(Error::Config(format!(
            "Cursor account injection is unsupported on {platform}"
        ))),
    }
}

async fn inject_if_missing_at(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
    )
    .execute(&mut connection)
    .await?;

    let token = local_token()?;
    let account = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind("cursorAuth/accessToken")
        .fetch_optional(&mut connection)
        .await?;
    if account.is_some_and(|row| {
        row.try_get::<String, _>("value")
            .is_ok_and(|value| !value.trim().is_empty() && value != token)
    }) {
        return Ok(());
    }

    let values = [
        ("cursorAuth/accessToken", token.as_str()),
        ("cursorAuth/refreshToken", token.as_str()),
        ("cursorAuth/cachedEmail", EMAIL),
        ("cursorAuth/cachedSignUpType", SIGN_UP_TYPE),
        ("cursorAuth/stripeMembershipAuthId", SUBJECT),
        ("cursorAuth/stripeMembershipType", MEMBERSHIP_TYPE),
        ("cursorAuth/stripeSubscriptionStatus", SUBSCRIPTION_STATUS),
    ];
    let mut transaction = connection.begin().await?;
    for (key, value) in values {
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(key)
            .bind(value)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    tracing::info!(
        email = EMAIL,
        subject = SUBJECT,
        "injected local Cursor account"
    );
    Ok(())
}

pub(crate) fn is_local_cursor_authorization(authorization: &str) -> bool {
    authorization
        .strip_prefix("Bearer ")
        .is_some_and(is_local_cursor_token)
}

pub(crate) fn authorization_subject(authorization: &str) -> Option<String> {
    let (scheme, token) = authorization.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    jwt_subject(token.trim())
}

/// Extracts the JWT `sub` claim without verifying the signature, matching how
/// Cursor's `getAuthIdFromToken` derives the Statsig `userID`.
pub(crate) fn jwt_subject(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?.trim_end_matches('=');
    let _signature = parts.next()?;
    if parts.next().is_some() || payload.is_empty() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map(str::to_owned)
}

fn is_local_cursor_token(token: &str) -> bool {
    local_token().is_ok_and(|local| local == token)
}

pub(super) fn local_token() -> Result<String> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
        "sub": SUBJECT,
        "email": EMAIL,
        "type": "session",
        "iss": "cursor-client",
        "scope": "openid profile email",
        "exp": 4070908800_u64
    }))?);
    Ok(format!("{header}.{payload}.{SUBJECT}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reinjection_repairs_local_membership_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        inject_if_missing_at(&path).await.unwrap();

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sqlx::query("UPDATE ItemTable SET value = 'free' WHERE key = ?")
            .bind("cursorAuth/stripeMembershipType")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("DELETE FROM ItemTable WHERE key = ?")
            .bind("cursorAuth/stripeMembershipAuthId")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        inject_if_missing_at(&path).await.unwrap();

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let membership_type: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind("cursorAuth/stripeMembershipType")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        let membership_auth_id: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind("cursorAuth/stripeMembershipAuthId")
                .fetch_one(&mut connection)
                .await
                .unwrap();

        assert_eq!(membership_type, MEMBERSHIP_TYPE);
        assert_eq!(membership_auth_id, SUBJECT);
    }

    #[test]
    fn recognizes_only_the_injected_cursor_token() {
        let token = local_token().unwrap();
        assert!(is_local_cursor_token(&token));
        assert!(is_local_cursor_authorization(&format!("Bearer {token}")));
        assert!(!is_local_cursor_authorization(&token));
        assert!(!is_local_cursor_authorization(
            "Bearer official-cursor-token"
        ));
        assert!(!is_local_cursor_token("official-cursor-token"));
        assert!(!is_local_cursor_token(""));
    }

    #[test]
    fn extracts_jwt_subject_from_valid_and_rejects_malformed_tokens() {
        let local = local_token().unwrap();
        assert_eq!(jwt_subject(&local).as_deref(), Some(SUBJECT));
        assert_eq!(
            authorization_subject(&format!("Bearer {local}")).as_deref(),
            Some(SUBJECT)
        );
        assert_eq!(
            authorization_subject(&format!("bearer {local}")).as_deref(),
            Some(SUBJECT)
        );

        let official_payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"auth0|user_123"}"#);
        let official = format!("eyJ0eXAiOiJKV1QifQ.{official_payload}.sig");
        assert_eq!(jwt_subject(&official).as_deref(), Some("auth0|user_123"));

        // Padded base64url payload is also accepted.
        let padded_payload = format!("{}=", URL_SAFE_NO_PAD.encode(br#"{"sub":"auth0|padded"}"#));
        assert_eq!(
            jwt_subject(&format!("hdr.{padded_payload}.sig")).as_deref(),
            Some("auth0|padded")
        );

        // Malformed inputs safely yield None.
        let blank_sub = URL_SAFE_NO_PAD.encode(br#"{"sub":"   "}"#);
        let numeric_sub = URL_SAFE_NO_PAD.encode(br#"{"sub":42}"#);
        let not_object = URL_SAFE_NO_PAD.encode(br#"["sub"]"#);
        let not_json = URL_SAFE_NO_PAD.encode(b"{bad-json");
        for malformed in [
            "",
            "not-a-jwt",
            "only.two",
            "too.many.dots.here",
            "hdr.!!!not-base64!!!.sig",
            &format!("hdr.{blank_sub}.sig"),
            &format!("hdr.{numeric_sub}.sig"),
            &format!("hdr.{not_object}.sig"),
            &format!("hdr.{not_json}.sig"),
        ] {
            assert_eq!(jwt_subject(malformed), None, "token={malformed:?}");
        }
        assert_eq!(authorization_subject(&local), None);
        assert_eq!(authorization_subject(&format!("Basic {local}")), None);
    }
}
