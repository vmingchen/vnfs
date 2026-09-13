#![cfg(feature = "rpcsec-gss")]

use std::time::Duration;

use vnfs::{
    NfsAuthentication, NfsConnectOptions, NfsVecFs, ReadOp, RpcsecGssProtection, VecFs, VfFile,
    VfOffset, WriteOp,
};

fn options(protection: RpcsecGssProtection) -> NfsConnectOptions {
    NfsConnectOptions {
        minorversion: Some(1),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        authentication: NfsAuthentication::RpcsecGss {
            service_principal: Some(
                std::env::var("VNFS_GSS_SERVICE").unwrap_or_else(|_| "nfs@localhost".into()),
            ),
            protection,
        },
    }
}

#[test]
fn supported_rpcsec_gss_protection_levels_access_the_export() {
    if std::env::var_os("VNFS_GSS_INTEGRATION").is_none() {
        eprintln!("set VNFS_GSS_INTEGRATION=1 to run against a Kerberos export");
        return;
    }

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
        fs.writev(
            &[WriteOp::from_path(&path, VfOffset::At(0), payload.clone())
                .with_creation()
                .with_truncate()],
        )
        .unwrap_or_else(|error| panic!("write using {name}: {error}"));
        let result = fs
            .readv(&[ReadOp::from_path(&path, VfOffset::At(0), payload.len())])
            .unwrap_or_else(|error| panic!("read using {name}: {error}"));
        assert_eq!(result[0].data, payload, "payload using {name}");
        fs.removev(&[VfFile::from_path(&path)])
            .unwrap_or_else(|error| panic!("remove using {name}: {error}"));
    }
}

#[test]
fn auth_sys_is_not_silently_upgraded_or_used_as_a_gss_fallback() {
    if std::env::var_os("VNFS_GSS_INTEGRATION").is_none() {
        eprintln!("set VNFS_GSS_INTEGRATION=1 to run against a Kerberos export");
        return;
    }

    let host = std::env::var("VNFS_GSS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let error = NfsVecFs::connect_with_options(
        &host,
        NfsConnectOptions {
            minorversion: Some(1),
            ..NfsConnectOptions::default()
        },
    )
    .err()
    .expect("a GSS-only export must reject AUTH_SYS");
    assert!(
        !error.is_transport(),
        "server should reject AUTH_SYS: {error}"
    );
}
