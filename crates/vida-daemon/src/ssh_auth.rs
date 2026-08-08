//! Local-only credential broker for the system OpenSSH client's askpass flow.
//! Secrets travel over a one-shot loopback connection and are never placed in
//! argv, environment variables, or logs.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use secrecy::{ExposeSecret, SecretString};

const ENV_ADDR: &str = "VIDA_SSH_ASKPASS_ADDR";
const ENV_TOKEN: &str = "VIDA_SSH_ASKPASS_TOKEN";

pub struct AskpassBroker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub struct AskpassEnv {
    pub address: String,
    pub token: String,
}

impl AskpassBroker {
    pub fn start(secret: SecretString) -> Result<(Self, AskpassEnv)> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("无法建立本机 SSH 凭据通道")?;
        listener
            .set_nonblocking(true)
            .context("无法配置本机 SSH 凭据通道")?;
        let address = listener.local_addr()?.to_string();
        let token = uuid::Uuid::new_v4().to_string();
        let thread_token = token.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            while !thread_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut supplied = Vec::new();
                        if stream.read_to_end(&mut supplied).is_ok()
                            && supplied == thread_token.as_bytes()
                        {
                            let bytes = secret.expose_secret().as_bytes();
                            let _ = stream.write_all(&(bytes.len() as u32).to_be_bytes());
                            let _ = stream.write_all(bytes);
                            let _ = stream.flush();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(15));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok((
            Self {
                stop,
                thread: Some(handle),
            },
            AskpassEnv { address, token },
        ))
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AskpassBroker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run the daemon binary in OpenSSH askpass helper mode when the broker
/// variables are present. Returns false during a normal daemon launch.
pub fn run_helper_if_requested() -> Result<bool> {
    let Ok(address) = std::env::var(ENV_ADDR) else {
        return Ok(false);
    };
    let token = std::env::var(ENV_TOKEN).context("SSH askpass token missing")?;
    let mut stream = TcpStream::connect(address).context("无法连接 SSH 凭据通道")?;
    stream.write_all(token.as_bytes())?;
    stream.shutdown(Shutdown::Write)?;
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let mut secret = vec![0_u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut secret)?;
    std::io::stdout().write_all(&secret)?;
    std::io::stdout().write_all(b"\n")?;
    std::io::stdout().flush()?;
    Ok(true)
}

pub fn apply_env(command: &mut portable_pty::CommandBuilder, env: &AskpassEnv) -> Result<()> {
    let executable = std::env::current_exe().context("无法定位 vida-daemon askpass helper")?;
    command.env("SSH_ASKPASS", executable);
    command.env("SSH_ASKPASS_REQUIRE", "force");
    command.env("DISPLAY", "vida:0");
    command.env(ENV_ADDR, &env.address);
    command.env(ENV_TOKEN, &env.token);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_rejects_wrong_token_and_serves_secret() {
        let (mut broker, env) =
            AskpassBroker::start(SecretString::from("s3cret".to_string())).unwrap();

        let mut wrong = TcpStream::connect(&env.address).unwrap();
        wrong.write_all(b"wrong").unwrap();
        wrong.shutdown(Shutdown::Write).unwrap();
        let mut rejected = Vec::new();
        wrong.read_to_end(&mut rejected).unwrap();
        assert!(rejected.is_empty());

        let mut valid = TcpStream::connect(&env.address).unwrap();
        valid.write_all(env.token.as_bytes()).unwrap();
        valid.shutdown(Shutdown::Write).unwrap();
        let mut length = [0_u8; 4];
        valid.read_exact(&mut length).unwrap();
        let mut secret = vec![0_u8; u32::from_be_bytes(length) as usize];
        valid.read_exact(&mut secret).unwrap();
        assert_eq!(secret, b"s3cret");
        broker.stop();
    }
}
