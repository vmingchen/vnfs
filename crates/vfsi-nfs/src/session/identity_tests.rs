use super::*;

#[test]
fn file_review_session_sequence_wraps_on_the_wire_without_panicking() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut observed = Vec::new();
        for _ in 0..4 {
            let mut request = Vec::new();
            loop {
                let mut marker = [0; 4];
                if stream.read_exact(&mut marker).is_err() {
                    return observed;
                }
                let marker = u32::from_be_bytes(marker);
                let length = (marker & 0x7fff_ffff) as usize;
                assert!(length <= 4096);
                let start = request.len();
                request.resize(start + length, 0);
                stream.read_exact(&mut request[start..]).unwrap();
                if marker & 0x8000_0000 != 0 {
                    break;
                }
            }
            // The sole COMPOUND operation is SEQUENCE; its last four words
            // are sequence ID, slot ID, highest slot ID, and cache-this.
            let seq = u32::from_be_bytes(
                request[request.len() - 16..request.len() - 12]
                    .try_into()
                    .unwrap(),
            );
            observed.push(seq);
            let xid = u32::from_be_bytes(request[..4].try_into().unwrap());
            let mut reply = Vec::new();
            for word in [xid, 1, 0, 0, 0, 0, 0, 0, 1, nfs_opnum4_NFS4_OP_SEQUENCE, 0] {
                reply.extend_from_slice(&word.to_be_bytes());
            }
            reply.extend_from_slice(&[7; 16]);
            for word in [seq, 0, 0, 0, 0] {
                reply.extend_from_slice(&word.to_be_bytes());
            }
            stream
                .write_all(&(0x8000_0000 | reply.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&reply).unwrap();
        }
        observed
    });
    // Construct an established test session without performing a handshake.
    // clientid=0 prevents teardown RPCs against this four-request responder.
    let rpc = RpcClient::connect_with_timeouts(
        &address.to_string(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap();
    let mut session = Session {
        rpc,
        clientid: 0,
        sessionid: [7; 16],
        minorversion: 1,
        slot_seqid: u32::MAX - 1,
        create_session_seqid: 1,
        session_created: false,
        owns_clientid: false,
        poisoned: false,
        max_requestsize: 4096,
        max_responsesize: 4096,
        max_operations: 16,
        open_owner: OpenOwner {
            name: Vec::new(),
            seqid: 0,
        },
        path_owner: OpenOwner {
            name: Vec::new(),
            seqid: 0,
        },
        client_owner: None,
    };
    for expected in [u32::MAX, 0, 1, 2] {
        let mut compound = Compound::new();
        let reply = session.compound(&mut compound).unwrap();
        session.expect_all_ok(&reply).unwrap();
        assert_eq!(session.slot_seqid, expected);
    }
    drop(session);
    assert_eq!(server.join().unwrap(), [u32::MAX - 1, u32::MAX, 0, 1]);
}

#[test]
fn create_session_preserves_server_exchange_sequence() {
    for sequence in [0, 1, 2, 17, u32::MAX] {
        let args = create_session_args_with_sequence(41, sequence);
        assert_eq!(args.csa_clientid, 41);
        assert_eq!(args.csa_sequence, sequence);
    }
}

#[test]
fn rejected_reused_identity_setup_does_not_destroy_original_client() {
    let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minorversion = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let authentication = NfsAuth::AuthSys;
    let owner = format!(
        "prr-session-setup-failure-{}-{minorversion}",
        std::process::id()
    );
    let verifier = make_verifier();
    let mut original = Session::connect_minor_with_identity(
        &host,
        minorversion,
        Duration::from_secs(5),
        Duration::from_secs(5),
        &authentication,
        Some(owner.as_bytes()),
        Some(verifier),
    )
    .unwrap();
    let rpc = RpcClient::connect_with_authentication(
        &host,
        Duration::from_secs(5),
        Duration::from_secs(5),
        &authentication,
    )
    .unwrap();
    let mut failed = Session {
        rpc,
        clientid: 0,
        sessionid: [0; 16],
        minorversion,
        slot_seqid: 1,
        create_session_seqid: 1,
        session_created: false,
        owns_clientid: false,
        poisoned: false,
        max_requestsize: 4 * 1024 * 1024,
        max_responsesize: 4 * 1024 * 1024,
        max_operations: 256,
        open_owner: OpenOwner {
            name: b"unused".to_vec(),
            seqid: 0,
        },
        path_owner: OpenOwner {
            name: b"unused-path".to_vec(),
            seqid: 0,
        },
        client_owner: Some(owner.as_bytes().into()),
    };
    failed.exchange_id(Some(verifier)).unwrap();
    assert!(failed.shares_identity(&original));
    assert!(!failed.owns_clientid);
    failed.create_session_seqid = failed.create_session_seqid.wrapping_add(17);
    assert_eq!(
        failed.create_session().unwrap_err().status,
        nfsstat4_NFS4ERR_SEQ_MISORDERED
    );
    drop(failed);
    let mut probe = Compound::new();
    probe.putrootfh();
    probe.getfh();
    let result = original.compound(&mut probe).unwrap();
    original.expect_all_ok(&result).unwrap();
    original.shutdown().unwrap();
}
