use super::*;

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
    let authentication = NfsAuthentication::AuthSys;
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
