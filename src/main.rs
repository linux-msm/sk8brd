use clap::Parser;
use colored::Colorize;
use sk8brd::{
    console_print, parse_recv_msg, print_string_msg, select_brd, send_ack, send_image, send_msg,
    Sk8brdMsgs, MSG_HDR_SIZE,
};
use ssh2::Session;
use std::fs;
use std::io::{stdout, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use tokio::sync::Mutex;

const SSH_BUFFER_SIZE: usize = 2048;
const CDBA_SERVER_BIN_NAME: &str = "cdba-server";

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

async fn handle_keypress(c: char, quit: &mut Arc<Mutex<bool>>) {
    if true {
        match c {
            'q' => *quit.lock().await = true,
            'Q' => *quit.lock().await = true,
            _ => (),
        }
    }
}

macro_rules! todo {
    ($s: expr) => {{
        let val = format!($s);
        writeln!(stdout(), "{val}\r").unwrap();
    }};
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

    // Connect to the local SSH server
    let tcp = TcpStream::connect(format!("{}:{}", args.farm, args.port))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    // Try to authenticate with the first identity in the agent.
    sess.userauth_agent("cdba")?;

    let mut chan = sess.channel_session()?;
    chan.exec(CDBA_SERVER_BIN_NAME)?;

    sess.set_blocking(false);

    send_ack(&mut chan, Sk8brdMsgs::MsgListDevices)?;
    select_brd(&mut chan, &args.board)?;
    if args.power_cycle {
        println!("Powering off the board first");
        send_ack(&mut chan, Sk8brdMsgs::MsgPowerOff)?;
    }

    crossterm::terminal::enable_raw_mode()?;

    let mut quit2 = Arc::clone(&quit);
    let stdin_handler = tokio::spawn(async move {
        let mut stdin = os_pipe::dup_stdin().expect("Couldn't dup stdin");

        while !*quit2.lock().await {
            if let Ok(len) = stdin.read(&mut key_buf) {
                for c in key_buf[0..len].iter() {
                    handle_keypress(*c as char, &mut quit2).await;
                }
            };
        }
    });

    while !*quit.lock().await {
        // Stream of "blue text" - status updates from the server
        if let Ok(bytes_read) = chan.stderr().read(&mut buf) {
            let s = String::from_utf8_lossy(&buf[..bytes_read]);
            writeln!(stdout(), "{}\r", s.blue())?;
            stdout().flush()?;
        }

        // Msg handler
        // Read the message header first
        if chan.read_exact(&mut hdr_buf).is_ok() {
            sess.set_blocking(true);
            let msg = parse_recv_msg(&hdr_buf);
            let mut msgbuf = vec![0u8; msg.len as usize];

            // Now read the actual data...
            chan.read_exact(&mut msgbuf)?;

            // ..and process it
            match msg.r#type.try_into() {
                Ok(Sk8brdMsgs::MsgSelectBoard) => send_msg(&mut chan, Sk8brdMsgs::MsgPowerOn, &[])?,
                Ok(Sk8brdMsgs::MsgConsole) => console_print(&msgbuf),
                Ok(Sk8brdMsgs::MsgHardReset) => todo!("MsgHardReset is unused"),
                Ok(Sk8brdMsgs::MsgPowerOn) => (),
                Ok(Sk8brdMsgs::MsgPowerOff) => (),
                Ok(Sk8brdMsgs::MsgFastbootPresent) => {
                    if !msgbuf.is_empty() && msgbuf[0] != 0 {
                        send_image(&mut chan, &fastboot_image)?
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
            sess.set_blocking(false);
        }
    }

    // No more keypresses will be useful
    stdin_handler.abort();

    // Pick up the trash
    crossterm::terminal::disable_raw_mode()?;

    // Power off the board on goodbye
    send_ack(&mut chan, Sk8brdMsgs::MsgPowerOff)?;

    sess.disconnect(
        Option::Some(ssh2::DisconnectCode::ConnectionLost),
        "bye",
        Option::Some("C"),
    )?;

    println!("Goodbye");
    Ok(())
}
