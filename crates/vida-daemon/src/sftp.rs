//! Owner-only file transfer through the system OpenSSH `sftp` client.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use vida_core::vault::{AuthMethod, HostEntry};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SftpEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub permissions: String,
    pub modified: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SftpListing {
    pub path: String,
    pub entries: Vec<SftpEntry>,
}

struct PreparedAuth {
    key_path: Option<PathBuf>,
    password_only: bool,
    secret: Option<SecretString>,
    temporary_key: Option<PathBuf>,
}

impl Drop for PreparedAuth {
    fn drop(&mut self) {
        if let Some(path) = self.temporary_key.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn prepare_auth(auth: AuthMethod) -> Result<PreparedAuth> {
    match auth {
        AuthMethod::Password { password } => Ok(PreparedAuth {
            key_path: None,
            password_only: true,
            secret: Some(SecretString::from(password.expose().to_owned())),
            temporary_key: None,
        }),
        AuthMethod::Key {
            private_key_path,
            passphrase,
        } => Ok(PreparedAuth {
            key_path: Some(PathBuf::from(private_key_path)),
            password_only: false,
            secret: passphrase.map(|p| SecretString::from(p.expose().to_owned())),
            temporary_key: None,
        }),
        AuthMethod::KeyInline {
            private_key,
            passphrase,
        } => {
            let directory = vida_core::config::config_dir()?.join("ssh-keys");
            std::fs::create_dir_all(&directory).context("无法创建 SFTP 临时密钥目录")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
                let path = directory.join(format!("{}.sftp.key", Uuid::new_v4()));
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                file.write_all(private_key.expose().as_bytes())?;
                file.flush()?;
                Ok(PreparedAuth {
                    key_path: Some(path.clone()),
                    password_only: false,
                    secret: passphrase.map(|p| SecretString::from(p.expose().to_owned())),
                    temporary_key: Some(path),
                })
            }
            #[cfg(not(unix))]
            anyhow::bail!("当前平台暂不支持内嵌私钥 SFTP")
        }
    }
}

fn quote(value: &str) -> Result<String> {
    if value.is_empty() || value.contains(['\n', '\r', '\0']) {
        anyhow::bail!("SFTP 路径无效，请选择有效文件或目录");
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn run(host: HostEntry, batch: String) -> Result<String> {
    if host.host.is_empty()
        || host.host.starts_with('-')
        || host.user.is_empty()
        || host.user.starts_with('-')
    {
        anyhow::bail!("SFTP 主机配置无效，请编辑主机地址和用户名");
    }
    let auth = prepare_auth(host.auth)?;
    let mut command = Command::new("sftp");
    // Keep protocol status text (not remote filenames) stable for parsing.
    command.env("LC_ALL", "C");
    command.args([
        "-q",
        "-P",
        &host.port.to_string(),
        "-o",
        "ConnectTimeout=15",
        "-o",
        "BatchMode=no",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]);
    if auth.password_only {
        command.args([
            "-o",
            "PreferredAuthentications=password,keyboard-interactive",
            "-o",
            "PubkeyAuthentication=no",
        ]);
    } else if let Some(path) = &auth.key_path {
        command
            .arg("-i")
            .arg(path)
            .args(["-o", "IdentitiesOnly=yes"]);
    }
    command.arg(format!("{}@{}", host.user, host.host));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut broker = None;
    if let Some(secret) = auth.secret.as_ref() {
        let (created, env) = crate::ssh_auth::AskpassBroker::start(SecretString::from(
            secret.expose_secret().to_owned(),
        ))?;
        crate::ssh_auth::apply_env_process(&mut command, &env)?;
        broker = Some(created);
    }
    let mut child = command
        .spawn()
        .context("无法启动系统 sftp；请确认 OpenSSH 客户端已安装")?;
    child
        .stdin
        .take()
        .context("无法写入 sftp 命令")?
        .write_all(batch.as_bytes())?;
    let output = child.wait_with_output().context("等待 sftp 完成失败")?;
    drop(broker);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let diagnostic = actionable_stderr(&stderr);
    // Interactive `sftp` commonly exits with status 0 even when an individual
    // batch command failed (for example one unreadable child during `get -R`).
    // Treat command diagnostics as failure so the GUI never reports an empty
    // partially-created directory as a completed recursive download.
    if !output.status.success() || !diagnostic.is_empty() {
        anyhow::bail!(
            "SFTP 操作失败：{}。请检查网络、权限和远端路径",
            if diagnostic.is_empty() {
                "远端拒绝请求"
            } else {
                &diagnostic
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn actionable_stderr(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Warning: Permanently added "))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn list(host: HostEntry, path: &str) -> Result<SftpListing> {
    // `cd` first so `pwd` reports the server-canonical absolute directory.
    // This matters for the initial relative `.` (normally the remote home):
    // the GUI must know it is really `/root`, `/home/alice`, etc. to navigate up.
    let output = run(host, format!("cd {}\npwd\nls -la .\nbye\n", quote(path)?))?;
    let path = parse_working_directory(&output).context("SFTP 未返回远端当前目录；请刷新后重试")?;
    Ok(SftpListing {
        path,
        entries: parse_listing(&output, "."),
    })
}

pub fn create_directory(host: HostEntry, parent: &str, name: &str) -> Result<()> {
    validate_directory_name(name)?;
    let remote = join_remote(parent, name);
    run(host, format!("mkdir {}\nbye\n", quote(&remote)?))?;
    Ok(())
}

pub fn upload(host: HostEntry, local: &Path, remote: &str) -> Result<()> {
    if !local.exists() {
        anyhow::bail!("本地上传内容不存在，请重新选择文件或文件夹");
    }
    run(host, upload_batch(local, remote, local.is_dir())?)?;
    Ok(())
}

fn upload_batch(local: &Path, remote: &str, recursive: bool) -> Result<String> {
    Ok(format!(
        "put {}{} {}\nbye\n",
        if recursive { "-R " } else { "" },
        quote(&local.to_string_lossy())?,
        quote(remote)?
    ))
}

pub fn download(host: HostEntry, remote: &str, local: &Path, recursive: bool) -> Result<()> {
    run(host, download_batch(remote, local, recursive)?)?;
    Ok(())
}

fn download_batch(remote: &str, local: &Path, recursive: bool) -> Result<String> {
    Ok(format!(
        "get {}{} {}\nbye\n",
        if recursive { "-R " } else { "" },
        quote(remote)?,
        quote(&local.to_string_lossy())?
    ))
}

fn parse_listing(output: &str, path: &str) -> Vec<SftpEntry> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("sftp>") || line.starts_with("Connected to") {
                return None;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 9 || !matches!(parts[0].as_bytes().first(), Some(b'-' | b'd' | b'l')) {
                return None;
            }
            let listed_name = parts[8..].join(" ");
            let prefix = if path == "/" {
                "/".to_string()
            } else {
                format!("{}/", path.trim_end_matches('/'))
            };
            let name = listed_name
                .strip_prefix(&prefix)
                .unwrap_or(&listed_name)
                .to_string();
            if name == "." || name == ".." {
                return None;
            }
            Some(SftpEntry {
                name,
                is_dir: parts[0].starts_with('d'),
                size: parts[4].parse().unwrap_or(0),
                permissions: parts[0].to_string(),
                modified: parts[5..8].join(" "),
            })
        })
        .collect()
}

fn parse_working_directory(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Remote working directory: ")
            .map(str::to_owned)
    })
}

fn validate_directory_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains(['\n', '\r', '\0'])
        || name.chars().count() > 255
    {
        anyhow::bail!("文件夹名称无效：请使用不含斜杠的单个名称");
    }
    Ok(())
}

fn join_remote(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_unix_listing_and_preserves_spaces() {
        let rows = parse_listing(
            "drwxr-xr-x  2 root root 4096 Aug 10 12:00 logs\n-rw-r--r-- 1 root root 12 Aug 10 12:01 hello world.txt\n",
            ".",
        );
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_dir);
        assert_eq!(rows[1].name, "hello world.txt");
        assert_eq!(rows[1].size, 12);
    }

    #[test]
    fn strips_sftp_directory_prefix_and_dot_entries() {
        let rows = parse_listing(
            "drwx------ ? root root 4096 Aug 10 12:00 /tmp/.\ndrwxr-xr-x ? root root 4096 Aug 10 12:00 /tmp/..\n-rw-r--r-- ? root root 12 Aug 10 12:01 /tmp/a b.txt\n",
            "/tmp",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a b.txt");
    }
    #[test]
    fn rejects_batch_control_characters() {
        assert!(quote("/tmp/x\nbye").is_err());
    }

    #[test]
    fn reads_canonical_remote_working_directory() {
        let output = "Remote working directory: /root\n-rw-r--r-- 1 root root 2 Aug 10 12:00 ./a\n";
        assert_eq!(parse_working_directory(output).as_deref(), Some("/root"));
    }

    #[test]
    fn validates_new_directory_name() {
        assert!(validate_directory_name("uploads").is_ok());
        assert!(validate_directory_name("../escape").is_err());
        assert!(validate_directory_name("x\nbye").is_err());
        assert_eq!(join_remote("/", "uploads"), "/uploads");
        assert_eq!(join_remote("/root", "uploads"), "/root/uploads");
    }

    #[test]
    fn folder_upload_uses_recursive_sftp_put() {
        let batch =
            upload_batch(Path::new("/tmp/local folder"), "/root/remote folder", true).unwrap();
        assert_eq!(
            batch,
            "put -R \"/tmp/local folder\" \"/root/remote folder\"\nbye\n"
        );
    }

    #[test]
    fn folder_download_uses_recursive_sftp_get() {
        let batch = download_batch("/remote folder", Path::new("/tmp/local folder"), true)
            .expect("recursive batch");
        assert_eq!(
            batch,
            "get -R \"/remote folder\" \"/tmp/local folder\"\nbye\n"
        );
    }

    #[test]
    fn command_errors_are_not_hidden_by_sftp_zero_exit_status() {
        assert_eq!(
            actionable_stderr("File \"/remote/private/file\" not found.\n"),
            "File \"/remote/private/file\" not found."
        );
        assert!(
            actionable_stderr(
                "Warning: Permanently added '[example.test]:22' (ED25519) to the list of known hosts.\n"
            )
            .is_empty()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recursive_get_downloads_nested_file_with_system_sftp() {
        let root = std::env::temp_dir().join(format!("vida-sftp-test-{}", Uuid::new_v4()));
        let remote = root.join("remote");
        let local = root.join("local").join("copied");
        std::fs::create_dir_all(remote.join("sub")).expect("create remote fixture");
        std::fs::create_dir_all(root.join("local")).expect("create local fixture");
        std::fs::write(remote.join("sub/file.txt"), b"nested\n").expect("write fixture");

        let batch = download_batch(&remote.to_string_lossy(), &local, true).expect("batch");
        let mut child = Command::new("sftp")
            .args(["-q", "-D", "/usr/libexec/sftp-server"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start system sftp");
        child
            .stdin
            .take()
            .expect("sftp stdin")
            .write_all(batch.as_bytes())
            .expect("write batch");
        let output = child.wait_with_output().expect("wait for sftp");

        assert!(output.status.success(), "{:?}", output);
        assert_eq!(
            std::fs::read(local.join("sub/file.txt")).expect("nested file downloaded"),
            b"nested\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
