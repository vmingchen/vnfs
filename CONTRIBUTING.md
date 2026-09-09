# Contributing

Thanks for improving VFSI. Bug reports should include the backend and protocol
version, client/server operating systems, the smallest reproducer available,
and whether the failure also occurs with the dummy backend. Never include
passwords, Kerberos material, packet captures containing credentials, or other
secrets in a public issue.

For code changes:

1. Open an issue first for protocol or public-API changes so compatibility and
   wire-semantics impact can be discussed.
2. Add a regression test at the lowest useful layer. Python behavior should
   include an `fsspec` contract test when applicable; protocol behavior should
   include NFS or Samba integration coverage.
3. Run `cargo fmt --all --check`, Clippy with warnings denied, the Rust unit
   tests, and `python -m pytest python/tests`.
4. Keep commits focused and explain round-trip, compatibility, and retry or
   idempotency implications in the pull request.

The full CI suite requires Linux native build dependencies and starts local
NFS-Ganesha and Samba servers. A pull request may rely on GitHub Actions for
those integration jobs.

By contributing, you agree that your work is licensed under the repository's
dual MIT/Apache-2.0 terms.
