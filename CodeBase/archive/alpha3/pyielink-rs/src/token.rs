use rand::RngCore;
use sha2::{Digest, Sha256};

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
/// ~/.pyielink so containers can mount a volume (and tests can isolate).
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

pub fn save_client_token(user: &str, ip: &str, token: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = tokens_dir();
    std::fs::create_dir_all(&dir)?;
    let path = client_token_path(user, ip);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, token)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn load_client_token(user: &str, ip: &str) -> Option<String> {
    std::fs::read_to_string(client_token_path(user, ip))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
