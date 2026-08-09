//! Detached PTY session host.
//!
//! The production daemon is a replaceable control plane. This process owns
//! `PtyEngine`, SSH askpass brokers, terminal grids and child processes, and
//! exposes the existing operations over a 0600 Unix socket. Requests may carry
//! credentials, so they are never logged and never passed in argv or env.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use secrecy::{ExposeSecret, SecretString};
    use serde::{Deserialize, Serialize};
    use zeroize::{Zeroize, Zeroizing};

    use crate::pty::push::{BoundedReceiver, PushKind, PushPayload, bounded_channel};
    use crate::pty::{PtyEngine, ScreenData, ScreenStyled, SessionInfo, SshAuth};

    const HOST_ARG: &str = "--vida-session-host";
    const PROTOCOL_VERSION: u32 = 1;
    const MAX_CONTROL_MESSAGE: usize = 64 * 1024 * 1024;
    const PUSH_MAGIC: u8 = 0x56;

    enum HostProbe {
        Compatible(UnixStream),
        Unavailable,
    }

    #[derive(Serialize, Deserialize, Zeroize)]
    #[zeroize(drop)]
    enum HostAuth {
        Password(String),
        KeyFile {
            path: String,
            passphrase: Option<String>,
        },
        InlineKey {
            private_key: String,
            passphrase: Option<String>,
        },
    }

    #[derive(Serialize, Deserialize)]
    enum HostRequest {
        Ping,
        OpenLocal {
            cols: u16,
            rows: u16,
        },
        OpenSsh {
            cols: u16,
            rows: u16,
            host: String,
            user: String,
            port: u16,
            auth: HostAuth,
        },
        Input {
            session_id: String,
            data: Vec<u8>,
        },
        Paste {
            session_id: String,
            data: Vec<u8>,
        },
        Resize {
            session_id: String,
            cols: u16,
            rows: u16,
        },
        Scroll {
            session_id: String,
            lines: i32,
        },
        Close {
            session_id: String,
        },
        List,
        ReadScreen {
            session_id: String,
        },
        ReadScreenStyled {
            session_id: String,
        },
        Subscribe {
            session_id: String,
        },
    }

    #[derive(Serialize, Deserialize)]
    struct HostResponse {
        ok: bool,
        value: serde_json::Value,
        error: Option<String>,
    }

    impl HostResponse {
        fn success(value: serde_json::Value) -> Self {
            Self {
                ok: true,
                value,
                error: None,
            }
        }

        fn failure(error: impl std::fmt::Display) -> Self {
            Self {
                ok: false,
                value: serde_json::Value::Null,
                error: Some(error.to_string()),
            }
        }
    }

    pub struct SessionHostClient {
        socket_path: PathBuf,
        control: Mutex<Option<UnixStream>>,
    }

    impl SessionHostClient {
        pub fn connect_or_spawn() -> Result<Self> {
            let socket_path = vida_core::config::config_dir()?.join("session-host.sock");
            let client = Self {
                socket_path,
                control: Mutex::new(None),
            };
            match client.probe()? {
                HostProbe::Compatible(stream) => {
                    *client
                        .control
                        .lock()
                        .map_err(|_| anyhow::anyhow!("会话宿主控制通道锁异常"))? = Some(stream);
                    return Ok(client);
                }
                HostProbe::Unavailable => {}
            }

            if client.socket_path.exists() {
                fs::remove_file(&client.socket_path).context("无法移除失效的会话宿主 socket")?;
            }
            let executable = std::env::current_exe().context("无法定位 vida-daemon 可执行文件")?;
            let mut command = Command::new(executable);
            command
                .arg(HOST_ARG)
                .arg(&client.socket_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: this callback only invokes the async-signal-safe setsid
            // syscall before exec. A new session keeps the host alive when the
            // daemon's process group receives Ctrl-C.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            command.spawn().context("无法启动终端会话宿主进程")?;

            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if client.ping().is_ok() {
                    return Ok(client);
                }
                thread::sleep(Duration::from_millis(25));
            }
            anyhow::bail!("终端会话宿主启动超时，请重新启动 vida-daemon")
        }

        fn ping(&self) -> Result<()> {
            let value = self.call(HostRequest::Ping)?;
            let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
            if version != u64::from(PROTOCOL_VERSION) {
                anyhow::bail!("会话宿主协议版本不兼容，请关闭现有 Vida 会话后重试");
            }
            Ok(())
        }

        fn probe(&self) -> Result<HostProbe> {
            match UnixStream::connect(&self.socket_path) {
                Ok(mut stream) => {
                    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
                    write_json(&mut stream, &HostRequest::Ping)?;
                    let response: HostResponse = read_json(&mut stream)
                        .context("现有会话宿主协议无法识别，请先关闭仍在运行的 Vida 会话")?;
                    if !response.ok {
                        anyhow::bail!(response.error.unwrap_or_else(|| {
                            "现有会话宿主拒绝连接，请先关闭仍在运行的 Vida 会话".into()
                        }));
                    }
                    let version = response
                        .value
                        .get("version")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    if version != u64::from(PROTOCOL_VERSION) {
                        anyhow::bail!("会话宿主协议版本不兼容，请关闭现有 Vida 会话后重试");
                    }
                    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
                    Ok(HostProbe::Compatible(stream))
                }
                Err(_) => Ok(HostProbe::Unavailable),
            }
        }

        fn call(&self, request: HostRequest) -> Result<serde_json::Value> {
            let mut control = self
                .control
                .lock()
                .map_err(|_| anyhow::anyhow!("会话宿主控制通道锁异常"))?;
            if control.is_none() {
                let stream = UnixStream::connect(&self.socket_path)
                    .context("无法连接终端会话宿主；会话可能已意外结束")?;
                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                stream.set_write_timeout(Some(Duration::from_secs(10)))?;
                *control = Some(stream);
            }
            let stream = control.as_mut().expect("control stream initialized");
            if let Err(error) = write_json(stream, &request) {
                *control = None;
                return Err(error).context("无法向终端会话宿主发送请求");
            }
            let response: HostResponse = match read_json(stream) {
                Ok(response) => response,
                Err(error) => {
                    *control = None;
                    return Err(error).context("终端会话宿主未返回完整响应");
                }
            };
            if response.ok {
                Ok(response.value)
            } else {
                anyhow::bail!(
                    response
                        .error
                        .unwrap_or_else(|| "会话宿主返回未知错误".into())
                )
            }
        }

        pub fn open_session(&self, cols: u16, rows: u16) -> Result<String> {
            value_string(
                self.call(HostRequest::OpenLocal { cols, rows })?,
                "session_id",
            )
        }

        pub fn open_ssh_session(
            &self,
            cols: u16,
            rows: u16,
            host: &str,
            user: &str,
            port: u16,
            auth: SshAuth,
        ) -> Result<String> {
            let auth = match auth {
                SshAuth::Password(secret) => HostAuth::Password(secret.expose_secret().to_owned()),
                SshAuth::KeyFile { path, passphrase } => HostAuth::KeyFile {
                    path,
                    passphrase: passphrase.map(|value| value.expose_secret().to_owned()),
                },
                SshAuth::InlineKey {
                    private_key,
                    passphrase,
                } => HostAuth::InlineKey {
                    private_key: private_key.expose_secret().to_owned(),
                    passphrase: passphrase.map(|value| value.expose_secret().to_owned()),
                },
            };
            value_string(
                self.call(HostRequest::OpenSsh {
                    cols,
                    rows,
                    host: host.to_string(),
                    user: user.to_string(),
                    port,
                    auth,
                })?,
                "session_id",
            )
        }

        pub fn session_input(&self, session_id: &str, data: &[u8]) -> Result<()> {
            self.call(HostRequest::Input {
                session_id: session_id.into(),
                data: data.to_vec(),
            })?;
            Ok(())
        }

        pub fn paste_session(&self, session_id: &str, data: &[u8]) -> Result<()> {
            self.call(HostRequest::Paste {
                session_id: session_id.into(),
                data: data.to_vec(),
            })?;
            Ok(())
        }

        pub fn resize_session(&self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
            self.call(HostRequest::Resize {
                session_id: session_id.into(),
                cols,
                rows,
            })?;
            Ok(())
        }

        pub fn scroll_session(&self, session_id: &str, lines: i32) -> Result<usize> {
            let value = self.call(HostRequest::Scroll {
                session_id: session_id.into(),
                lines,
            })?;
            Ok(value
                .get("display_offset")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize)
        }

        pub fn close_session(&self, session_id: &str) -> Result<()> {
            self.call(HostRequest::Close {
                session_id: session_id.into(),
            })?;
            Ok(())
        }

        pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
            Ok(serde_json::from_value(self.call(HostRequest::List)?)?)
        }

        pub fn read_screen(&self, session_id: &str) -> Result<ScreenData> {
            Ok(serde_json::from_value(self.call(
                HostRequest::ReadScreen {
                    session_id: session_id.into(),
                },
            )?)?)
        }

        pub fn read_screen_styled(&self, session_id: &str) -> Result<ScreenStyled> {
            Ok(serde_json::from_value(self.call(
                HostRequest::ReadScreenStyled {
                    session_id: session_id.into(),
                },
            )?)?)
        }

        pub fn subscribe_session(&self, session_id: &str) -> Result<BoundedReceiver<PushPayload>> {
            let mut stream =
                UnixStream::connect(&self.socket_path).context("无法连接终端会话宿主")?;
            write_json(
                &mut stream,
                &HostRequest::Subscribe {
                    session_id: session_id.into(),
                },
            )?;
            let response: HostResponse = read_json(&mut stream)?;
            if !response.ok {
                anyhow::bail!(response.error.unwrap_or_else(|| "订阅会话失败".into()));
            }
            let (tx, rx) = bounded_channel();
            thread::spawn(move || {
                while let Ok(payload) = read_push(&mut stream) {
                    if !tx.send_drop_oldest(payload) {
                        break;
                    }
                }
            });
            Ok(rx)
        }

        pub fn unsubscribe_session(&self, _session_id: &str, rx: BoundedReceiver<PushPayload>) {
            drop(rx);
        }
    }

    pub fn run_if_requested() -> Result<bool> {
        let mut args = std::env::args_os();
        let _executable = args.next();
        if args.next().as_deref() != Some(std::ffi::OsStr::new(HOST_ARG)) {
            return Ok(false);
        }
        let socket_path = args
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("会话宿主缺少 socket 路径"))?;
        run_host(&socket_path)?;
        Ok(true)
    }

    fn run_host(socket_path: &Path) -> Result<()> {
        let directory = socket_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("会话宿主 socket 路径无父目录"))?;
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        if socket_path.exists() {
            fs::remove_file(socket_path)?;
        }
        let listener = UnixListener::bind(socket_path).context("无法创建会话宿主 socket")?;
        listener.set_nonblocking(true)?;
        fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
        crate::pty::cleanup_stale_inline_keys()?;
        let engine = Arc::new(RwLock::new(PtyEngine::default()));
        let connections = Arc::new(AtomicUsize::new(0));
        let original_parent = unsafe { libc::getppid() };
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    connections.fetch_add(1, Ordering::Relaxed);
                    let engine = Arc::clone(&engine);
                    let connections = Arc::clone(&connections);
                    thread::spawn(move || {
                        let _ = handle_connection(stream, engine);
                        connections.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let parent_changed = unsafe { libc::getppid() } != original_parent;
                    let no_connections = connections.load(Ordering::Relaxed) == 0;
                    let no_sessions = engine
                        .read()
                        .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                        .list_sessions()
                        .is_empty();
                    if parent_changed && no_connections && no_sessions {
                        break;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error).context("会话宿主接受连接失败"),
            }
        }
        let _ = fs::remove_file(socket_path);
        Ok(())
    }

    fn handle_connection(mut stream: UnixStream, engine: Arc<RwLock<PtyEngine>>) -> Result<()> {
        loop {
            let request: HostRequest = match read_json(&mut stream) {
                Ok(request) => request,
                Err(error) if is_connection_end(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            if let HostRequest::Subscribe { session_id } = request {
                let rx = match engine
                    .read()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .subscribe_session(&session_id)
                {
                    Ok(rx) => rx,
                    Err(error) => {
                        write_json(&mut stream, &HostResponse::failure(error))?;
                        return Ok(());
                    }
                };
                write_json(
                    &mut stream,
                    &HostResponse::success(serde_json::json!({"ok": true})),
                )?;
                while let Some(payload) = rx.recv() {
                    if write_push(&mut stream, &payload).is_err() {
                        break;
                    }
                }
                return Ok(());
            }

            let response = match dispatch(request, &engine) {
                Ok(value) => HostResponse::success(value),
                Err(error) => HostResponse::failure(error),
            };
            write_json(&mut stream, &response)?;
        }
    }

    fn is_connection_end(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                )
            })
        })
    }

    fn dispatch(
        request: HostRequest,
        engine: &Arc<RwLock<PtyEngine>>,
    ) -> Result<serde_json::Value> {
        match request {
            HostRequest::Ping => Ok(serde_json::json!({"version": PROTOCOL_VERSION})),
            HostRequest::OpenLocal { cols, rows } => {
                let id = engine
                    .write()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .open_session(cols, rows)?;
                Ok(serde_json::json!({"session_id": id}))
            }
            HostRequest::OpenSsh {
                cols,
                rows,
                host,
                user,
                port,
                auth,
            } => {
                let auth = match &auth {
                    HostAuth::Password(value) => {
                        SshAuth::Password(SecretString::from(value.to_owned()))
                    }
                    HostAuth::KeyFile { path, passphrase } => SshAuth::KeyFile {
                        path: path.to_owned(),
                        passphrase: passphrase
                            .as_ref()
                            .map(|value| SecretString::from(value.to_owned())),
                    },
                    HostAuth::InlineKey {
                        private_key,
                        passphrase,
                    } => SshAuth::InlineKey {
                        private_key: SecretString::from(private_key.to_owned()),
                        passphrase: passphrase
                            .as_ref()
                            .map(|value| SecretString::from(value.to_owned())),
                    },
                };
                let id = engine
                    .write()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .open_ssh_session(cols, rows, &host, &user, port, auth)?;
                Ok(serde_json::json!({"session_id": id}))
            }
            HostRequest::Input { session_id, data } => {
                engine
                    .read()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .session_input(&session_id, &data)?;
                Ok(serde_json::json!({"ok": true}))
            }
            HostRequest::Paste { session_id, data } => {
                engine
                    .read()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .paste_session(&session_id, &data)?;
                Ok(serde_json::json!({"ok": true}))
            }
            HostRequest::Resize {
                session_id,
                cols,
                rows,
            } => {
                engine
                    .read()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .resize_session(&session_id, cols, rows)?;
                Ok(serde_json::json!({"ok": true}))
            }
            HostRequest::Scroll { session_id, lines } => {
                let offset = engine
                    .read()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .scroll_session(&session_id, lines)?;
                Ok(serde_json::json!({"display_offset": offset}))
            }
            HostRequest::Close { session_id } => {
                engine
                    .write()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .close_session(&session_id)?;
                Ok(serde_json::json!({"ok": true}))
            }
            HostRequest::List => Ok(serde_json::to_value(
                engine
                    .read()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .list_sessions(),
            )?),
            HostRequest::ReadScreen { session_id } => Ok(serde_json::to_value(
                engine
                    .read()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .read_screen(&session_id)?,
            )?),
            HostRequest::ReadScreenStyled { session_id } => Ok(serde_json::to_value(
                engine
                    .read()
                    .map_err(|_| anyhow::anyhow!("PTY 锁异常"))?
                    .read_screen_styled(&session_id)?,
            )?),
            HostRequest::Subscribe { .. } => unreachable!(),
        }
    }

    fn value_string(value: serde_json::Value, key: &str) -> Result<String> {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("会话宿主响应缺少 {}", key))
    }

    fn write_json<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
        let bytes = Zeroizing::new(serde_json::to_vec(value)?);
        if bytes.len() > MAX_CONTROL_MESSAGE {
            anyhow::bail!("会话宿主请求过大")
        }
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&bytes)?;
        stream.flush()?;
        Ok(())
    }

    fn read_json<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T> {
        let mut length = [0u8; 4];
        stream.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_CONTROL_MESSAGE {
            anyhow::bail!("会话宿主消息过大")
        }
        let mut bytes = Zeroizing::new(vec![0u8; length]);
        stream.read_exact(&mut bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn write_push(stream: &mut UnixStream, payload: &PushPayload) -> Result<()> {
        stream.write_all(&[PUSH_MAGIC])?;
        stream.write_all(&payload.frame_seq.to_be_bytes())?;
        match payload.kind {
            PushKind::Frame => stream.write_all(&[0, 0, 0, 0, 0])?,
            PushKind::SessionClosed { exit_code } => {
                stream.write_all(&[1])?;
                stream.write_all(&exit_code.to_be_bytes())?;
            }
        }
        stream.write_all(&(payload.bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&payload.bytes)?;
        stream.flush()?;
        Ok(())
    }

    fn read_push(stream: &mut UnixStream) -> Result<PushPayload> {
        let mut header = [0u8; 14];
        stream.read_exact(&mut header)?;
        if header[0] != PUSH_MAGIC {
            anyhow::bail!("会话宿主推送帧头无效")
        }
        let frame_seq = u64::from_be_bytes(header[1..9].try_into().unwrap());
        let exit_code = u32::from_be_bytes(header[10..14].try_into().unwrap());
        let kind = match header[9] {
            0 => PushKind::Frame,
            1 => PushKind::SessionClosed { exit_code },
            _ => anyhow::bail!("会话宿主推送类型无效"),
        };
        let mut length = [0u8; 4];
        stream.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_CONTROL_MESSAGE {
            anyhow::bail!("会话宿主推送过大")
        }
        let mut bytes = vec![0u8; length];
        stream.read_exact(&mut bytes)?;
        Ok(PushPayload {
            frame_seq,
            kind,
            bytes,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn client(socket_path: &Path) -> SessionHostClient {
            SessionHostClient {
                socket_path: socket_path.to_path_buf(),
                control: Mutex::new(None),
            }
        }

        fn wait_for_text(client: &SessionHostClient, session_id: &str, needle: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let text = client
                    .read_screen(session_id)
                    .map(|screen| screen.lines.join("\n"))
                    .unwrap_or_default();
                if text.contains(needle) {
                    return text;
                }
                assert!(
                    Instant::now() < deadline,
                    "terminal never contained {needle:?}: {text}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn sessions_survive_control_client_replacement() {
            let directory = tempfile::tempdir().unwrap();
            let socket_path = directory.path().join("session-host.sock");
            let host_path = socket_path.clone();
            thread::spawn(move || run_host(&host_path).unwrap());

            let first = client(&socket_path);
            let deadline = Instant::now() + Duration::from_secs(3);
            while first.ping().is_err() {
                assert!(Instant::now() < deadline, "session host did not start");
                thread::sleep(Duration::from_millis(20));
            }

            let session_id = first.open_session(100, 30).unwrap();
            first
                .session_input(
                    &session_id,
                    b"export VIDA_M4_MARK=session-host-ok; cd /tmp; echo first-ready\r",
                )
                .unwrap();
            wait_for_text(&first, &session_id, "first-ready");
            drop(first);

            // A replacement daemon connects through a new control client. The
            // shell process, environment and working directory remain intact.
            let second = client(&socket_path);
            second.ping().unwrap();
            let sessions = second.list_sessions().unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, session_id);
            second
                .session_input(
                    &session_id,
                    b"printf '%s:%s\\n' \"$VIDA_M4_MARK\" \"$PWD\"\r",
                )
                .unwrap();
            let text = wait_for_text(&second, &session_id, "session-host-ok:/tmp");
            assert!(text.contains("first-ready"));
            second.close_session(&session_id).unwrap();
        }

        #[test]
        fn push_codec_preserves_frames_and_exit_codes() {
            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            let payload = PushPayload {
                frame_seq: 42,
                kind: PushKind::SessionClosed { exit_code: 255 },
                bytes: b"final frame".to_vec(),
            };
            write_push(&mut writer, &payload).unwrap();
            let decoded = read_push(&mut reader).unwrap();
            assert_eq!(decoded.frame_seq, 42);
            assert!(matches!(
                decoded.kind,
                PushKind::SessionClosed { exit_code: 255 }
            ));
            assert_eq!(decoded.bytes, b"final frame");
        }

        #[test]
        fn incompatible_live_host_is_not_treated_as_a_stale_socket() {
            let directory = tempfile::tempdir().unwrap();
            let socket_path = directory.path().join("session-host.sock");
            let listener = UnixListener::bind(&socket_path).unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _: HostRequest = read_json(&mut stream).unwrap();
                write_json(
                    &mut stream,
                    &HostResponse::success(serde_json::json!({"version": 999})),
                )
                .unwrap();
            });

            let result = client(&socket_path).probe();
            assert!(result.is_err());
            assert!(socket_path.exists(), "live host socket must be preserved");
            server.join().unwrap();
        }
    }
}

#[cfg(unix)]
pub use unix::{SessionHostClient, run_if_requested};

#[cfg(not(unix))]
pub struct SessionHostClient;

#[cfg(not(unix))]
impl SessionHostClient {
    fn unsupported<T>() -> anyhow::Result<T> {
        anyhow::bail!("当前平台尚不支持持久终端会话宿主，请使用 Unix 平台")
    }

    pub fn connect_or_spawn() -> anyhow::Result<Self> {
        Self::unsupported()
    }

    pub fn open_session(&self, _cols: u16, _rows: u16) -> anyhow::Result<String> {
        Self::unsupported()
    }

    pub fn open_ssh_session(
        &self,
        _cols: u16,
        _rows: u16,
        _host: &str,
        _user: &str,
        _port: u16,
        _auth: crate::pty::SshAuth,
    ) -> anyhow::Result<String> {
        Self::unsupported()
    }

    pub fn session_input(&self, _session_id: &str, _data: &[u8]) -> anyhow::Result<()> {
        Self::unsupported()
    }

    pub fn paste_session(&self, _session_id: &str, _data: &[u8]) -> anyhow::Result<()> {
        Self::unsupported()
    }

    pub fn resize_session(&self, _session_id: &str, _cols: u16, _rows: u16) -> anyhow::Result<()> {
        Self::unsupported()
    }

    pub fn scroll_session(&self, _session_id: &str, _lines: i32) -> anyhow::Result<usize> {
        Self::unsupported()
    }

    pub fn close_session(&self, _session_id: &str) -> anyhow::Result<()> {
        Self::unsupported()
    }

    pub fn list_sessions(&self) -> anyhow::Result<Vec<crate::pty::SessionInfo>> {
        Self::unsupported()
    }

    pub fn read_screen(&self, _session_id: &str) -> anyhow::Result<crate::pty::ScreenData> {
        Self::unsupported()
    }

    pub fn read_screen_styled(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<crate::pty::ScreenStyled> {
        Self::unsupported()
    }

    pub fn subscribe_session(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<crate::pty::push::BoundedReceiver<crate::pty::push::PushPayload>> {
        Self::unsupported()
    }

    pub fn unsubscribe_session(
        &self,
        _session_id: &str,
        rx: crate::pty::push::BoundedReceiver<crate::pty::push::PushPayload>,
    ) {
        drop(rx);
    }
}

#[cfg(not(unix))]
pub fn run_if_requested() -> anyhow::Result<bool> {
    Ok(false)
}
