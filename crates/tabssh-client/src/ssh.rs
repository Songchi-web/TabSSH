//! SSH transport built on `russh`.
//!
//! This is the only module that talks to the SSH library directly.  It exposes
//! a small async surface: connect + authenticate (trust on first use for host
//! keys), run a command and capture its output, open an interactive shell,
//! open an sftp session, and attach to a session channel.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use russh::client::Msg;
use russh::client::{self, Handle};
use russh::keys::{
    decode_secret_key, load_secret_key, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate,
};
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf};

use crate::{t, tf};

pub type KnownHosts = Arc<Mutex<BTreeMap<String, String>>>;

/// Notified with (host, fingerprint) when a brand new host key is accepted.
pub type OnNewKey = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// A host key that needs a ruling before the connection may proceed: either a
/// host we have never seen, or one whose key no longer matches what was saved.
/// `previous` holds the saved fingerprint when the key changed.
#[derive(Debug, Clone)]
pub struct KeyCheck {
    pub host: String,
    pub fingerprint: String,
    pub previous: Option<String>,
}

/// Asks whether to trust a host key; the future resolves to true to accept
/// (and save) it, false to refuse.  Without one, a first key is trusted on
/// first use and a changed key is always refused.
pub type AskKey = Arc<dyn Fn(KeyCheck) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Result of a one-shot remote command.
#[derive(Debug, Clone, Default)]
pub struct ExecOut {
    pub code: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// False when the server never sent an exit status — `code` is then just
    /// the default 0 and must not be read as success.
    pub exit_seen: bool,
}

impl ExecOut {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    pub fn ok(&self) -> bool {
        self.exit_seen && self.code == 0
    }
}

/// One way of proving who we are.  The client tries them in the order given,
/// so the user never has to pick one — a password, a key file, the usual keys
/// in `~/.ssh`, a running ssh-agent and finally none are attempted in turn.
#[derive(Debug, Clone)]
pub enum Auth {
    Password(String),
    Key {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// A key the user pasted in rather than pointing at a file.
    KeyData {
        data: String,
        passphrase: Option<String>,
    },
    /// The usual `~/.ssh/id_*` keys, and the ssh-agent if one is available.
    DefaultKeys,
    None,
}

#[derive(Clone)]
pub struct ConnectOpts {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Auth methods to try, in order.
    pub auths: Vec<Auth>,
    pub known: KnownHosts,
    /// Called with (host, fingerprint) when a brand new host key is accepted.
    pub on_new_key: Option<OnNewKey>,
    /// Asked before an unseen or changed host key is trusted; without it a
    /// first key is trusted on first use and a changed key is refused.
    pub ask: Option<AskKey>,
}

pub struct ToFuHandler {
    host: String,
    port: u16,
    known: KnownHosts,
    on_new_key: Option<OnNewKey>,
    ask: Option<AskKey>,
}

fn fingerprint(key: &PublicKeyOrCertificate) -> String {
    match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => {
            key.fingerprint(HashAlg::Sha256).to_string()
        }
        PublicKeyOrCertificate::Certificate(c) => {
            format!("cert:{}", c.to_openssh().unwrap_or_default())
        }
    }
}

impl client::Handler for ToFuHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fp = fingerprint(server_public_key);
        // Entries are keyed by host:port — two ports on one host are different
        // endpoints with different keys.  A legacy entry keyed by the bare host
        // (written by older builds, for whatever port) only answers when it
        // matches; a mismatch is offered as a *new* key, never as a "changed"
        // one — the entry may simply belong to another port.
        let key_id = format!("{}:{}", self.host, self.port);
        let previous = {
            let known = self.known.lock().unwrap();
            match known.get(&key_id) {
                Some(prev) => Some(prev.clone()),
                // Legacy fallback: a bare-host match is still good enough.
                None if known.get(&self.host).is_some_and(|p| *p == fp) => return Ok(true),
                None => None,
            }
        };
        let changed = match previous {
            Some(prev) if prev == fp => return Ok(true),
            other => other,
        };
        // The ruling may come from the ui and take as long as it takes, so the
        // lock is never held while waiting for it.
        let accepted = match &self.ask {
            Some(ask) => {
                ask(KeyCheck {
                    host: key_id.clone(),
                    fingerprint: fp.clone(),
                    previous: changed,
                })
                .await
            }
            // No one to ask: a first key is trusted on first use, a changed key
            // never is.
            None => changed.is_none(),
        };
        if !accepted {
            return Ok(false);
        }
        self.known
            .lock()
            .unwrap()
            .insert(key_id.clone(), fp.clone());
        if let Some(cb) = &self.on_new_key {
            cb(&key_id, &fp);
        }
        Ok(true)
    }
}

/// An authenticated connection.
#[derive(Clone)]
pub struct Ssh {
    handle: Arc<Handle<ToFuHandler>>,
}

fn default_key_paths() -> Vec<PathBuf> {
    let home = crate::config::home_dir();
    [".ssh/id_ed25519", ".ssh/id_ecdsa", ".ssh/id_rsa"]
        .iter()
        .map(|p| home.join(p))
        .filter(|p| p.exists())
        .collect()
}

impl Ssh {
    pub async fn connect(opts: ConnectOpts) -> Result<Ssh> {
        let config = Arc::new(client::Config {
            inactivity_timeout: None,
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            nodelay: true,
            ..Default::default()
        });
        let handler = ToFuHandler {
            host: opts.host.clone(),
            port: opts.port,
            known: Arc::clone(&opts.known),
            on_new_key: opts.on_new_key.clone(),
            ask: opts.ask.clone(),
        };
        let addr = (opts.host.as_str(), opts.port);
        let mut handle = client::connect(config, addr, handler)
            .await
            .with_context(|| tf!("connecting to {}:{}", opts.host, opts.port))?;

        let user = opts.user.clone();
        let mut last_err: Option<String> = None;
        let mut authenticated = false;

        // The caller hands us an ordered list; make sure the catch-alls are
        // present so a connection is never attempted with nothing to try.
        let mut attempts = opts.auths.clone();
        if !attempts.iter().any(|a| matches!(a, Auth::DefaultKeys)) {
            attempts.push(Auth::DefaultKeys);
        }
        if !attempts.iter().any(|a| matches!(a, Auth::None)) {
            attempts.push(Auth::None);
        }

        for attempt in attempts {
            let res: Result<(), String> = match attempt {
                Auth::Password(pw) => match handle.authenticate_password(&user, pw).await {
                    Ok(r) if r.success() => Ok(()),
                    Ok(_) => Err(t!("password rejected").into()),
                    Err(e) => Err(e.to_string()),
                },
                Auth::KeyData { data, passphrase } => {
                    match decode_secret_key(&data, passphrase.as_deref()) {
                        Ok(kp) => {
                            let alg = handle
                                .best_supported_rsa_hash()
                                .await
                                .ok()
                                .flatten()
                                .flatten();
                            match handle
                                .authenticate_publickey(
                                    &user,
                                    PrivateKeyWithHashAlg::new(Arc::new(kp), alg),
                                )
                                .await
                            {
                                Ok(r) if r.success() => Ok(()),
                                Ok(_) => Err(t!("public key rejected").into()),
                                Err(e) => Err(e.to_string()),
                            }
                        }
                        Err(e) => Err(tf!("cannot read the pasted key: {}", e)),
                    }
                }
                Auth::Key { path, passphrase } => {
                    match load_secret_key(&path, passphrase.as_deref()) {
                        Ok(kp) => {
                            let alg = handle
                                .best_supported_rsa_hash()
                                .await
                                .ok()
                                .flatten()
                                .flatten();
                            match handle
                                .authenticate_publickey(
                                    &user,
                                    PrivateKeyWithHashAlg::new(Arc::new(kp), alg),
                                )
                                .await
                            {
                                Ok(r) if r.success() => Ok(()),
                                Ok(_) => Err(t!("public key rejected").into()),
                                Err(e) => Err(e.to_string()),
                            }
                        }
                        Err(e) => Err(tf!("cannot load key {}: {}", path.display(), e)),
                    }
                }
                Auth::DefaultKeys => {
                    let mut done = Err(t!("no default keys found").to_string());
                    for path in default_key_paths() {
                        let Ok(kp) = load_secret_key(&path, None) else {
                            continue;
                        };
                        let alg = handle
                            .best_supported_rsa_hash()
                            .await
                            .ok()
                            .flatten()
                            .flatten();
                        match handle
                            .authenticate_publickey(
                                &user,
                                PrivateKeyWithHashAlg::new(Arc::new(kp), alg),
                            )
                            .await
                        {
                            Ok(r) if r.success() => {
                                done = Ok(());
                                break;
                            }
                            Ok(_) => done = Err(t!("public key rejected").into()),
                            Err(e) => done = Err(e.to_string()),
                        }
                    }
                    done
                }
                Auth::None => match handle.authenticate_none(&user).await {
                    Ok(r) if r.success() => Ok(()),
                    Ok(_) => Err(t!("none auth rejected").into()),
                    Err(e) => Err(e.to_string()),
                },
            };
            match res {
                Ok(()) => {
                    authenticated = true;
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }

        if !authenticated {
            bail!(
                "{}",
                tf!(
                    "authentication failed for {}@{}: {}",
                    user,
                    opts.host,
                    last_err.unwrap_or_else(|| t!("no method succeeded").into())
                )
            );
        }

        Ok(Ssh {
            handle: Arc::new(handle),
        })
    }

    /// Runs a command to completion, capturing stdout/stderr.
    pub async fn exec(&self, cmd: &str) -> Result<ExecOut> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .context(t!("opening exec channel"))?;
        channel
            .exec(true, cmd.as_bytes().to_vec())
            .await
            .with_context(|| tf!("exec: {}", cmd))?;

        let mut out = ExecOut::default();
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext: 1 } => out.stderr.extend_from_slice(&data),
                ChannelMsg::ExtendedData { .. } => {}
                ChannelMsg::ExitStatus { exit_status } => {
                    out.code = exit_status;
                    out.exit_seen = true;
                }
                ChannelMsg::Eof | ChannelMsg::Close => {}
                _ => {}
            }
        }
        Ok(out)
    }

    /// Opens a session channel and runs `cmd` in it over a pty, so a
    /// full-screen program on the far side has a real terminal.
    ///
    /// `echo` asks the far side to echo input back; a plain interactive shell
    /// needs it on (otherwise typing is invisible), while a program that paints
    /// the whole screen — `screen` attaching to a task — wants it off.
    pub async fn open_pty_exec(
        &self,
        cmd: &str,
        term: &str,
        cols: u16,
        rows: u16,
        echo: bool,
    ) -> Result<(ChannelReadHalf, ChannelWriteHalf<Msg>)> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .context(t!("opening shell channel"))?;
        channel
            .request_pty(
                true,
                term,
                cols as u32,
                rows as u32,
                0,
                0,
                &[(russh::Pty::ECHO, echo as u32)],
            )
            .await
            .context(t!("requesting pty"))?;
        channel
            .exec(true, cmd.as_bytes().to_vec())
            .await
            .with_context(|| tf!("exec stream: {}", cmd))?;
        Ok(channel.split())
    }

    /// Opens an sftp session.
    pub async fn sftp(&self) -> Result<russh_sftp::client::SftpSession> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .context(t!("opening sftp channel"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .context(t!("requesting sftp subsystem"))?;
        let stream = channel.into_stream();
        russh_sftp::client::SftpSession::new(stream)
            .await
            .map_err(|e| anyhow!("{}", tf!("sftp handshake failed: {}", e)))
    }

    pub async fn disconnect(&self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::server::{Auth as SrvAuth, ChannelOpenHandle, Msg, Server as _, Session};
    use russh::Channel;

    #[derive(Clone)]
    struct TestServer;

    impl russh::server::Server for TestServer {
        type Handler = TestHandler;
        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> TestHandler {
            TestHandler
        }
    }

    struct TestHandler;

    impl russh::server::Handler for TestHandler {
        type Error = russh::Error;

        async fn auth_none(&mut self, _user: &str) -> Result<SrvAuth, Self::Error> {
            Ok(SrvAuth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            _channel: Channel<Msg>,
            reply: ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }
    }

    /// A throwaway key generated for these tests only (ssh-keygen -t ed25519,
    /// empty comment so no machine or user name is embedded).
    const TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\nQyNTUxOQAAACD2PuauLmAHOFMQVZ4PEICEKdnIdoq7ELs5n4/QLDPYewAAAIh6CFmLeghZ\niwAAAAtzc2gtZWQyNTUxOQAAACD2PuauLmAHOFMQVZ4PEICEKdnIdoq7ELs5n4/QLDPYew\nAAAEBSxow74BMyjctcaWANrFt7gIZgb/X0mmaVUSIOs6CzPfY+5q4uYAc4UxBVng8QgIQp\n2ch2irsQuzmfj9AsM9h7AAAAAAECAwQF\n-----END OPENSSH PRIVATE KEY-----\n";

    /// Starts an in-process server and returns its port.
    async fn serve() -> u16 {
        let key = russh::keys::decode_secret_key(TEST_KEY, None).unwrap();
        let config = russh::server::Config {
            keys: vec![key],
            ..Default::default()
        };
        let mut server = TestServer;
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        tokio::spawn(async move {
            server.run_on_socket(Arc::new(config), &socket).await.ok();
        });
        port
    }

    fn opts(port: u16, known: &KnownHosts, ask: Option<AskKey>) -> ConnectOpts {
        ConnectOpts {
            host: "127.0.0.1".into(),
            port,
            user: "someone".into(),
            auths: vec![Auth::None],
            known: Arc::clone(known),
            on_new_key: None,
            ask,
        }
    }

    fn verdict(yes: bool) -> AskKey {
        Arc::new(move |_| Box::pin(async move { yes }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn first_connect_accepts_and_remembers_the_host_key() {
        let port = serve().await;
        let known: KnownHosts = Arc::new(Mutex::new(BTreeMap::new()));
        let ssh = Ssh::connect(opts(port, &known, None))
            .await
            .expect("first connect must succeed");
        let key_id = format!("127.0.0.1:{port}");
        assert!(
            known.lock().unwrap().contains_key(&key_id),
            "host key remembered under host:port"
        );
        ssh.disconnect().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_key_is_refused_when_there_is_no_one_to_ask() {
        let port = serve().await;
        let known: KnownHosts = Arc::new(Mutex::new(BTreeMap::new()));
        known.lock().unwrap().insert(
            format!("127.0.0.1:{port}"),
            "SHA256:not-the-real-key".into(),
        );
        let err = match Ssh::connect(opts(port, &known, None)).await {
            Ok(_) => panic!("a changed key must refuse"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("key"), "{err:#}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_key_is_saved_once_the_user_accepts_it() {
        let port = serve().await;
        let known: KnownHosts = Arc::new(Mutex::new(BTreeMap::new()));
        known.lock().unwrap().insert(
            format!("127.0.0.1:{port}"),
            "SHA256:not-the-real-key".into(),
        );
        let ssh = Ssh::connect(opts(port, &known, Some(verdict(true))))
            .await
            .expect("an accepted key must connect");
        let saved = known.lock().unwrap()[&format!("127.0.0.1:{port}")].clone();
        assert_ne!(saved, "SHA256:not-the-real-key", "new key saved");
        ssh.disconnect().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_key_does_not_connect_and_is_not_saved() {
        let port = serve().await;
        let known: KnownHosts = Arc::new(Mutex::new(BTreeMap::new()));
        if let Ok(_) = Ssh::connect(opts(port, &known, Some(verdict(false)))).await {
            panic!("a refused key must not connect");
        }
        assert!(known.lock().unwrap().is_empty(), "nothing saved");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_legacy_bare_host_entry_still_answers() {
        let port = serve().await;
        let known: KnownHosts = Arc::new(Mutex::new(BTreeMap::new()));
        // Learn the fingerprint, then keep it only under the old bare-host key.
        let ssh = Ssh::connect(opts(port, &known, None)).await.unwrap();
        ssh.disconnect().await;
        let fp = known.lock().unwrap().values().next().unwrap().clone();
        known.lock().unwrap().clear();
        known.lock().unwrap().insert("127.0.0.1".into(), fp);
        // No one to ask: a legacy match must not turn into a ruling at all.
        let ssh = Ssh::connect(opts(port, &known, None))
            .await
            .expect("a legacy bare-host entry must still match");
        ssh.disconnect().await;
    }
}

/// Quotes a string for safe embedding in a remote shell command.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._/=:@,+".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

