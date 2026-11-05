use super::*;

#[test]
fn file_review_path_owner_sequences_wrap_for_scalar_batched_and_merged_io() {
    let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let mut client = NfsClient::connect_minor(&host, minor).unwrap();
    let root = client.root().clone();
    let name = format!("file-review-sequence-{}-{minor}", std::process::id());
    let directory = client.mkdir(&root, &name).unwrap();
    let names = ["a", "b", "c"];
    for name in names {
        let (fh, stateid) = client
            .open(
                &directory,
                name.as_bytes(),
                OPEN4_SHARE_ACCESS_BOTH,
                OpenCreate::Exclusive,
            )
            .unwrap();
        client.write(&fh, &stateid, 0, b"data").unwrap();
        client.close(&fh, &stateid).unwrap();
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let opens: Vec<_> = names
            .iter()
            .map(|name| OpenOp {
                dir: directory.clone(),
                name: name.as_bytes().to_vec(),
                access: OPEN4_SHARE_ACCESS_READ,
                create: OpenCreate::NoCreate,
            })
            .collect();
        client.session.path_owner.seqid = u32::MAX;
        let opened = client.open_many_path(&opens).unwrap();
        assert_eq!(client.session.path_owner.seqid, 2);
        let closes: Vec<_> = opened
            .into_iter()
            .map(|(fh, stateid)| CloseOp { fh, stateid })
            .collect();
        client.session.path_owner.seqid = u32::MAX;
        client.close_many_path(&closes).unwrap();
        assert_eq!(client.session.path_owner.seqid, 2);

        // An OPEN error must still close its successful prefix across the wrap.
        let failing = [
            OpenOp {
                dir: directory.clone(),
                name: b"a".to_vec(),
                access: OPEN4_SHARE_ACCESS_READ,
                create: OpenCreate::NoCreate,
            },
            OpenOp {
                dir: directory.clone(),
                name: b"missing".to_vec(),
                access: OPEN4_SHARE_ACCESS_READ,
                create: OpenCreate::NoCreate,
            },
        ];
        client.session.path_owner.seqid = u32::MAX;
        assert_eq!(
            client.open_many_path(&failing).unwrap_err().status,
            nfsstat4_NFS4ERR_NOENT
        );
        assert_eq!(client.session.path_owner.seqid, 1);

        client.session.path_owner.seqid = u32::MAX;
        let (fh, stateid) = client
            .open_slot(
                &directory,
                b"a",
                OPEN4_SHARE_ACCESS_READ,
                OpenCreate::NoCreate,
                OwnerSlot::Path,
                None,
            )
            .unwrap();
        assert_eq!(client.session.path_owner.seqid, 0);
        client.session.path_owner.seqid = u32::MAX;
        client.close_path(&fh, &stateid).unwrap();
        assert_eq!(client.session.path_owner.seqid, 0);

        let writes: Vec<_> = names
            .iter()
            .map(|entry| PathWriteOp {
                file: FileRef::Path(format!("{name}/{entry}").into_bytes()),
                offset: 0,
                data: b"edit",
                create: false,
                truncate: false,
                stateid: None,
            })
            .collect();
        client.session.path_owner.seqid = u32::MAX - 1;
        let result = client.writev_path_compound(&writes, true).unwrap();
        assert!(result.failed.is_none());
        assert!(result.close_failed.is_none());
        assert_eq!(result.counts, [Some(4); 3]);
        assert_eq!(client.session.path_owner.seqid, 1);

        let reads: Vec<_> = names
            .iter()
            .map(|entry| PathReadOp {
                file: FileRef::Path(format!("{name}/{entry}").into_bytes()),
                offset: 0,
                count: 4,
                stateid: None,
            })
            .collect();
        client.session.path_owner.seqid = u32::MAX - 1;
        let result = client.readv_path_compound(&reads, true).unwrap();
        assert!(result.failed.is_none());
        assert!(result.close_failed.is_none());
        assert_eq!(result.data, vec![Some(b"edit".to_vec()); 3]);
        assert_eq!(client.session.path_owner.seqid, 1);
    }));
    for name in names {
        client.remove(&directory, name).unwrap();
    }
    client.remove(&root, &name).unwrap();
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}
