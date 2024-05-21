use anyhow::Context as _;
use ssh2::{Channel, Session};
use std::net::TcpStream;
use std::sync::Arc;
use tokio::sync::Mutex;

pub const SSH_BUFFER_SIZE: usize = 2048;

pub async fn ssh_connect(farm: String, port: String, username: String) -> anyhow::Result<Session> {
    // Connect to the local SSH server
    let tcp = TcpStream::connect(format!("{}:{}", farm, port))
        .with_context(|| format!("Couldn't connect to {}:{}", farm, port))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    // Try to authenticate with the first identity in the agent.
    sess.userauth_agent(&username)
        .with_context(|| format!("Couldn't authenticate as {username}"))?;

    Ok(sess)
}

pub async fn ssh_get_chan(
    sess: &mut Session,
    server_bin_name: &str,
) -> anyhow::Result<Arc<Mutex<Channel>>> {
    let chan = Arc::new(Mutex::new(sess.channel_session()?));
    (*chan.lock().await)
        .exec(server_bin_name)
        .with_context(|| format!("Couldn't execute {server_bin_name} on remote host"))?;

    Ok(chan)
}

pub async fn ssh_disconnect(sess: &mut Session) -> anyhow::Result<()> {
    sess.disconnect(
        Option::Some(ssh2::DisconnectCode::ConnectionLost),
        "bye",
        Option::Some("C"),
    )
    .context("Couldn't disconnect cleanly")
}
