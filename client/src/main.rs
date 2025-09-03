use anyhow::Context;
use clap::Parser;
use colored::Colorize;
use russh::client::Msg;
use sk8brd::ssh::{ssh_connect, SSH_BUFFER_SIZE};
use sk8brd::{
    console_print, parse_recv_msg, print_string_msg, select_brd, send_ack, send_break,
    send_console, send_image, send_msg, todo, Sk8brdMsgs, CDBA_SERVER_BIN_NAME, MSG_HDR_SIZE,
};
use std::fs;
use std::io::{stdout, Read, Write};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::sync::Mutex;

macro_rules! get_arc {
    ($a: expr) => {{
        $a.lock().await
    }};
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short)]
    farm: String,

    #[arg(short, default_value_t = String::from("22"))]
    port: String,

    #[arg(short)]
    board: String,

    #[arg(short)]
    image_path: String,

    #[arg(short, default_value_t = String::from("cdba"))]
    user: String,

    #[arg(long, default_value_t = false)]
    power_cycle: bool,
}

async fn handle_keypress(
    c: char,
    quit: &mut Arc<Mutex<bool>>,
    special: &mut bool,
    message_sink: &mut Arc<Mutex<impl AsyncWrite + Unpin>>,
) {
    if *special {
        *special = false;
        match c {
            'a' => send_console(message_sink, &[1u8]).await.unwrap(),
            'B' => send_break(message_sink).await.unwrap(),
            'P' => send_ack(message_sink, Sk8brdMsgs::MsgPowerOn)
                .await
                .unwrap(),
            'p' => send_ack(message_sink, Sk8brdMsgs::MsgPowerOff)
                .await
                .unwrap(),
            'q' => *get_arc!(quit) = true,
            's' => (), //TODO:
            'V' => send_ack(message_sink, Sk8brdMsgs::MsgVbusOn).await.unwrap(),
            'v' => send_ack(message_sink, Sk8brdMsgs::MsgVbusOff)
                .await
                .unwrap(),
            _ => (),
        }
    } else {
        match c.try_into() {
            Ok(1u8) => *special = true, // CTRL-A, TODO: configurable?
            Ok(_) => send_console(message_sink, &[c as u8]).await.unwrap(),
            Err(_) => (),
        }
    }
}

// For raw mode TTY
#[allow(clippy::explicit_write)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut hdr_buf = [0u8; MSG_HDR_SIZE];
    let mut buf = [0u8; SSH_BUFFER_SIZE];
    let mut key_buf = [0u8; 1];
    let quit = Arc::new(Mutex::new(false));
    let args = Args::parse();

    let fastboot_image = fs::read(args.image_path).expect("boot image not found");

    println!("sk8brd {}", env!("CARGO_PKG_VERSION"));

    let chan = Arc::new(Mutex::new(
        ssh_connect(&format!("{}:{}", args.farm, args.port), args.user).await?,
    ));
    get_arc!(chan)
        .exec(true, CDBA_SERVER_BIN_NAME)
        .await
        .with_context(|| format!("Couldn't execute {CDBA_SERVER_BIN_NAME} on remote server"))?;

    let mut server_stdin = Arc::new(Mutex::new(get_arc!(chan).make_writer()));

    let (server_stdout, server_stderr) = sk8brd::ssh::into_streams::<Msg>(chan).await;
    let server_stdout = Arc::new(Mutex::new(server_stdout));
    let server_stderr = Arc::new(Mutex::new(server_stderr));

    send_ack(&mut server_stdin, Sk8brdMsgs::MsgListDevices).await?;
    select_brd(&mut server_stdin, &args.board).await?;
    if args.power_cycle {
        println!("Powering off the board first");
        send_ack(&mut server_stdin, Sk8brdMsgs::MsgPowerOff).await?;
    }

    crossterm::terminal::enable_raw_mode()?;

    let mut quit2 = Arc::clone(&quit);
    let mut server_stdin2 = Arc::clone(&server_stdin);
    let stdin_handler = tokio::spawn(async move {
        let mut stdin = os_pipe::dup_stdin().expect("Couldn't dup stdin");
        let mut ctrl_a_pressed = false;

        while !*get_arc!(quit2) {
            if let Ok(len) = stdin.read(&mut key_buf) {
                for c in key_buf[0..len].iter() {
                    handle_keypress(
                        *c as char,
                        &mut quit2,
                        &mut ctrl_a_pressed,
                        &mut server_stdin2,
                    )
                    .await;
                }
            };
        }
    });

    while !*get_arc!(quit) {
        // Stream of "blue text" - status updates from the server
        if let Ok(bytes_read) = (*get_arc!(server_stderr)).read(&mut buf).await {
            let s = String::from_utf8_lossy(&buf[..bytes_read]);
            print!(
                "{}\r",
                s.split('\n').collect::<Vec<_>>().join("\r\n").blue()
            );
            stdout().flush()?;
        }

        // Msg handler
        // Read the message header first
        if (*get_arc!(server_stdout))
            .read_exact(&mut hdr_buf)
            .await
            .is_ok()
        {
            let msg = parse_recv_msg(&hdr_buf);
            let mut msgbuf = vec![0u8; msg.len as usize];

            // Now read the actual data...
            (*get_arc!(server_stdout)).read_exact(&mut msgbuf).await?;

            // ..and process it
            match msg.r#type.try_into() {
                Ok(Sk8brdMsgs::MsgSelectBoard) => {
                    send_msg(&mut server_stdin, Sk8brdMsgs::MsgPowerOn, &[]).await?
                }
                Ok(Sk8brdMsgs::MsgConsole) => console_print(&msgbuf).await,
                Ok(Sk8brdMsgs::MsgHardReset) => todo!("MsgHardReset is unused"),
                Ok(Sk8brdMsgs::MsgPowerOn) => (),
                Ok(Sk8brdMsgs::MsgPowerOff) => (),
                Ok(Sk8brdMsgs::MsgFastbootPresent) => {
                    if !msgbuf.is_empty() && msgbuf[0] != 0 {
                        send_image(&mut server_stdin, &fastboot_image, &quit).await?
                    }
                }
                Ok(Sk8brdMsgs::MsgFastbootDownload) => (),
                Ok(Sk8brdMsgs::MsgFastbootBoot) => todo!("MsgFastbootBoot is unused"),
                Ok(Sk8brdMsgs::MsgStatusUpdate) => todo!("MsgStatusUpdate: implement me!"),
                Ok(Sk8brdMsgs::MsgVbusOn) => todo!("Unexpected MsgVbusOn"),
                Ok(Sk8brdMsgs::MsgVbusOff) => todo!("Unexpected MsgVbusOff"),
                Ok(Sk8brdMsgs::MsgFastbootReboot) => todo!("MsgFastbootReboot is unused"),
                Ok(Sk8brdMsgs::MsgSendBreak) => todo!("MsgSendBreak: implement me!"),
                Ok(Sk8brdMsgs::MsgListDevices) => print_string_msg(&msgbuf),
                Ok(Sk8brdMsgs::MsgBoardInfo) => print_string_msg(&msgbuf),
                Ok(Sk8brdMsgs::MsgFastbootContinue) => (),

                Ok(m) => todo!("{m:?} is unimplemented, skipping.."),
                Err(e) => todo!("Received unknown/invalid message: `{e}`"),
            };
        }
    }

    // No more keypresses will be useful
    stdin_handler.abort();

    // Pick up the trash
    crossterm::terminal::disable_raw_mode()?;

    // Power off the board on goodbye
    send_ack(&mut server_stdin, Sk8brdMsgs::MsgPowerOff).await?;

    // ssh_disconnect(&mut sess).await?;

    println!("\nGoodbye");
    Ok(())
}
