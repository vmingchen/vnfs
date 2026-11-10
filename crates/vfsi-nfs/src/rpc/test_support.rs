//! Bounded real-TCP RPC framing for protocol regression tests.
use std::io::{Read, Write};
use std::net::TcpStream;

pub(crate) fn read_record(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut marker = [0; 4];
        socket.read_exact(&mut marker)?;
        let marker = u32::from_be_bytes(marker);
        let size = (marker & 0x7fff_ffff) as usize;
        assert!(out.len() + size < 64 * 1024);
        let start = out.len();
        out.resize(start + size, 0);
        socket.read_exact(&mut out[start..])?;
        if marker & 0x8000_0000 != 0 {
            return Ok(out);
        }
    }
}

pub(crate) fn write_record(socket: &mut TcpStream, body: &[u8]) {
    socket
        .write_all(&(0x8000_0000 | body.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(body).unwrap();
}

pub(crate) fn accepted_reply(socket: &mut TcpStream, request: &[u8], payload: &[u8]) {
    let mut body = request[..4].to_vec();
    for word in [1u32, 0, 0, 0, 0] {
        body.extend_from_slice(&word.to_be_bytes());
    }
    body.extend_from_slice(payload);
    write_record(socket, &body);
}
