//! `TokioHost`: a [`Host`] over real local sockets, the system clock and an
//! in-memory SQLite database.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use hearth_mp_protocol::{decode, Frame, WireMsg};
use hearth_mp_server::{Attachment, Host, Result, Sql};
use hearth_mp_test::MemHost;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::room::FUTURE_EVENT;

/// A socket identity, unique for the server's lifetime.
pub type Socket = u64;

/// What the room tells one socket's writer task.
#[derive(Debug)]
pub enum Out {
    Frame(WireMsg),
    Close(u16, String),
}

struct Inner {
    /// Used only for its in-memory SQLite `Sql` implementation.
    db: MemHost,
    sockets: RefCell<Vec<(Socket, UnboundedSender<Out>)>>,
    attachments: RefCell<HashMap<Socket, Attachment>>,
    alarm: Cell<Option<u64>>,
    /// The `forward.json` cases marked `inject`.
    forward: Vec<Value>,
}

#[derive(Clone)]
pub struct TokioHost {
    inner: Rc<Inner>,
}

impl TokioHost {
    pub fn new() -> Self {
        let forward: Vec<Value> =
            serde_json::from_str(include_str!("../../../conformance/vectors/forward.json"))
                .expect("forward.json is JSON");
        Self {
            inner: Rc::new(Inner {
                db: MemHost::new(),
                sockets: RefCell::new(Vec::new()),
                attachments: RefCell::new(HashMap::new()),
                alarm: Cell::new(None),
                forward: forward
                    .into_iter()
                    .filter(|case| case["inject"] == true)
                    .collect(),
            }),
        }
    }

    pub fn add_socket(&self, socket: Socket, out: UnboundedSender<Out>) {
        self.inner.sockets.borrow_mut().push((socket, out));
    }

    /// Forget a socket the client closed. Returns whether it was still open.
    pub fn remove_socket(&self, socket: Socket) -> bool {
        let mut sockets = self.inner.sockets.borrow_mut();
        let before = sockets.len();
        sockets.retain(|(s, _)| *s != socket);
        self.inner.attachments.borrow_mut().remove(&socket);
        sockets.len() != before
    }

    pub fn alarm_at(&self) -> Option<u64> {
        self.inner.alarm.get()
    }

    fn sender(&self, socket: Socket) -> Option<UnboundedSender<Out>> {
        self.inner
            .sockets
            .borrow()
            .iter()
            .find(|(s, _)| *s == socket)
            .map(|(_, tx)| tx.clone())
    }

    /// The injected frames, in the socket's encoding.
    fn forward_frames(&self, binary: bool) -> Vec<WireMsg> {
        self.inner
            .forward
            .iter()
            .filter_map(|case| {
                if binary {
                    case["msgpack_hex"]
                        .as_str()
                        .map(|hex| WireMsg::Binary(unhex(hex)))
                } else {
                    case["json"]
                        .as_object()
                        .map(|json| WireMsg::Text(Value::Object(json.clone()).to_string()))
                }
            })
            .collect()
    }
}

impl Default for TokioHost {
    fn default() -> Self {
        Self::new()
    }
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap_or(0))
        .collect()
}

impl hearth_mp_server::__private::Sealed for TokioHost {}

impl Host for TokioHost {
    type Socket = Socket;

    fn sockets(&self) -> Vec<Socket> {
        self.inner
            .sockets
            .borrow()
            .iter()
            .map(|(s, _)| *s)
            .collect()
    }

    fn attachment(&self, socket: &Socket) -> Option<Attachment> {
        self.inner.attachments.borrow().get(socket).cloned()
    }

    fn set_attachment(&self, socket: &Socket, attachment: &Attachment) {
        if self.sender(*socket).is_some() {
            self.inner
                .attachments
                .borrow_mut()
                .insert(*socket, attachment.clone());
        }
    }

    fn send(&self, socket: &Socket, msg: WireMsg) {
        let Some(tx) = self.sender(*socket) else {
            return;
        };
        // The Conf room's marker event becomes the forward-compatibility frames
        // (a room cannot emit unknown opcodes itself).
        if let Ok(Frame::Event { name, .. }) = decode(&msg) {
            if name == FUTURE_EVENT {
                for frame in self.forward_frames(matches!(msg, WireMsg::Binary(_))) {
                    let _ = tx.send(Out::Frame(frame));
                }
                return;
            }
        }
        let _ = tx.send(Out::Frame(msg));
    }

    fn close(&self, socket: &Socket, code: u16, reason: &str) {
        if let Some(tx) = self.sender(*socket) {
            let _ = tx.send(Out::Close(code, reason.to_owned()));
        }
        self.remove_socket(*socket);
    }

    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn sql(&self) -> &dyn Sql {
        self.inner.db.sql()
    }

    fn sql_handle(&self) -> Rc<dyn Sql> {
        Rc::new(self.inner.db.clone())
    }

    async fn set_alarm(&self, at_ms: Option<u64>) -> Result<()> {
        self.inner.alarm.set(at_ms);
        Ok(())
    }

    async fn wipe(&self) -> Result<()> {
        self.inner.db.wipe().await
    }
}
