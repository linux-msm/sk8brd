use clap::Parser;
use colored::Colorize;
use sk8brd::{
    console_print, list_device, parse_recv_msg, select_brd, send_ack, send_image, send_msg,
    u8_to_msg, Sk8brdMsgs, MSG_HDR_SIZE,
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
    power_cycle: bool
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

// For raw mode TTY
#[allow(clippy::explicit_write)]
#[tokio::main]
async fn main() {
    let mut hdr_buf = [0u8; MSG_HDR_SIZE];
    let mut buf = [0u8; SSH_BUFFER_SIZE];
    let mut key_buf = [0u8; 1];
    let quit = Arc::new(Mutex::new(false));
    let args = Args::parse();

    let fastboot_image = fs::read(args.image_path).expect("boot image not found");

    // Connect to the local SSH server
    let tcp = match TcpStream::connect(format!("{}:{}", args.farm, args.port)) {
        Ok(t) => t,
        Err(e) => panic!("Server connection failed: {e}"),
    };
    let mut sess = Session::new().unwrap();
    sess.set_tcp_stream(tcp);
    match sess.handshake() {
        Ok(s) => s,
        Err(e) => panic!("SSH handshake failed: {e}"),
    };

    // Try to authenticate with the first identity in the agent.
    match sess.userauth_agent("cdba") {
        Ok(s) => s,
        Err(e) => panic!("SSH agent authentication failed: {e}"),
    };

    let mut chan = sess.channel_session().unwrap();
    chan.exec(CDBA_SERVER_BIN_NAME).unwrap();

    sess.set_blocking(false);

    send_msg(&mut chan, Sk8brdMsgs::MsgListDevices, 0, &[0]);
    select_brd(&mut chan, &args.board);
    if args.power_cycle {
        println!("Powering off the board first");
        send_ack(&mut chan, Sk8brdMsgs::MsgPowerOff);
    }

    crossterm::terminal::enable_raw_mode().unwrap();

    let mut quit2 = Arc::clone(&quit);
    let stdin_handler = tokio::spawn(async move {
        let mut stdin = os_pipe::dup_stdin().expect("Couldn't dup stdin");

        loop {
            if stdin.read(&mut key_buf).unwrap() > 0 {
                handle_keypress(key_buf[0] as char, &mut quit2).await;
            };
        }
    });

    while !*quit.lock().await {
        // Stream of "blue text" - status updates from the server
        let bytes_read = chan.stderr().read(&mut buf).unwrap_or(0);
        if bytes_read > 0 {
            let s = String::from_utf8_lossy(&buf[..bytes_read]);
            writeln!(stdout(), "{}\r", s.blue()).unwrap();
            stdout().flush().unwrap();
        }

        // Msg handler
        // Read the message header first
        if chan.read_exact(&mut hdr_buf).is_ok() {
            sess.set_blocking(true);
            let msg = parse_recv_msg(&hdr_buf);

            // Now read the actual data
            chan.read_exact(&mut buf[..msg.len as usize]).unwrap();

            // And process it
            match u8_to_msg(msg.r#type) {
                Sk8brdMsgs::MsgSelectBoard => send_msg(&mut chan, Sk8brdMsgs::MsgPowerOn, 0, &[0]),
                Sk8brdMsgs::MsgConsole => console_print(&buf, msg.len),
                Sk8brdMsgs::MsgPowerOn => (),
                Sk8brdMsgs::MsgPowerOff => (),
                Sk8brdMsgs::MsgFastbootPresent => send_image(&mut chan, &fastboot_image),
                Sk8brdMsgs::MsgListDevices => list_device(&buf, msg.len),
                _ => println!("unknown msg: {:?}", msg),
            };
            sess.set_blocking(false);
        }
    }

    // No more keypresses will be useful
    stdin_handler.abort();

    // Pick up the trash
    crossterm::terminal::disable_raw_mode().unwrap();

    // Power off the board on goodbye
    send_ack(&mut chan, Sk8brdMsgs::MsgPowerOff);

    sess.disconnect(
        Option::Some(ssh2::DisconnectCode::ConnectionLost),
        "bye",
        Option::Some("C"),
    )
    .unwrap();

    println!("Goodbye");
}
