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

/// Borrow the NFS COMPOUND operation stream after the RPC/auth/tag envelope.
pub(crate) fn compound_operations(request: &[u8]) -> &[u8] {
    let word = |at| u32::from_be_bytes(request[at..at + 4].try_into().unwrap());
    let mut at = 24;
    for _ in 0..2 {
        at += 8 + (word(at + 4) as usize).div_ceil(4) * 4;
    }
    at += 4 + (word(at) as usize).div_ceil(4) * 4 + 8;
    &request[at..]
}

pub(crate) fn successful_sequence_reply(request: &[u8], operations: u32) -> Vec<u8> {
    let ops = compound_operations(request);
    assert_eq!(
        u32::from_be_bytes(ops[..4].try_into().unwrap()),
        nfsv41_sys::nfs_opnum4_NFS4_OP_SEQUENCE
    );
    let mut payload = Vec::new();
    for word in [
        0u32,
        0,
        operations,
        nfsv41_sys::nfs_opnum4_NFS4_OP_SEQUENCE,
        0,
    ] {
        payload.extend_from_slice(&word.to_be_bytes());
    }
    payload.extend_from_slice(&ops[4..28]);
    payload.extend_from_slice(&[0; 12]);
    payload
}

/// A two-page directory peer requiring the verifier returned with its cookie.
#[cfg(feature = "test-faults")]
pub(crate) fn readdir_verifier_peer(
    listener: std::net::TcpListener,
    sparse: bool,
) -> std::thread::JoinHandle<Vec<(u64, [u8; 8])>> {
    readdir_test_peer(listener, sparse, None)
}

/// Filtered pages followed by success, a cookie cycle, or a status error.
#[cfg(feature = "test-faults")]
pub(crate) fn readdir_filtered_peer(
    listener: std::net::TcpListener,
    outcome: &str,
) -> std::thread::JoinHandle<Vec<(u64, [u8; 8])>> {
    readdir_test_peer(listener, false, Some(outcome.to_owned()))
}

#[cfg(feature = "test-faults")]
fn readdir_test_peer(
    listener: std::net::TcpListener,
    sparse: bool,
    filtered: Option<String>,
) -> std::thread::JoinHandle<Vec<(u64, [u8; 8])>> {
    std::thread::spawn(move || {
        use nfsv41_sys::*;
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut observed = Vec::new();
        let advancing = filtered
            .as_deref()
            .is_some_and(|mode| mode.starts_with("advancing-"));
        let pages = match filtered.as_deref() {
            Some("siblings-status") => 3,
            Some("advancing-limit" | "advancing-visible" | "advancing-eof") => 16,
            _ => 2,
        };
        for page in 0..pages {
            let request = read_record(&mut socket).unwrap();
            let ops = compound_operations(&request);
            let word = |at| u32::from_be_bytes(ops[at..at + 4].try_into().unwrap());
            assert_eq!(word(36), nfs_opnum4_NFS4_OP_PUTFH);
            let mut at = 44 + (word(40) as usize).div_ceil(4) * 4;
            let child = word(at) == nfs_opnum4_NFS4_OP_LOOKUP;
            if child {
                at += 8 + (word(at + 4) as usize).div_ceil(4) * 4;
                assert_eq!(word(at), nfs_opnum4_NFS4_OP_GETFH);
                at += 4;
            }
            assert_eq!(word(at), nfs_opnum4_NFS4_OP_READDIR);
            let cookie = u64::from_be_bytes(ops[at + 4..at + 12].try_into().unwrap());
            let verifier: [u8; 8] = ops[at + 12..at + 20].try_into().unwrap();
            observed.push((cookie, verifier));
            if page == 1 && filtered.as_deref() == Some("siblings-lost") {
                return observed;
            }
            let valid = if page == 0 {
                cookie == 0 && verifier == [0; 8]
            } else {
                cookie == if advancing { 41 + page } else { 42 } && verifier == [0x7e; 8]
            };
            let mut payload = successful_sequence_reply(&request, if child { 5 } else { 3 });
            let mut push = |word: u32| payload.extend_from_slice(&word.to_be_bytes());
            push(nfs_opnum4_NFS4_OP_PUTFH);
            push(0);
            if child {
                push(nfs_opnum4_NFS4_OP_LOOKUP);
                push(0);
                push(nfs_opnum4_NFS4_OP_GETFH);
                push(0);
                push(4);
                payload.extend_from_slice(b"dir!");
            }
            for word in [
                nfs_opnum4_NFS4_OP_READDIR,
                if valid
                    && !(page != 0
                        && filtered
                            .as_deref()
                            .is_some_and(|mode| mode.ends_with("status")))
                {
                    0
                } else {
                    nfsstat4_NFS4ERR_BAD_COOKIE
                },
            ] {
                payload.extend_from_slice(&word.to_be_bytes());
            }
            if valid
                && !(page != 0
                    && filtered
                        .as_deref()
                        .is_some_and(|mode| mode.ends_with("status")))
            {
                payload.extend_from_slice(&[0x7e; 8]);
                payload.extend_from_slice(&1u32.to_be_bytes()); // entry present
                let cycle = page == 1 && filtered.as_deref() == Some("cycle");
                payload
                    .extend_from_slice(&(if cycle { 42u64 } else { 42u64 + page }).to_be_bytes());
                let dot = if advancing {
                    page + 1 < pages
                        || matches!(
                            filtered.as_deref(),
                            Some("advancing-eof" | "advancing-limit")
                        )
                } else {
                    filtered.is_some() && (page == 0 || cycle)
                };
                payload.extend_from_slice(&(if dot { 1u32 } else { 3u32 }).to_be_bytes());
                payload.extend_from_slice(if dot {
                    b".\0\0\0"
                } else if page == 0 {
                    b"one\0"
                } else {
                    b"two\0"
                });
                let mode_bit = if sparse { 0 } else { 1 << (FATTR4_MODE - 32) };
                for word in [
                    2u32,
                    1 << FATTR4_TYPE,
                    mode_bit | (1 << (FATTR4_TIME_MODIFY - 32)),
                ] {
                    payload.extend_from_slice(&word.to_be_bytes());
                }
                let mut attrs = nfs_ftype4_NF4REG.to_be_bytes().to_vec();
                if !sparse {
                    attrs.extend_from_slice(&0o640u32.to_be_bytes());
                }
                attrs.extend_from_slice(&1_700_000_001_i64.to_be_bytes());
                attrs.extend_from_slice(&123u32.to_be_bytes());
                payload.extend_from_slice(&(attrs.len() as u32).to_be_bytes());
                payload.extend_from_slice(&attrs);
                payload.extend_from_slice(&0u32.to_be_bytes()); // next entry absent
                let eof = if advancing {
                    page + 1 == pages && filtered.as_deref() != Some("advancing-limit")
                } else {
                    !cycle && page != 0
                };
                payload.extend_from_slice(&u32::from(eof).to_be_bytes());
            } else {
                payload[..4].copy_from_slice(&nfsstat4_NFS4ERR_BAD_COOKIE.to_be_bytes());
            }
            if page == 0
                && filtered
                    .as_deref()
                    .is_some_and(|mode| mode.starts_with("siblings-"))
            {
                // Three initial pages: filtered, visible, filtered. Only the
                // filtered siblings need independent followup RPCs.
                let prefix = successful_sequence_reply(&request, 7);
                let mut middle = payload[prefix.len()..].to_vec();
                middle[40] = 120;
                let last = payload[prefix.len()..].to_vec();
                payload[..prefix.len()].copy_from_slice(&prefix);
                payload.extend_from_slice(&middle);
                payload.extend_from_slice(&last);
            }
            accepted_reply(&mut socket, &request, &payload);
        }
        observed
    })
}
