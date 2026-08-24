use crate::token;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

const HASH_ROUNDS: u32 = 4096;
const ROLE_USER: &str = "user";
const ROLE_ADMIN: &str = "admin";

#[derive(Clone, Default)]
pub struct UserRecord {
    pub pw_salt: String,
    pub pw_hash: String,
    pub licensed: bool,
    pub token_hash: String,
    /// "user" (default) or "admin". Admins may run elevated (`sudo`) commands
    /// on the remote terminal channel; standard users cannot.
    pub role: String,
}

impl UserRecord {
    pub fn is_admin(&self) -> bool {
        self.role == ROLE_ADMIN
    }
}

#[derive(Clone, Default)]
pub struct HostState {
    pub enabled: bool,
    pub users: BTreeMap<String, UserRecord>,
    pub ip_whitelist: Vec<String>,
    pub allow_all_ips: bool,
}

pub fn state_path() -> std::path::PathBuf {
    token::pyielink_dir().join("host_state.json")
}

/* ---- state encryption at rest ----
 *
 * host_state.json is never stored as plaintext JSON. Contents are sealed
 * with AES-256-GCM under a random 32-byte master key held in host.key
 * (same directory). The file format:
 *   PYLENC1:<hex(12-byte nonce)><hex(ciphertext+16-byte tag)>
 * Legacy plaintext files are still readable and are re-sealed on the next
 * save. Losing host.key means accounts must be recreated — there is no
 * recovery path by design.
 */

const ENC_MAGIC: &str = "PYLENC1:";

fn key_path() -> std::path::PathBuf {
    token::pyielink_dir().join("host.key")
}

fn master_key() -> std::io::Result<[u8; 32]> {
    match std::fs::read_to_string(key_path()) {
        Ok(body) => {
            let raw = token::from_hex(body.trim())
                .filter(|k| k.len() == 32)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "corrupt host.key",
                    )
                })?;
            let mut out = [0u8; 32];
            out.copy_from_slice(&raw);
            Ok(out)
        }
        Err(_) => {
            use rand::RngCore;
            let mut k = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut k);
            let p = key_path();
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, token::to_hex(&k))?;
            Ok(k)
        }
    }
}

fn seal_state(json: &str) -> std::io::Result<String> {
    let key = master_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "aes init"))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, json.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "aes seal"))?;
    Ok(format!("{}{}{}", ENC_MAGIC, token::to_hex(nonce.as_slice()), token::to_hex(&ct)))
}

fn unseal_state(body: &str) -> Option<String> {
    let rest = body.trim().strip_prefix(ENC_MAGIC)?;
    if rest.len() < 24 + 32 || rest.len() % 2 != 0 {
        return None;
    }
    let nonce_hex = &rest[..24];
    let ct = token::from_hex(&rest[24..])?;
    let key = master_key().ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let plain = cipher.decrypt(Nonce::from_slice(token::from_hex(nonce_hex)?.as_slice()), ct.as_ref()).ok()?;
    String::from_utf8(plain).ok()
}

pub fn load_state() -> HostState {
    match std::fs::read_to_string(state_path()) {
        Ok(body) => {
            if body.trim_start().starts_with(ENC_MAGIC) {
                match unseal_state(&body) {
                    Some(json) => parse_state(&json).unwrap_or_default(),
                    None => {
                        eprintln!(
                            "  [warn] host state exists but cannot be decrypted (missing/corrupt {}). treating as empty.",
                            key_path().display()
                        );
                        HostState::default()
                    }
                }
            } else {
                // legacy plaintext file: usable now, re-sealed on next save
                parse_state(&body).unwrap_or_default()
            }
        }
        Err(_) => HostState::default(),
    }
}

static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn save_state(state: &HostState) -> std::io::Result<()> {
    let _guard = SAVE_LOCK.lock().unwrap();
    let path = state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Escape hatch for test harnesses and CI that inspect/edit the state
    // file directly. Never set this on a real deployment.
    let body = if std::env::var("PYIELINK_PLAINTEXT_STATE").as_deref() == Ok("1") {
        render_state(state)
    } else {
        seal_state(&render_state(state))?
    };
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

pub fn validate_username(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 32 {
        return Err("username must be 1-32 characters".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err("username may only contain letters, digits, '_', '.', '-'".into());
    }
    Ok(())
}

pub fn hash_password(salt_hex: &str, password: &str) -> String {
    let salt = token::from_hex(salt_hex).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(&salt);
    h.update(password.as_bytes());
    let mut acc = h.finalize();
    for _ in 1..HASH_ROUNDS {
        let mut r = Sha256::new();
        r.update(acc);
        r.update(&salt);
        acc = r.finalize();
    }
    token::to_hex(&acc)
}

pub fn new_salt() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    token::to_hex(&buf)
}

/* ---- challenge-response auth: secrets never cross the wire ---- */

/// Fresh hex nonce for a challenge (16 random bytes -> 32 chars).
pub fn new_nonce() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    token::to_hex(&buf)
}

/// proof = sha256(secret_hex || nonce_hex). The secret is the stored
/// password hash or the raw connection token — both stay client-side/host-side.
pub fn compute_proof(secret_hex: &str, nonce_hex: &str) -> String {
    let mut h = Sha256::new();
    h.update(secret_hex.as_bytes());
    h.update(nonce_hex.as_bytes());
    token::to_hex(&h.finalize())
}

pub fn verify_proof(expected_secret_hex: &str, nonce_hex: &str, proof_hex: &str) -> bool {
    if expected_secret_hex.is_empty()
        || nonce_hex.is_empty()
        || proof_hex.len() != 64
        || !proof_hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return false;
    }
    // constant-time-ish comparison to avoid trivially leaking position of first diff
    let a = compute_proof(expected_secret_hex, nonce_hex);
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(proof_hex.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn normalize_role(role: &str) -> Result<String, String> {
    match role.trim().to_ascii_lowercase().as_str() {
        "user" | "standard" | "" => Ok(ROLE_USER.to_string()),
        "admin" | "sudo" => Ok(ROLE_ADMIN.to_string()),
        other => Err(format!("unknown role '{}' (use 'user' or 'admin')", other)),
    }
}

pub fn add_user(name: &str, role: &str) -> Result<(), String> {
    validate_username(name)?;
    let role = normalize_role(role)?;
    let mut state = load_state();
    if state.users.contains_key(name) {
        return Err(format!("user '{}' already exists", name));
    }
    println!("creating account '{}' (role: {})...", name, role);
    let pw = prompt_new_password()?;
    let salt = new_salt();
    let hash = hash_password(&salt, &pw);
    state.users.insert(
        name.to_string(),
        UserRecord {
            pw_salt: salt,
            pw_hash: hash,
            licensed: false,
            token_hash: String::new(),
            role,
        },
    );
    save_state(&state).map_err(|e| format!("could not save state: {}", e))?;
    println!("  [ok] user '{}' created (state sealed with {})", name, key_path().display());
    println!("       note: run /enable to open this device for connections.");
    Ok(())
}

pub fn cmd_enable() -> Result<(), String> {
    let mut state = load_state();
    if state.users.is_empty() {
        return Err("no user accounts exist yet. run '/adduser -m <name>' first.".into());
    }
    if !state.enabled {
        state.enabled = true;
        save_state(&state).map_err(|e| format!("could not save state: {}", e))?;
    }
    Ok(())
}

pub fn cmd_enable_with_flags(allow_all: bool, whitelist: Vec<String>) -> Result<(), String> {
    let mut state = load_state();
    if state.users.is_empty() {
        return Err("no user accounts exist yet. run '/adduser -m <name>' first.".into());
    }
    state.enabled = true;
    state.allow_all_ips = allow_all;
    state.ip_whitelist = whitelist;
    save_state(&state).map_err(|e| format!("could not save state: {}", e))?;
    Ok(())
}

/// Add an IP to the whitelist and persist state.
/// Whitelist entries are IP address strings (e.g., "192.168.1.100").
pub fn add_to_whitelist(ip: String) -> Result<(), String> {
    let mut state = load_state();
    if state.users.is_empty() {
        return Err("no user accounts exist. run '/adduser -m <name>' first.".into());
    }
    // Add to whitelist if not already present
    if !state.ip_whitelist.contains(&ip) {
        state.ip_whitelist.push(ip);
        save_state(&state).map_err(|e| format!("could not save state: {}", e))?;
    }
    Ok(())
}

/// Remove an IP from the whitelist and persist state.
pub fn remove_from_whitelist(ip: &str) -> Result<(), String> {
    let mut state = load_state();
    if state.users.is_empty() {
        return Err("no user accounts exist. run '/adduser -m <name>' first.".into());
    }
    // Remove from whitelist
    state.ip_whitelist.retain(|x| x != ip);
    save_state(&state).map_err(|e| format!("could not save state: {}", e))?;
    Ok(())
}

/* ---- interactive prompts ---- */

#[cfg(windows)]
unsafe extern "system" {
    fn _getch() -> i32;
    fn GetStdHandle(n_std_handle: isize) -> isize;
    fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
}

/// True when stdin is a real console (safe for _getch). False for pipes,
/// which is how emtypyie.cli's GUI wrapper spawns engines.
#[cfg(windows)]
fn stdin_is_console() -> bool {
    const STD_INPUT_HANDLE: isize = -10;
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if h == 0 || h == -1 {
        return false;
    }
    let mut mode: u32 = 0;
    unsafe { GetConsoleMode(h, &mut mode) == 1 }
}

/// True when stdin is a real console (safe for _getch). False for pipes,
/// which is how emtypyie.cli's GUI wrapper spawns engines.
#[cfg(windows)]
pub fn stdin_is_tty() -> bool {
    stdin_is_console()
}

/// Non-Windows builds have no console probe without extra deps; callers
/// treat stdin as non-tty unless PYIELINK_SHELL=1 forces interactive mode.
#[cfg(not(windows))]
pub fn stdin_is_tty() -> bool {
    false
}

pub fn read_masked() -> String {
    #[cfg(windows)]
    {
        if stdin_is_console() {
            return read_masked_tty();
        }
    }
    // Piped / redirected stdin fallback (scripting, GUI wrappers)
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim_end_matches(['\r', '\n']).to_string()
}

#[cfg(windows)]
fn read_masked_tty() -> String {
    print!("> ");
    let _ = std::io::stdout().flush();
    let mut pw: Vec<u8> = Vec::new();
    loop {
        let c = unsafe { _getch() };
        match c {
            13 | 10 => break,
            8 => {
                if pw.pop().is_some() {
                    print!("\x08 \x08");
                    let _ = std::io::stdout().flush();
                }
            }
            3 => std::process::exit(130),
            0 | 0xE0 => {
                unsafe { _getch(); }
            }
            32..=255 => {
                pw.push(c as u8);
                print!("*");
                let _ = std::io::stdout().flush();
            }
            _ => {}
        }
    }
    println!();
    String::from_utf8_lossy(&pw).into_owned()
}

pub fn prompt_new_password() -> Result<String, String> {
    print!("create password: ");
    let _ = std::io::stdout().flush();
    let a = read_masked();
    if a.is_empty() {
        return Err("password cannot be empty".into());
    }
    print!("confirm password: ");
    let _ = std::io::stdout().flush();
    let b = read_masked();
    if a != b {
        return Err("passwords do not match. user not created.".into());
    }
    Ok(a)
}

pub fn read_line_prompt(prompt: &str) -> String {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line.trim().to_string()
}

/* ---- minimal JSON for the fixed host_state schema ---- */

pub fn render_state(state: &HostState) -> String {
    let mut out = String::from("{\n  \"enabled\": ");
    out.push_str(if state.enabled { "true" } else { "false" });
    out.push_str(",\n  \"allow_all_ips\": ");
    out.push_str(if state.allow_all_ips { "true" } else { "false" });
    out.push_str(",\n  \"ip_whitelist\": [");
    for (i, ip) in state.ip_whitelist.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("\"{}\"", ip));
    }
    out.push_str("],\n  \"users\": {\n");
    let mut first = true;
    for (name, u) in &state.users {
        if !first {
            out.push_str(",\n");
        }
        first = false;
        out.push_str(&format!(
            "    \"{}\": {{ \"pw_salt\": \"{}\", \"pw_hash\": \"{}\", \"licensed\": {}, \"token_hash\": \"{}\", \"role\": \"{}\" }}",
            name, u.pw_salt, u.pw_hash, u.licensed, u.token_hash,
            if u.role.is_empty() { ROLE_USER } else { &u.role }
        ));
    }
    if !first {
        out.push('\n');
    }
    out.push_str("  }\n}\n");
    out
}

struct Scan<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Scan<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.i < self.s.len() && self.s[self.i] == c {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }
    fn str_lit(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i] != b'"' {
            self.i += 1;
        }
        if self.i >= self.s.len() {
            return None;
        }
        let v = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        self.i += 1;
        Some(v)
    }
    fn need(&mut self, c: u8) -> Option<()> {
        if self.eat(c) {
            Some(())
        } else {
            None
        }
    }
    fn need_key(&mut self, want: &str) -> Option<()> {
        let k = self.str_lit().unwrap_or_default();
        if k == want && self.eat(b':') {
            Some(())
        } else {
            None
        }
    }
    fn boolean(&mut self) -> Option<bool> {
        self.ws();
        if self.s[self.i..].starts_with(b"true") {
            self.i += 4;
            Some(true)
        } else if self.s[self.i..].starts_with(b"false") {
            self.i += 5;
            Some(false)
        } else {
            None
        }
    }
}

pub fn parse_state(body: &str) -> Option<HostState> {
    let mut sc = Scan { s: body.as_bytes(), i: 0 };
    let mut state = HostState::default();
    if !sc.eat(b'{') {
        return None;
    }
    loop {
        match sc.peek()? {
            b'}' => {
                sc.eat(b'}');
                break;
            }
            b',' => {
                sc.eat(b',');
            }
            b'"' => {
                let probe = Scan { s: sc.s, i: sc.i };
                if probe.clone_key_is("enabled") {
                    sc.need_key("enabled")?;
                    state.enabled = sc.boolean()?;
                } else if probe.clone_key_is("allow_all_ips") {
                    sc.need_key("allow_all_ips")?;
                    state.allow_all_ips = sc.boolean()?;
                } else if probe.clone_key_is("ip_whitelist") {
                    sc.need_key("ip_whitelist")?;
                    sc.need(b'[')?;
                    loop {
                        match sc.peek()? {
                            b']' => {
                                sc.eat(b']');
                                break;
                            }
                            b',' => {
                                sc.eat(b',');
                            }
                            b'"' => {
                                let ip = sc.str_lit()?;
                                state.ip_whitelist.push(ip);
                            }
                            _ => return None,
                        }
                    }
                } else if probe.clone_key_is("users") {
                    sc.need_key("users")?;
                    sc.need(b'{')?;
                    loop {
                        match sc.peek()? {
                            b'}' => {
                                sc.eat(b'}');
                                break;
                            }
                            b',' => {
                                sc.eat(b',');
                            }
                            b'"' => {
                                let name = sc.str_lit()?;
                                sc.need(b':')?;
                                let rec = parse_user(&mut sc)?;
                                state.users.insert(name, rec);
                            }
                            _ => return None,
                        }
                    }
                } else {
                    let _ = sc.str_lit();
                    skip_value(&mut sc)?;
                }
            }
            _ => return None,
        }
    }
    Some(state)
}

impl<'a> Scan<'a> {
    fn clone_key_is(&self, want: &str) -> bool {
        let mut p = Scan { s: self.s, i: self.i };
        let k = p.str_lit().unwrap_or_default();
        k == want
    }
}

fn parse_user(sc: &mut Scan) -> Option<UserRecord> {
    let mut rec = UserRecord::default();
    if !sc.eat(b'{') {
        return None;
    }
    loop {
        match sc.peek()? {
            b'}' => {
                sc.eat(b'}');
                return Some(rec);
            }
            b',' => {
                sc.eat(b',');
            }
            b'"' => {
                if sc.clone_key_is("pw_salt") {
                    sc.need_key("pw_salt")?;
                    rec.pw_salt = sc.str_lit()?;
                } else if sc.clone_key_is("pw_hash") {
                    sc.need_key("pw_hash")?;
                    rec.pw_hash = sc.str_lit()?;
                } else if sc.clone_key_is("licensed") {
                    sc.need_key("licensed")?;
                    rec.licensed = sc.boolean()?;
                } else if sc.clone_key_is("token_hash") {
                    sc.need_key("token_hash")?;
                    rec.token_hash = sc.str_lit()?;
                } else if sc.clone_key_is("role") {
                    sc.need_key("role")?;
                    rec.role = sc.str_lit().unwrap_or_default();
                } else {
                    let _ = sc.str_lit();
                    sc.need(b':')?;
                    skip_value(sc)?;
                }
            }
            _ => return None,
        }
    }
}

fn skip_value(sc: &mut Scan) -> Option<()> {
    match sc.peek()? {
        b'"' => {
            let _ = sc.str_lit();
        }
        _ => {
            while let Some(c) = sc.peek() {
                if c == b',' || c == b'}' {
                    break;
                }
                sc.i += 1;
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_rules() {
        assert!(validate_username("bob_01").is_ok());
        assert!(validate_username("a").is_ok());
        assert!(validate_username("").is_err());
        assert!(validate_username("has space").is_err());
        assert!(validate_username("bad@at").is_err());
        assert!(validate_username(&"x".repeat(33)).is_err());
    }

    #[test]
    fn password_hash_round_trip() {
        let salt = new_salt();
        let h = hash_password(&salt, "hunter2");
        assert_eq!(h.len(), 64);
        // same password + same salt must derive the identical stored secret
        assert_eq!(hash_password(&salt, "hunter2"), h);
        assert_ne!(hash_password(&salt, "hunter3"), h);
    }

    #[test]
    fn unique_salts_change_hash() {
        let h1 = hash_password(&new_salt(), "same");
        let h2 = hash_password(&new_salt(), "same");
        assert_ne!(h1, h2);
    }

    #[test]
    fn proof_symmetry() {
        let secret = "aabbccdd";
        let nonce = new_nonce();
        assert_eq!(compute_proof(secret, &nonce), compute_proof(secret, &nonce));
        assert!(verify_proof(secret, &nonce, &compute_proof(secret, &nonce)));
        assert!(!verify_proof(secret, &nonce, &compute_proof(secret, "other")));
        assert!(!verify_proof("0000", &nonce, &compute_proof(secret, &nonce)));
    }

    #[test]
    fn proof_rejects_malformed() {
        let nonce = new_nonce();
        assert!(!verify_proof("", &nonce, &"a".repeat(64)));
        assert!(!verify_proof("aa", &nonce, "short"));
        assert!(!verify_proof("aa", &nonce, &"z".repeat(64)));
        // empty nonce must never verify
        assert!(!verify_proof("aa", "", &compute_proof("aa", "")));
    }

    #[test]
    fn json_round_trip() {
        let mut state = HostState { enabled: true, users: BTreeMap::new() };
        state.users.insert(
            "bob".into(),
            UserRecord {
                pw_salt: "aabb".into(),
                pw_hash: "ccdd".into(),
                licensed: true,
                token_hash: "eeff".into(),
                role: "user".into(),
            },
        );
        state.users.insert("alice".into(), UserRecord::default());
        let rendered = render_state(&state);
        let parsed = parse_state(&rendered).unwrap();
        assert_eq!(parsed.enabled, true);
        assert_eq!(parsed.users.len(), 2);
        assert_eq!(parsed.users["bob"].pw_hash, "ccdd");
        assert_eq!(parsed.users["bob"].licensed, true);
        assert_eq!(parsed.users["bob"].token_hash, "eeff");
        assert_eq!(parsed.users["alice"].licensed, false);
    }

    #[test]
    fn parses_empty_users_and_defaults() {
        let parsed = parse_state("{ \"enabled\": false, \"users\": {} }").unwrap();
        assert!(!parsed.enabled);
        assert!(parsed.users.is_empty());
        assert!(parse_state("garbage").is_none());
    }

    #[test]
    fn role_render_parse_round_trip() {
        let mut state = HostState::default();
        state.users.insert(
            "rooty".into(),
            UserRecord { role: "admin".into(), pw_salt: "aa".into(), pw_hash: "bb".into(), ..Default::default() },
        );
        state.users.insert("plain".into(), UserRecord::default());
        let parsed = parse_state(&render_state(&state)).unwrap();
        assert!(parsed.users["rooty"].is_admin());
        assert!(!parsed.users["plain"].is_admin());
        // legacy file without role field still parses, defaults to user
        let legacy = "{ \"enabled\": false, \"users\": { \"old\": { \"pw_salt\": \"s\", \"pw_hash\": \"h\", \"licensed\": false, \"token_hash\": \"\" } } }";
        let p2 = parse_state(legacy).unwrap();
        assert!(!p2.users["old"].is_admin());
    }

    #[test]
    fn normalize_roles() {
        assert_eq!(normalize_role("admin").unwrap(), "admin");
        assert_eq!(normalize_role("SUDO").unwrap(), "admin");
        assert_eq!(normalize_role("user").unwrap(), "user");
        assert_eq!(normalize_role("").unwrap(), "user");
        assert!(normalize_role("root").is_err());
    }

    #[test]
    fn seal_unseal_round_trip() {
        let json = r#"{ "enabled": true, "users": { "x": { "pw_salt": "aa" } } }"#;
        let sealed = seal_state(json).unwrap();
        assert!(sealed.starts_with(ENC_MAGIC));
        assert!(!sealed.contains("pw_salt"));
        assert_eq!(unseal_state(&sealed).unwrap(), json);
        // tampered ciphertext must not decrypt
        let mut bad = sealed.clone();
        bad.replace_range(30..32, "zz");
        assert!(unseal_state(&bad).is_none());
    }
}
