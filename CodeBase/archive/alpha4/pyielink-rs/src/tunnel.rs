use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use std::thread;
use std::io::{BufRead, BufReader};

const DEFAULT_TUNNEL_PORT: u16 = 4242;

#[derive(Debug)]
pub struct TunnelConfig {
    pub enabled: bool,
    pub allow_all: bool,
    pub whitelist: Vec<String>,
    pub tunnel_type: TunnelType,
    pub tunnel_url: Option<String>,
    pub local_port: u16,
    pub public_url: Option<String>,
    pub process: Option<std::process::Child>,
}

// `Child` is not `Clone`, so implement `Clone` manually and drop the
// process handle (clones are used for read-only config snapshots).
impl Clone for TunnelConfig {
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled,
            allow_all: self.allow_all,
            whitelist: self.whitelist.clone(),
            tunnel_type: self.tunnel_type.clone(),
            tunnel_url: self.tunnel_url.clone(),
            local_port: self.local_port,
            public_url: self.public_url.clone(),
            process: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TunnelType {
    None,
    Cloudflare,
    Ngrok,
    Custom,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_all: false,
            whitelist: Vec::new(),
            tunnel_type: TunnelType::None,
            tunnel_url: None,
            local_port: DEFAULT_TUNNEL_PORT,
            public_url: None,
            process: None,
        }
    }
}

pub struct TunnelManager {
    config: Arc<Mutex<TunnelConfig>>,
}

impl TunnelManager {
    pub fn new() -> Self {
        Self {
            config: Arc::new(Mutex::new(TunnelConfig::default())),
        }
    }

    pub fn get_config(&self) -> TunnelConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn set_whitelist(&self, whitelist: Vec<String>) {
        let mut config = self.config.lock().unwrap();
        config.whitelist = whitelist;
    }

    pub fn add_to_whitelist(&self, ip: String) {
        let mut config = self.config.lock().unwrap();
        if !config.whitelist.contains(&ip) {
            config.whitelist.push(ip);
        }
    }

    pub fn remove_from_whitelist(&self, ip: &str) {
        let mut config = self.config.lock().unwrap();
        config.whitelist.retain(|ip_str| ip_str != ip);
    }

    pub fn set_allow_all(&self, allow: bool) {
        let mut config = self.config.lock().unwrap();
        config.allow_all = allow;
    }

    pub fn is_ip_allowed(&self, ip: &str) -> bool {
        let config = self.config.lock().unwrap();
        if config.allow_all {
            return true;
        }
        config.whitelist.iter().any(|allowed| allowed == ip)
    }

    pub async fn start_tunnel(&self, tunnel_type: TunnelType, local_port: u16) -> Result<String, String> {
        let mut config = self.config.lock().unwrap();
        
        if config.enabled {
            return Err("Tunnel already running".into());
        }

        config.tunnel_type = tunnel_type.clone();
        config.local_port = local_port;

        let public_url = match tunnel_type {
            TunnelType::Cloudflare => self.start_cloudflare_tunnel(local_port).await?,
            TunnelType::Ngrok => self.start_ngrok_tunnel(local_port).await?,
            TunnelType::Custom => {
                return Err("Custom tunnel type not implemented".into());
            }
            TunnelType::None => {
                return Err("No tunnel type specified".into());
            }
        };

        config.enabled = true;
        config.public_url = Some(public_url.clone());
        config.tunnel_type = tunnel_type;

        Ok(public_url)
    }

    async fn start_cloudflare_tunnel(&self, local_port: u16) -> Result<String, String> {
        // Try to use cloudflared
        let mut output = Command::new("cloudflared")
            .args(["tunnel", "--url", &format!("http://localhost:{}", local_port)])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to start cloudflared: {}", e))?;

        // Read stdout to get the tunnel URL
        let stdout = output.stdout.take().ok_or("Failed to capture stdout")?;
        let reader = BufReader::new(stdout);
        
        for line in BufReader::new(reader).lines() {
            let line = line.map_err(|_| "Failed to read line")?;
            if line.contains("trycloudflare.com") || line.contains("https://") {
                // Extract the URL
                if let Some(url_start) = line.find("https://") {
                    let url = &line[url_start..];
                    let url = url.split_whitespace().next().unwrap_or("");
                    return Ok(url.to_string());
                }
            }
        }

        Err("Failed to get cloudflare tunnel URL".into())
    }

    async fn start_ngrok_tunnel(&self, local_port: u16) -> Result<String, String> {
        // Try to use ngrok
        let output = Command::new("ngrok")
            .args(["http", local_port.to_string().as_str()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("Failed to start ngrok: {}", e))?;

        let output_str = String::from_utf8_lossy(&output.stdout);
        
        // Parse ngrok output for the public URL
        for line in output_str.lines() {
            if line.contains("https://") && line.contains("ngrok") {
                if let Some(url_start) = line.find("https://") {
                    let url = &line[url_start..];
                    let url = url.split_whitespace().next().unwrap_or("");
                    return Ok(url.to_string());
                }
            }
        }

        Err("Failed to get ngrok tunnel URL".into())
    }

    pub fn stop_tunnel(&self) -> Result<(), String> {
        let mut config = self.config.lock().unwrap();
        
        if let Some(mut child) = config.process.take() {
            let _ = child.kill();
        }
        
        config.enabled = false;
        config.public_url = None;
        config.tunnel_type = TunnelType::None;
        
        Ok(())
    }

    pub fn get_tunnel_info(&self) -> TunnelConfig {
        self.config.lock().unwrap().clone()
    }
}

// Global tunnel manager instance
static TUNNEL_MANAGER: OnceLock<TunnelManager> = OnceLock::new();

pub fn tunnel_manager() -> &'static TunnelManager {
    TUNNEL_MANAGER.get_or_init(TunnelManager::new)
}