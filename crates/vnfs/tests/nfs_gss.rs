#![cfg(feature = "rpcsec-gss")]

use std::time::Duration;
use std::{process::Command, thread};

use vfsi_nfs::NfsConnectOptions;
use vfsi_nfs::NfsVecFs;
use vfsi_sync::{Backend, ReadOp, VfFile, VfOffset, WriteOp};
use vnfs::{NfsAuthentication, RpcsecGssProtection};

fn options(protection: RpcsecGssProtection) -> NfsConnectOptions {
    let mut options = NfsConnectOptions::default();
    options.minorversion = Some(1);
    options.connect_timeout = Duration::from_secs(5);
    options.request_timeout = Duration::from_secs(5);
    options.authentication = NfsAuthentication::RpcsecGss {
        service_principal: Some(
            std::env::var("VNFS_GSS_SERVICE").unwrap_or_else(|_| "nfs@localhost".into()),
        ),
        protection,
    };
    options
}

#[test]
#[ignore = "requires the Kerberos/GSS-only export fixture"]
fn supported_rpcsec_gss_protection_levels_access_the_export() {
    assert_eq!(
        std::env::var("VNFS_GSS_INTEGRATION").as_deref(),
        Ok("1"),
        "VNFS_GSS_INTEGRATION=1 is required"
    );

    let host = std::env::var("VNFS_GSS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let protection_levels = [
        ("krb5", RpcsecGssProtection::Authentication),
        ("krb5i", RpcsecGssProtection::Integrity),
    ];

    for (name, protection) in protection_levels {
        let mut fs = NfsVecFs::connect_with_options(&host, options(protection))
            .unwrap_or_else(|error| panic!("connect using {name}: {error}"));
        let path = format!("/vnfs-gss-{name}-{}", std::process::id());
        let payload = format!("authenticated with {name}").into_bytes();
        fs.vwrite_owned_impl(
            &[WriteOp::from_path(&path, VfOffset::At(0), payload.clone())
                .with_creation()
                .with_truncate()],
        )
        .unwrap_or_else(|error| panic!("write using {name}: {error}"));
        let result = fs
            .vread_impl(&[ReadOp::from_path(&path, VfOffset::At(0), payload.len())])
            .unwrap_or_else(|error| panic!("read using {name}: {error}"));
        assert_eq!(result[0].data, payload, "payload using {name}");
        fs.vremove_impl(&[VfFile::from_path(&path)])
            .unwrap_or_else(|error| panic!("remove using {name}: {error}"));
    }
}

#[test]
#[ignore = "requires the Kerberos/GSS-only export fixture"]
fn gss_only_export_rejects_auth_sys() {
    assert_eq!(
        std::env::var("VNFS_GSS_INTEGRATION").as_deref(),
        Ok("1"),
        "VNFS_GSS_INTEGRATION=1 is required"
    );

    let host = std::env::var("VNFS_GSS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let error = NfsVecFs::connect_with_options(&host, {
        let mut options = NfsConnectOptions::default();
        options.minorversion = Some(1);
        options
    })
    .err()
    .expect("a GSS-only export must reject AUTH_SYS");
    assert!(
        !error.is_transport(),
        "server should reject AUTH_SYS: {error}"
    );
}

#[test]
#[ignore = "requires a one-minute renewable Kerberos ticket"]
fn renewable_ticket_allows_reconnect_after_ticket_expiry() {
    assert_eq!(
        std::env::var("VNFS_GSS_EXPIRY_INTEGRATION").as_deref(),
        Ok("1"),
        "VNFS_GSS_EXPIRY_INTEGRATION=1 is required"
    );

    let host = std::env::var("VNFS_GSS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let mut fs = NfsVecFs::connect_with_options(&host, options(RpcsecGssProtection::Integrity))
        .expect("connect while the renewable Kerberos ticket is valid");
    let path = format!("/vnfs-gss-renew-{}", std::process::id());
    let payload = b"ticket renewal and RPCSEC_GSS reconnect";
    fs.vwrite_owned_impl(
        &[WriteOp::from_path(&path, VfOffset::At(0), payload.to_vec())
            .with_creation()
            .with_truncate()],
    )
    .expect("initial authenticated write");

    // The integration script obtains a one-minute renewable TGT. Let it
    // expire, assert the cache no longer has a valid ticket, renew it, then
    // force a new NFS/RPCSEC_GSS session from that renewed cache.
    thread::sleep(Duration::from_secs(65));
    let expired = Command::new("klist")
        .arg("-s")
        .status()
        .expect("run klist to verify ticket expiry");
    assert!(
        !expired.success(),
        "the short-lived TGT should have expired"
    );
    let renewed = Command::new("kinit")
        .arg("-R")
        .status()
        .expect("run kinit to renew the renewable TGT");
    assert!(
        renewed.success(),
        "the expired-but-renewable TGT should renew"
    );

    fs.reconnect()
        .expect("reconnect using renewed GSS credentials");
    let read = fs
        .vread_impl(&[ReadOp::from_path(&path, VfOffset::At(0), payload.len())])
        .expect("read after GSS reconnect");
    assert_eq!(read[0].data, payload);
    fs.vremove_impl(&[VfFile::from_path(&path)])
        .expect("remove authenticated fixture");
}
