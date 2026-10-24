//! Logical messages exchanged between peers (inside the encrypted channel).

use serde::{Deserialize, Serialize};
use std::io;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Entry {
    /// Relative path, '/'-separated. Top-level entries have no '/'.
    pub path: String,
    pub size: u64,
    pub dir: bool,
    /// Unix seconds.
    pub mtime: i64,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t")]
pub enum Ctl {
    Hello {
        name: String,
        version: String,
    },
    /// Clipboard changed on the sender. PNG bytes (if `png`) travel as the blob.
    Clip {
        id: u64,
        text: Option<String>,
        html: Option<String>,
        png: bool,
        files: Vec<Entry>,
    },
    /// Receiver wants the file contents of clip `id`.
    Fetch {
        id: u64,
    },
    Cancel {
        id: u64,
    },
    Done {
        id: u64,
    },
    Fail {
        id: u64,
        msg: String,
    },
    Ping,
}

pub enum Msg {
    Ctl(Ctl, Vec<u8>),
    /// File data for entry `idx` of clip `id`, sent strictly in order.
    Chunk {
        id: u64,
        idx: u32,
        data: Vec<u8>,
    },
}

const T_CTL: u8 = 0;
const T_CHUNK: u8 = 1;

pub fn encode(m: &Msg) -> Vec<u8> {
    match m {
        Msg::Ctl(c, blob) => {
            let json = serde_json::to_vec(c).expect("serialize");
            let mut v = Vec::with_capacity(5 + json.len() + blob.len());
            v.push(T_CTL);
            v.extend_from_slice(&(json.len() as u32).to_be_bytes());
            v.extend_from_slice(&json);
            v.extend_from_slice(blob);
            v
        }
        Msg::Chunk { id, idx, data } => {
            let mut v = Vec::with_capacity(13 + data.len());
            v.push(T_CHUNK);
            v.extend_from_slice(&id.to_be_bytes());
            v.extend_from_slice(&idx.to_be_bytes());
            v.extend_from_slice(data);
            v
        }
    }
}

fn bad(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s.to_string())
}

pub fn decode(mut b: Vec<u8>) -> io::Result<Msg> {
    match b.first() {
        Some(&T_CTL) if b.len() >= 5 => {
            let n = u32::from_be_bytes(b[1..5].try_into().unwrap()) as usize;
            if b.len() < 5 + n {
                return Err(bad("short ctl"));
            }
            let c: Ctl = serde_json::from_slice(&b[5..5 + n]).map_err(|e| bad(&e.to_string()))?;
            let blob = b.split_off(5 + n);
            Ok(Msg::Ctl(c, blob))
        }
        Some(&T_CHUNK) if b.len() >= 13 => {
            let id = u64::from_be_bytes(b[1..9].try_into().unwrap());
            let idx = u32::from_be_bytes(b[9..13].try_into().unwrap());
            let data = b.split_off(13);
            Ok(Msg::Chunk { id, idx, data })
        }
        _ => Err(bad("unknown message")),
    }
}
