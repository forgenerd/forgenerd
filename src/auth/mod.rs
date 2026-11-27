pub mod crypto;
pub mod throttle;

use anyhow::anyhow;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use crate::api::AppState;
use crate::db::models::User;
use crate::error::{AppError, AppResult};

pub const SESSION_COOKIE: &str = "forgenerd_session";
pub const SESSION_TTL_DAYS: i64 = 7;

/// Argon2id with OWASP's m=7 MiB, t=5, p=1 profile: equivalent strength to the crate
/// default (m=19 MiB, t=2) at about a third of the memory, which matters on a 1 GB VPS
/// with a <30 MB control-plane budget. Verification reads parameters from each stored
/// hash, so hashes made with other parameters keep working.
const ARGON2_PARAMS: Params = match Params::new(7 * 1024, 5, 1, None) {
    Ok(params) => params,
    Err(_) => panic!("invalid Argon2 parameters"),
};

fn argon2() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, ARGON2_PARAMS)
}

/// Argon2id allocates m_cost KiB per hash. Running one at a time bounds peak memory on a
/// 1 GB VPS (allocators may keep a freed buffer per thread) and throttles login floods.
static ARGON2_PERMIT: Semaphore = Semaphore::const_new(1);

async fn run_argon2<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> AppResult<T> {
    let _permit = ARGON2_PERMIT
        .acquire()
        .await
        .map_err(|e| anyhow!("argon2 semaphore closed: {e}"))?;
    let result = tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| anyhow!(e))??;
    Ok(result)
}

pub const MIN_PASSWORD_LEN: usize = 12;

/// Argon2id hash in PHC format.
pub async fn hash_password(password: String) -> AppResult<String> {
    run_argon2(move || {
        argon2()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| anyhow!("hashing password: {e}"))
    })
    .await
}

pub async fn verify_password(password: String, phc: String) -> AppResult<bool> {
    run_argon2(move || {
        let parsed = PasswordHash::new(&phc).map_err(|e| anyhow!("stored hash invalid: {e}"))?;
        Ok(argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    })
    .await
}

/// 32 random bytes, base64url without padding.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Only the SHA-256 of a token is ever stored.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Create a session row and return the raw cookie token.
pub async fn create_session(db: &SqlitePool, user_id: &str) -> AppResult<String> {
    let token = new_token();
    let expires_at = (Utc::now() + Duration::days(SESSION_TTL_DAYS)).naive_utc();
    sqlx::query("INSERT INTO sessions (id, token_hash, user_id, expires_at) VALUES (?, ?, ?, ?)")
        .bind(crate::db::new_id())
        .bind(token_hash(&token))
        .bind(user_id)
        .bind(expires_at)
        .execute(db)
        .await?;
    Ok(token)
}

pub async fn delete_session(db: &SqlitePool, token: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash(token))
        .execute(db)
        .await?;
    Ok(())
}

pub async fn purge_expired_sessions(db: &SqlitePool) -> AppResult<u64> {
    let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(Utc::now().naive_utc())
        .execute(db)
        .await?;
    Ok(result.rows_affected())
}

/// Authenticated caller, resolved from the session cookie or a Bearer API token.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user: User,
    /// Hash of the session token when authenticated by cookie.
    pub session_hash: Option<String>,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(token) = bearer_token(parts) {
            let user = sqlx::query_as::<_, User>(
                "SELECT u.* FROM api_tokens t JOIN users u ON u.id = t.user_id WHERE t.token_hash = ?",
            )
            .bind(token_hash(token))
            .fetch_optional(&state.db)
            .await?;
            return user
                .map(|user| Self {
                    user,
                    session_hash: None,
                })
                .ok_or(AppError::Unauthorized);
        }

        let jar = CookieJar::from_headers(&parts.headers);
        let Some(cookie) = jar.get(SESSION_COOKIE) else {
            return Err(AppError::Unauthorized);
        };
        let hash = token_hash(cookie.value());
        let user = sqlx::query_as::<_, User>(
            "SELECT u.* FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ? AND s.expires_at > ?",
        )
        .bind(&hash)
        .bind(Utc::now().naive_utc())
        .fetch_optional(&state.db)
        .await?;
        user.map(|user| Self {
            user,
            session_hash: Some(hash),
        })
        .ok_or(AppError::Unauthorized)
    }
}

fn bearer_token(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn password_roundtrip() {
        let hash = hash_password("correct horse battery".into()).await.unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=7168,t=5,p=1$"), "{hash}");
        assert!(
            verify_password("correct horse battery".into(), hash.clone())
                .await
                .unwrap()
        );
        assert!(!verify_password("wrong".into(), hash).await.unwrap());
    }

    #[test]
    fn tokens_are_random_and_hashed() {
        let a = new_token();
        assert_ne!(a, new_token());
        assert_eq!(token_hash(&a).len(), 64);
        assert_ne!(token_hash(&a), a);
    }
}
