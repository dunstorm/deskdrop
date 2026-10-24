//! Encrypted, authenticated transport: TCP + Noise NNpsk0 (pre-shared pairing secret).
//!
//! Wire format: [u16 BE len][noise ciphertext]. Each plaintext frame is
//! [u8 final-flag][fragment]; fragments are joined into one logical message.

use crate::proto::{self, Msg};
use snow::StatelessTransportState;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"deskdrop/1";
const TAG: usize = 16;
const MAX_FRAME: usize = 65535;
const MAX_FRAG: usize = MAX_FRAME - TAG - 1;
const MAX_MSG: usize = 256 << 20;
pub const READ_TIMEOUT: Duration = Duration::from_secs(45);

pub struct Conn {
    w: Mutex<Writer>,
    sock: TcpStream,
    pub peer: String,
}

struct Writer {
    sock: TcpStream,
    st: Arc<StatelessTransportState>,
    nonce: u64,
    buf: Vec<u8>,
}

pub struct Reader {
    sock: TcpStream,
    st: Arc<StatelessTransportState>,
    nonce: u64,
    buf: Vec<u8>,
}

fn nerr(e: snow::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("noise: {e}"))
}

fn write_frame(s: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    let mut v = Vec::with_capacity(2 + data.len());
    v.extend_from_slice(&(data.len() as u16).to_be_bytes());
    v.extend_from_slice(data);
    s.write_all(&v)
}

fn read_frame(s: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut len = [0u8; 2];
    s.read_exact(&mut len)?;
    buf.resize(u16::from_be_bytes(len) as usize, 0);
    s.read_exact(buf)
}

/// Run the handshake and split into a sender handle and a reader.
pub fn handshake(mut sock: TcpStream, psk: &[u8; 32], initiator: bool) -> io::Result<(Arc<Conn>, Reader)> {
    let peer = sock.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    let b = snow::Builder::new(PATTERN.parse().unwrap())
        .psk(0, psk)
        .prologue(PROLOGUE);
    let mut hs = if initiator {
        b.build_initiator()
    } else {
        b.build_responder()
    }
    .map_err(nerr)?;
    let mut msg = vec![0u8; MAX_FRAME];
    let mut frame = Vec::new();
    if initiator {
        let n = hs.write_message(&[], &mut msg).map_err(nerr)?;
        write_frame(&mut sock, &msg[..n])?;
        read_frame(&mut sock, &mut frame)?;
        hs.read_message(&frame, &mut msg).map_err(|_| auth_fail())?;
    } else {
        read_frame(&mut sock, &mut frame)?;
        hs.read_message(&frame, &mut msg).map_err(|_| auth_fail())?;
        let n = hs.write_message(&[], &mut msg).map_err(nerr)?;
        write_frame(&mut sock, &msg[..n])?;
    }
    let st = Arc::new(hs.into_stateless_transport_mode().map_err(nerr)?);
    sock.set_read_timeout(Some(READ_TIMEOUT))?;
    let conn = Arc::new(Conn {
        w: Mutex::new(Writer {
            sock: sock.try_clone()?,
            st: st.clone(),
            nonce: 0,
            buf: vec![0; MAX_FRAME],
        }),
        sock: sock.try_clone()?,
        peer,
    });
    Ok((
        conn,
        Reader {
            sock,
            st,
            nonce: 0,
            buf: vec![0; MAX_FRAME],
        },
    ))
}

fn auth_fail() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "handshake failed (secret mismatch?)")
}

impl Conn {
    pub fn send(&self, m: &Msg) -> io::Result<()> {
        let plain = proto::encode(m);
        let mut w = self.w.lock().unwrap();
        let w = &mut *w;
        let n_frags = plain.len().div_ceil(MAX_FRAG).max(1);
        let mut out = Vec::with_capacity(plain.len() + n_frags * (TAG + 3));
        let mut frag = Vec::with_capacity(MAX_FRAG + 1);
        for i in 0..n_frags {
            let part = &plain[i * MAX_FRAG..((i + 1) * MAX_FRAG).min(plain.len())];
            frag.clear();
            frag.push((i + 1 == n_frags) as u8);
            frag.extend_from_slice(part);
            let n = w.st.write_message(w.nonce, &frag, &mut w.buf).map_err(nerr)?;
            w.nonce += 1;
            out.extend_from_slice(&(n as u16).to_be_bytes());
            out.extend_from_slice(&w.buf[..n]);
        }
        w.sock.write_all(&out)
    }

    pub fn close(&self) {
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

impl Reader {
    pub fn recv(&mut self) -> io::Result<Msg> {
        let mut msg = Vec::new();
        let mut frame = Vec::new();
        loop {
            read_frame(&mut self.sock, &mut frame)?;
            let n = self.st.read_message(self.nonce, &frame, &mut self.buf).map_err(nerr)?;
            self.nonce += 1;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "empty frame"));
            }
            msg.extend_from_slice(&self.buf[1..n]);
            if msg.len() > MAX_MSG {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
            }
            if self.buf[0] == 1 {
                return proto::decode(msg);
            }
        }
    }
}

pub fn psk_from_secret(secret: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"deskdrop-psk:");
    h.update(secret.trim().as_bytes());
    h.finalize().into()
}
