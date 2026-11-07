/*
    The WebSocket to the hpsdr-rs TCI server (ewebsock: a thread natively,
    the browser's own WebSocket on the web), as parsed TCI commands in and
    out plus the binary stream messages.
*/

use ewebsock::{WsEvent, WsMessage, WsReceiver, WsSender};
use hpsdr_proto::tci::{self, Command};

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Open,
    Closed(String),
}

pub enum Incoming {
    /// The connection just opened.
    Opened,
    Command(Command),
    Binary(Vec<u8>),
}

pub struct Link {
    tx: WsSender,
    rx: WsReceiver,
    pub status: Status,
    /// Bytes received and sent (TX audio), for the status line.
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl Link {
    /// Starts connecting to `url` (ws://host:port); egui is asked to
    /// repaint whenever something arrives.
    pub fn connect(url: &str, ctx: egui::Context) -> Result<Self, String> {
        let (tx, rx) = ewebsock::connect_with_wakeup(url, ewebsock::Options::default(), move || ctx.request_repaint())?;
        Ok(Self { tx, rx, status: Status::Connecting, bytes_in: 0, bytes_out: 0 })
    }

    pub fn send(&mut self, cmd: Command) {
        if self.status == Status::Open {
            self.tx.send(WsMessage::Text(cmd.to_string()));
        }
    }

    /// A binary stream message (TX audio).
    pub fn send_binary(&mut self, data: Vec<u8>) {
        if self.status == Status::Open {
            self.bytes_out += data.len() as u64;
            self.tx.send(WsMessage::Binary(data));
        }
    }

    /// Everything that's arrived since the last call.
    pub fn poll(&mut self) -> Vec<Incoming> {
        let mut out = Vec::new();
        while let Some(event) = self.rx.try_recv() {
            match event {
                WsEvent::Opened => {
                    self.status = Status::Open;
                    out.push(Incoming::Opened);
                }
                WsEvent::Message(WsMessage::Text(text)) => {
                    self.bytes_in += text.len() as u64;
                    out.extend(tci::parse(&text).into_iter().map(Incoming::Command));
                }
                WsEvent::Message(WsMessage::Binary(data)) => {
                    self.bytes_in += data.len() as u64;
                    out.push(Incoming::Binary(data));
                }
                WsEvent::Message(_) => {}
                WsEvent::Error(e) => self.status = Status::Closed(e),
                WsEvent::Closed => self.status = Status::Closed("closed by the server".to_string()),
            }
        }
        out
    }
}
