# Optional Kerberos authentication

AUTH_SYS is the compatibility default. It carries numeric UID/GID credentials
without cryptographic peer authentication, payload integrity, or privacy.
Use it only on trusted networks with appropriate server export policies.

## Enable RPCSEC_GSS

```toml
vnfs = { version = "0.0.20", features = ["rpcsec-gss"] }
```

Install `libkrb5-dev` in addition to the Linux build prerequisites. Configure a
Kerberos realm, obtain credentials with `kinit`, and configure the NFS server
with the matching export security flavor and service key. The client uses the
process's default GSS credential cache, not a supplied password.

```rust,no_run
# #[cfg(feature = "rpcsec-gss")]
# fn main() -> vnfs::Result<()> {
use vnfs::nfs::{Nfs, NfsAuthentication, RpcsecGssProtection};

let fs = Nfs::builder("nfs.example.com")
    .root("/export/application")
    .auth(NfsAuthentication::RpcsecGss {
        // Default host-based service name: nfs@nfs.example.com.
        service_principal: None,
        protection: RpcsecGssProtection::Integrity,
    })
    .connect()?;
drop(fs);
# Ok(())
# }
# #[cfg(not(feature = "rpcsec-gss"))]
# fn main() {}
```

`Authentication` corresponds to `sec=krb5`; `Integrity` corresponds to
`sec=krb5i` and is the recommended baseline. Supply an explicit host-based
service name if the server's identity differs from `nfs@<host>`.

Requested RPCSEC_GSS **fails rather than downgrading to AUTH_SYS**. Reconnects
reuse the authentication configuration and obtain fresh credentials from the
current process cache. Privacy (`krb5p`) is not exposed by the current API;
neither mode above encrypts file contents. RPC-over-TLS is also unsupported.

Mount discovery supports `sec=sys`, not Kerberos mounts. Configure a direct
connection explicitly for RPCSEC_GSS; `Auto` leaves Kerberos mounts on the
kernel route. See [mount routing](crate::nfs).
