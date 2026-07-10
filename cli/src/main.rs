use anyhow::Context;
use clap::Parser;
use colored::Colorize;
use russh::client::Msg;
use sk8brd::ssh::{ssh_connect, SSH_BUFFER_SIZE};
use sk8brd::{
    console_print, parse_recv_msg, print_string_msg, select_brd, send_ack, send_image, todo,
    Sk8brdMsgs, CDBA_SERVER_BIN_NAME, MSG_HDR_SIZE,
};
use std::fs;
use std::io::{stdout, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::time::{sleep_until, timeout};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short)]
    farm: String,

    #[arg(short, default_value_t = String::from("22"))]
    port: String,

    #[arg(short, default_value_t = String::from(""))]
    board: String,

    #[arg(short)]
    image_path: String,

    #[arg(short, default_value_t = String::from("cdba"))]
    user: String,

    #[arg(short, default_value_t = false)]
    verbose: bool,

    #[arg(short, default_value_t = 60)]
    timeout: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    const MAX_MSG_LEN: usize = 1024 * 1024;
    let quit = Arc::new(Mutex::new(false));
    let mut stderr_buf = [0u8; SSH_BUFFER_SIZE];
    let mut stdout_chunk = [0u8; SSH_BUFFER_SIZE];
    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_open = true;
    let args = Args::parse();
    let mut deadline = Instant::now() + Duration::from_secs(args.timeout);
    let mut should_exit = false;

    let fastboot_image = fs::read(args.image_path).expect("boot image not found");

    println!("sk8brd-cli {}", env!("CARGO_PKG_VERSION"));

    let chan = Arc::new(Mutex::new(
        ssh_connect(&format!("{}:{}", args.farm, args.port), args.user).await?,
    ));
    (*chan.lock().await)
        .exec(true, CDBA_SERVER_BIN_NAME)
        .await
        .with_context(|| format!("Couldn't execute {CDBA_SERVER_BIN_NAME} on remote server"))?;

    let mut server_stdin = Arc::new(Mutex::new((*chan.lock().await).make_writer()));
    let (mut server_stdout, mut server_stderr) = sk8brd::ssh::into_streams::<Msg>(chan).await;

    if args.board.is_empty() {
        send_ack(&mut server_stdin, Sk8brdMsgs::MsgListDevices).await?;
    } else {
        select_brd(&mut server_stdin, &args.board).await?;
    }

    // Msg handler
    while Instant::now() < deadline {
        tokio::select! {
            _ = sleep_until(tokio::time::Instant::from_std(deadline)) => break,

            // Stream of "blue text" - status updates from the server
            stderr_read = server_stderr.read(&mut stderr_buf), if stderr_open => {
                if let Ok(bytes_read) = stderr_read {
                    if bytes_read == 0 {
                        stderr_open = false;
                        continue;
                    }

                    let s = String::from_utf8_lossy(&stderr_buf[..bytes_read]);
                    print!(
                        "{}\r",
                        s.split('\n').collect::<Vec<_>>().join("\r\n").blue()
                    );
                    stdout().flush()?;
                }
            }

            // Binary protocol stream on stdout
            stdout_read = server_stdout.read(&mut stdout_chunk) => {
                if let Ok(bytes_read) = stdout_read {
                    if bytes_read == 0 {
                        break;
                    }

                    stdout_buf.extend_from_slice(&stdout_chunk[..bytes_read]);

                    // Parse as many complete framed messages as available.
                    loop {
                        if stdout_buf.len() < MSG_HDR_SIZE {
                            break;
                        }

                        let msg = parse_recv_msg(&stdout_buf[..MSG_HDR_SIZE]);
                        if Sk8brdMsgs::try_from(msg.r#type).is_err() || msg.len as usize > MAX_MSG_LEN {
                            // Resync in case stdout had unexpected text/noise.
                            stdout_buf.drain(..1);
                            continue;
                        }

                        let total_len = MSG_HDR_SIZE + msg.len as usize;
                        if stdout_buf.len() < total_len {
                            break;
                        }

                        let msgbuf = stdout_buf[MSG_HDR_SIZE..total_len].to_vec();
                        stdout_buf.drain(..total_len);
                        match msg.r#type.try_into() {
                            Ok(Sk8brdMsgs::MsgSelectBoard) => {
                                send_ack(&mut server_stdin, Sk8brdMsgs::MsgPowerOn).await?
                            }
                            Ok(Sk8brdMsgs::MsgConsole) => {
                                if args.verbose {
                                    console_print(&msgbuf).await
                                }
                            }
                            Ok(Sk8brdMsgs::MsgPowerOn) => {
                                // Refresh timeout window after power-on ack.
                                deadline = Instant::now() + Duration::from_secs(args.timeout);
                            }
                            Ok(Sk8brdMsgs::MsgFastbootPresent) => {
                                if !msgbuf.is_empty() && msgbuf[0] != 0 {
                                    send_image(&mut server_stdin, &fastboot_image, &quit).await?
                                }
                            }
                            Ok(Sk8brdMsgs::MsgFastbootDownload) => (),
                            Ok(Sk8brdMsgs::MsgListDevices) => {
                                print_string_msg(&msgbuf);
                                if msgbuf.is_empty() {
                                    should_exit = true;
                                    break;
                                }
                            }

                            // Ignore all other valid messages
                            Ok(_) => (),
                            Err(e) => todo!("Received unknown/invalid message: `{e}`"),
                        };
                    }
                    if should_exit {
                        break;
                    }
                }
            }
        }
    }

    // Power off the board on goodbye
    let _ = timeout(
        Duration::from_secs(1),
        send_ack(&mut server_stdin, Sk8brdMsgs::MsgPowerOff),
    )
    .await;

    // ssh_disconnect(&mut sess).await?;

    println!("\nGoodbye");
    Ok(())
}
