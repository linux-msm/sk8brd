use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::io::{stdout, Write};
use std::mem::size_of;
use std::sync::Arc;
use tokio::sync::Mutex;

#[repr(u8)]
#[derive(Debug, PartialEq)]
#[non_exhaustive]
pub enum Sk8brdMsgs {
    MsgSelectBoard = 1,
    MsgConsole,
    MsgHardReset,
    MsgPowerOn,
    MsgPowerOff,
    MsgFastbootPresent,
    MsgFastbootDownload,
    MsgFastbootBoot,
    MsgStatusUpdate,
    MsgVbusOn,
    MsgVbusOff,
    MsgFastbootReboot,
    MsgSendBreak,
    MsgListDevices,
    MsgBoardInfo,
    MsgFastbootContinue,
}

impl TryFrom<u8> for Sk8brdMsgs {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Sk8brdMsgs::MsgSelectBoard),
            2 => Ok(Sk8brdMsgs::MsgConsole),
            3 => Ok(Sk8brdMsgs::MsgHardReset),
            4 => Ok(Sk8brdMsgs::MsgPowerOn),
            5 => Ok(Sk8brdMsgs::MsgPowerOff),
            6 => Ok(Sk8brdMsgs::MsgFastbootPresent),
            7 => Ok(Sk8brdMsgs::MsgFastbootDownload),
            8 => Ok(Sk8brdMsgs::MsgFastbootBoot),
            9 => Ok(Sk8brdMsgs::MsgStatusUpdate),
            10 => Ok(Sk8brdMsgs::MsgVbusOn),
            11 => Ok(Sk8brdMsgs::MsgVbusOff),
            12 => Ok(Sk8brdMsgs::MsgFastbootReboot),
            13 => Ok(Sk8brdMsgs::MsgSendBreak),
            14 => Ok(Sk8brdMsgs::MsgListDevices),
            15 => Ok(Sk8brdMsgs::MsgBoardInfo),
            16 => Ok(Sk8brdMsgs::MsgFastbootContinue),
            _ => Err(format!("Unknown msg package {value}")),
        }
    }
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
#[repr(C)]
#[repr(packed(1))]
pub struct Sk8brdMsg {
    pub r#type: u8,
    pub len: u16,
}
pub const MSG_HDR_SIZE: usize = size_of::<Sk8brdMsg>();

pub async fn send_msg(
    write_sink: &mut Arc<Mutex<impl Write>>,
    r#type: Sk8brdMsgs,
    buf: &[u8],
) -> anyhow::Result<()> {
    // Make sure we're not trying to send two messages at once
    let mut write_sink = write_sink.lock().await;

    let len = buf.len();
    let hdr = [r#type as u8, (len & 0xff) as u8, ((len >> 8) & 0xff) as u8];

    write_sink.write_all(&hdr)?;
    write_sink.write_all(buf)?;
    Ok(())
}

pub async fn send_ack(
    write_sink: &mut Arc<Mutex<impl Write>>,
    r#type: Sk8brdMsgs,
) -> anyhow::Result<()> {
    send_msg(write_sink, r#type, &[]).await
}

pub fn parse_recv_msg(buf: &[u8]) -> Sk8brdMsg {
    let msg: Sk8brdMsg = Sk8brdMsg {
        r#type: buf[0],
        len: (buf[2] as u16) << 8 | buf[1] as u16,
    };

    // println!("{:?}", msg);

    msg
}

pub async fn console_print(buf: &[u8]) {
    print!("{}", String::from_utf8_lossy(buf));
    stdout().flush().unwrap();
}

#[allow(clippy::explicit_write)]
pub async fn send_image(
    write_sink: &mut Arc<Mutex<impl Write>>,
    buf: &[u8],
    quit: &Arc<Mutex<bool>>,
) -> anyhow::Result<()> {
    let mut last_percent_done: usize = 0;
    let mut bytes_sent = 0;

    for chunk in buf.chunks(2048) {
        let percent_done = 100 * bytes_sent / buf.len();

        if *quit.lock().await {
            return Ok(());
        }

        if percent_done != last_percent_done {
            let s = format!("Sending image: {}%\r", percent_done);
            print!("{}", s.green());
            stdout().flush()?;
        }

        send_msg(write_sink, Sk8brdMsgs::MsgFastbootDownload, chunk).await?;

        bytes_sent += chunk.len();
        last_percent_done = percent_done;

        if bytes_sent == buf.len() {
            print!("\r{}\r", " ".repeat(80));
            print!("{}\r\n", String::from("Image sent!").green());
            stdout().flush()?;
        }
    }

    send_ack(write_sink, Sk8brdMsgs::MsgFastbootDownload).await
}

pub async fn select_brd(write_sink: &mut Arc<Mutex<impl Write>>, name: &str) -> anyhow::Result<()> {
    send_msg(write_sink, Sk8brdMsgs::MsgSelectBoard, name.as_bytes()).await
}

pub async fn send_vbus_ctrl(
    write_sink: &mut Arc<Mutex<impl Write>>,
    en: bool,
) -> anyhow::Result<()> {
    send_ack(
        write_sink,
        if en {
            Sk8brdMsgs::MsgVbusOn
        } else {
            Sk8brdMsgs::MsgVbusOff
        },
    )
    .await
}

#[allow(clippy::explicit_write)]
pub fn print_string_msg(buf: &[u8]) {
    if buf.is_empty() {
        return;
    }

    println!("{}\r", String::from_utf8_lossy(buf));
    stdout().flush().unwrap();
}

pub fn list_boards() {}
