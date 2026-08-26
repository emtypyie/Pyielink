use rand::RngCore;
use sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

pub const TOKEN_LEN_BYTES: usize = 32;

pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

pub fn generate() -> String {
    let mut buf = [0u8; TOKEN_LEN_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    to_hex(&buf)
}

pub fn hash_token(token_hex: &str) -> String {
    let mut h = Sha256::new();
    h.update(token_hex.as_bytes());
    to_hex(&h.finalize())
}

/// Root of all pyielink local state. PYIELINK_HOME overrides the default
/// ~/.pyielink so containers can mount a volume (tests can isolate).
pub fn pyielink_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PYIELINK_HOME") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".pyielink")
}

pub fn tokens_dir() -> std::path::PathBuf {
    pyielink_dir().join("tokens")
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '@' || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

pub fn client_token_path(user: &str, ip: &str) -> std::path::PathBuf {
    tokens_dir().join(format!("{}@{}", sanitize(user), sanitize(ip)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub token_hash: String,
    pub created_at: u64,
    pub last_used_at: u64,
    pub user: String,
    pub ip: String,
}

impl TokenRecord {
    pub fn new(user: String, ip: String, token_hash: String) -> Self {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        Self {
            token_hash,
            created_at: now,
            last_used_at: now,
            user,
            ip,
        }
    }
}

fn ensure_tokens_dir() -> io::Result<()> {
    let dir = tokens_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

pub fn save_client_token(user: &str, ip: &str, token_hash: &str) -> io::Result<std::path::PathBuf> {
    ensure_tokens_dir()?;
    let path = client_token_path(user, ip);
    let record = TokenRecord::new(user.to_string(), ip.to_string(), token_hash.to_string());
    let json = serde_json::to_string_pretty(&record)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn load_client_token(user: &str, ip: &str) -> Option<String> {
    let path = client_token_path(user, ip);
    if !path.exists() {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    // Try new JSON format first
    if let Ok(record) = serde_json::from_str::<TokenRecord>(&content) {
        // Update last_used_at
        let mut record = record;
        record.last_used_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let _ = save_client_token(&record.user, &record.ip, &record.token_hash);
        return Some(record.token_hash);
    }
    // Fallback to legacy format (raw hash)
    let content = content.trim().to_string();
    if !content.is_empty() && content.len() == 64 && content.bytes().all(|b| b.is_ascii_hexdigit()) {
        // Upgrade to new format
        let _ = save_client_token(user, ip, &content);
        return Some(content);
    }
    None
}

pub fn delete_client_token(user: &str, ip: &str) -> io::Result<()> {
    let path = client_token_path(user, ip);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn list_client_tokens() -> Vec<TokenRecord> {
    let dir = tokens_dir();
    if !dir.exists() {
        return Vec::new();
    }
    let mut tokens = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(content) = fs::read_to_string(entry.path()) {
                if let Ok(record) = serde_json::from_str::<TokenRecord>(&content) {
                    tokens.push(record);
                }
            }
        }
    }
    tokens
}

pub fn verify_token(user: &str, ip: &str, token: &str) -> bool {
    if let Some(stored_hash) = load_client_token(user, ip) {
        let provided_hash = hash_token(token);
        stored_hash == provided_hash
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let bytes: Vec<u8> = (0u8..=255).cycle().take(64).collect();
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
        assert!(from_hex("").is_some());
    }

    #[test]
    fn token_is_64_hex_chars() {
        let t = generate();
        assert_eq!(t.len(), 64);
        assert!(from_hex(&t).is_some());
    }

    #[test]
    fn token_hash_is_deterministic_32_bytes() {
        assert_eq!(hash_token("ab"), hash_token("ab"));
        assert_ne!(hash_token("ab"), hash_token("cd"));
        assert_eq!(hash_token("ab").len(), 64);
    }

    #[test]
    fn sanitizes_hostile_names() {
        assert_eq!(
            client_token_path("../x", "::1").file_name().unwrap(),
            ".._x@__1"
        );
    }
}