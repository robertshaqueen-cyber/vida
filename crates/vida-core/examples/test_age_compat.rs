use std::fs;
use vida_core::vault::{self, AuthMethod, HostEntry, SecureString, Vault};

fn main() {
    let vault = Vault {
        version: 1,
        revision: 1,
        device_id: uuid::Uuid::new_v4().to_string(),
        modified_at: 0,
        hosts: vec![
            HostEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: "vps-hk".into(),
                host: "192.168.1.100".into(),
                user: "root".into(),
                port: 22,
                tags: vec!["production".into(), "hong-kong".into()],
                group: Some("servers".into()),
                color: Some("#ff6b6b".into()),
                auth: AuthMethod::Password {
                    password: SecureString::new("vps-root-pass".to_owned()),
                },
                notes: None,
                agent_trust: vida_core::agent_policy::AgentTrust::Ask,
            },
            HostEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: "dev-mac".into(),
                host: "10.0.0.50".into(),
                user: "admin".into(),
                port: 22,
                tags: vec!["development".into()],
                group: Some("local".into()),
                color: Some("#4ecdc4".into()),
                auth: AuthMethod::Key {
                    private_key_path: "~/.ssh/id_ed25519".into(),
                    passphrase: None,
                },
                notes: None,
                agent_trust: vida_core::agent_policy::AgentTrust::Ask,
            },
        ],
        ..Default::default()
    };

    let passphrase = "test-age-compat-123";

    println!("Encrypting vault...");
    let encrypted = vault::encrypt(&vault, passphrase).expect("Failed to encrypt");

    let out_path = "/tmp/test-vault.age";
    fs::write(out_path, &encrypted).expect("Failed to write vault file");
    println!("Encrypted vault written to {}", out_path);
    println!("File size: {} bytes", encrypted.len());

    // Show first few lines of the age file header
    let header = String::from_utf8_lossy(&encrypted);
    for line in header.lines().take(5) {
        println!("  {}", line);
    }

    println!("\nAttempting decryption with this crate...");
    let decrypted = vault::decrypt(&encrypted, passphrase).expect("Failed to decrypt");
    println!("Decrypted OK: {} hosts", decrypted.hosts.len());
    for h in &decrypted.hosts {
        println!("  - {} ({}@{}:{})", h.name, h.user, h.host, h.port);
    }
}
