use anyhow::{Context as _, bail};
use asynchronous_codec::BytesMut;
use russh::Channel;
use russh::client::{self, Msg};
use russh::keys::load_secret_key;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, ssh_key};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::AsyncRead;
use tokio::sync::mpsc::Receiver;
use tokio::sync::{Mutex, mpsc};

pub const SSH_BUFFER_SIZE: usize = 2048;

struct Client {}

impl client::Handler for Client {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub async fn ssh_connect(farm: &str, username: String) -> anyhow::Result<Channel<Msg>> {
    // Connect to the local SSH server
    let config = client::Config::default();
    let client = Client {};

    let mut sess = client::connect(Arc::new(config), farm, client)
        .await
        .with_context(|| format!("Couldn't connect to {farm}"))?;

    let mut authenticated = false;

    #[cfg(unix)]
    let mut agent = russh::keys::agent::client::AgentClient::connect_env()
        .await
        .ok();
    #[cfg(windows)]
    let mut agent =
        russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
            .await
            .ok();

    if let Some(agent) = agent.as_mut()
        && let Ok(keys) = agent.request_identities().await
    {
        for key in keys {
            if sess
                .authenticate_publickey_with(
                    &username,
                    key.to_owned(),
                    Some(HashAlg::Sha256),
                    agent,
                )
                .await
                .map(|result| result.success())
                .unwrap_or(false)
            {
                authenticated = true;
                break;
            }
        }
    }

    if !authenticated {
        for path in candidate_private_keys() {
            if !path.is_file() {
                continue;
            }

            let key_pair = match load_secret_key(&path, None) {
                Ok(key_pair) => key_pair,
                Err(_) => continue,
            };

            let hash_alg = if key_pair.algorithm().is_rsa() {
                sess.best_supported_rsa_hash()
                    .await
                    .with_context(|| format!("Could not check RSA signatures for {path:?}"))?
                    .flatten()
            } else {
                None
            };
            let private_key = PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash_alg);

            if sess
                .authenticate_publickey(&username, private_key)
                .await
                .map(|result| result.success())
                .unwrap_or(false)
            {
                authenticated = true;
                break;
            }
        }
    }

    if !authenticated || sess.is_closed() {
        bail!("No key was accepted by the server");
    }

    let chan = sess
        .channel_open_session()
        .await
        .expect("Couldn't open session");

    Ok(chan)
}

fn candidate_private_keys() -> Vec<PathBuf> {
    let mut keys = Vec::new();

    if let Ok(key_path) = std::env::var("SK8BRD_SSH_KEY") {
        keys.push(expand_tilde(key_path));
    }
    if let Ok(home) = std::env::var("HOME") {
        keys.push(Path::new(&home).join(".ssh").join("id_ed25519"));
        keys.push(Path::new(&home).join(".ssh").join("id_rsa"));
    }

    keys
}

fn expand_tilde(path: String) -> PathBuf {
    let path = path.trim().to_string();
    if let Some(tail) = path.strip_prefix("~/") {
        return std::env::var("HOME")
            .ok()
            .map_or_else(PathBuf::new, |home| Path::new(&home).join(tail));
    }

    Path::new(&path).to_path_buf()
}

pub struct Wrap(Receiver<Vec<u8>>, BytesMut);

impl Wrap {
    fn new(rx: Receiver<Vec<u8>>) -> Self {
        Self(rx, BytesMut::new())
    }
}

/// Create streams for a channel's stdout and stderr, consuming the channel in the process
pub async fn into_streams<S>(chan: Arc<Mutex<Channel<S>>>) -> (Wrap, Wrap)
where
    S: From<(russh::ChannelId, russh::ChannelMsg)> + std::marker::Send + 'static + Sync,
{
    let (txo, rxo) = mpsc::channel::<Vec<u8>>(1000);
    let (txe, rxe) = mpsc::channel::<Vec<u8>>(1000);

    tokio::spawn(async move {
        loop {
            match chan.lock().await.wait().await {
                Some(russh::ChannelMsg::Data { data }) => {
                    txo.send(data[..].into())
                        .await
                        .map_err(|_| russh::Error::SendError)?;
                }
                Some(russh::ChannelMsg::ExtendedData { data, ext: 1 }) => {
                    txe.send(data[..].into())
                        .await
                        .map_err(|_| russh::Error::SendError)?;
                }
                Some(russh::ChannelMsg::ExtendedData { data: _, ext }) => {
                    println!("Received surprise data on stream {ext}");
                }
                Some(russh::ChannelMsg::Eof) => {
                    // Send a 0-length chunk to indicate EOF.
                    txo.send(vec![])
                        .await
                        .map_err(|_| russh::Error::SendError)?;
                    break;
                }
                None => break,
                _ => (),
            }
        }

        chan.lock().await.close().await?;
        Ok::<_, russh::Error>(())
    });

    (Wrap::new(rxo), Wrap::new(rxe))
}

impl AsyncRead for Wrap {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let cache_size = self.1.len();
        buf.put_slice(&self.1.split_to(usize::min(buf.remaining(), cache_size)));

        if buf.remaining() > 0 {
            match self.0.poll_recv(cx) {
                Poll::Ready(Some(msg)) => {
                    self.1 = BytesMut::from(&msg[..]);
                    let len = self.1.len();
                    buf.put_slice(&self.1.split_to(usize::min(buf.remaining(), len)));
                    Poll::Ready(Ok(()))
                }

                Poll::Ready(None) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
