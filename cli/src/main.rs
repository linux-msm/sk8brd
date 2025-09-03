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
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

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
    let quit = Arc::new(Mutex::new(false));
    let mut buf = [0u8; SSH_BUFFER_SIZE];
    let mut time: SystemTime = SystemTime::now();
    let mut hdr_buf = [0u8; MSG_HDR_SIZE];
    let args = Args::parse();

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
    let (server_stdout, server_stderr) = sk8brd::ssh::into_streams::<Msg>(chan).await;
    let server_stdout = Arc::new(Mutex::new(server_stdout));
    let server_stderr = Arc::new(Mutex::new(server_stderr));

    if args.board.is_empty() {
        send_ack(&mut server_stdin, Sk8brdMsgs::MsgListDevices).await?;
    } else {
        select_brd(&mut server_stdin, &args.board).await?;
    }

    // Msg handler
    // Read the message header first
    while time.elapsed()? < Duration::from_secs(args.timeout) {
        // Stream of "blue text" - status updates from the server
        if let Ok(bytes_read) = (*server_stderr.lock().await).read(&mut buf).await {
            let s = String::from_utf8_lossy(&buf[..bytes_read]);
            print!(
                "{}\r",
                s.split('\n').collect::<Vec<_>>().join("\r\n").blue()
            );
            stdout().flush()?;
        }

        if (*server_stdout.lock().await)
            .read_exact(&mut hdr_buf)
            .await
            .is_ok()
        {
            let msg = parse_recv_msg(&hdr_buf);
            let mut msgbuf = vec![0u8; msg.len as usize];

            // Now read the actual data...
            (*server_stderr.lock().await)
                .read_exact(&mut msgbuf)
                .await?;

            // ..and process it
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
                    // Refresh the timer so that the timeout actually makes sense
                    time = SystemTime::now();
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
                        break;
                    }
                }

                // Ignore all other valid messages
                Ok(_) => (),
                Err(e) => todo!("Received unknown/invalid message: `{e}`"),
            };
        }
    }

    // Power off the board on goodbye
    send_ack(&mut server_stdin, Sk8brdMsgs::MsgPowerOff).await?;

    // ssh_disconnect(&mut sess).await?;

    println!("\nGoodbye");
    Ok(())
}
