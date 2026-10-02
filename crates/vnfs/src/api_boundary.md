## Application boundary

Application clients and their handles are opaque. Backend construction and
protocol conversions live under `vnfs::backend` or in the backend crates.
There is no public backend extraction, lock access, or representation coercion.

```compile_fail,E0599
fn extract(client: vnfs::NfsClient) { let _ = client.into_inner(); }
```

```compile_fail,E0599
fn construct(backend: vnfs::backend::NfsVecFs) { let _ = vnfs::NfsClient::new(backend); }
```

```compile_fail,E0599
fn connect() { let _ = vnfs::Nfs::builder("server").connect_backend(); }
```

```compile_fail,E0616
fn representation(client: vnfs::NfsClient) { let _ = client.inner; }
```

```compile_fail,E0599
fn lock(client: &vnfs::NfsClient) { let _ = client.lock(); }
```

```compile_fail,E0432
use vnfs::{FsClient, FsFile, FsDir, FsRead, FsWrite, SetMetadata};
```

```compile_fail,E0599
fn conversion() { let _ = vnfs::OpenFlags::READ.to_libc(); }
```

```compile_fail,E0599
fn conversion() { let _ = vnfs::FileType::from_nfs(1); }
```

```compile_fail,E0599
fn conversion() { let _ = vnfs::FileType::Regular.as_nfs(); }
```

```compile_fail,E0599
fn conversion(error: vnfs::backend::RpcError) { let _ = vnfs::Error::from_rpc(error, None); }
```

```compile_fail,E0277
fn conversion(attrs: vnfs::backend::VfAttrs) -> vnfs::Metadata { attrs.into() }
```

```compile_fail,E0277
fn conversion(result: vnfs::backend::ReadResult) -> vnfs::ReadResult { result.into() }
```

```compile_fail,E0277
fn coercion(file: &vnfs::NfsFile) -> &vnfs::backend::FsFile<vnfs::backend::NfsVecFs> { file }
```

Explicit backend users can still opt into the low-level layer:

```compile_fail,E0308
fn coercion(client: &vnfs::NfsClient) -> &vnfs::backend::FsClient<vnfs::backend::NfsVecFs> { client }
```

```compile_fail,E0277
fn conversion(result: vnfs::backend::WriteResult) -> vnfs::WriteResult { result.into() }
```

```compile_fail,E0599
fn descriptor(file: &vnfs::NfsFile) { let _ = file.descriptor(); }
```

```compile_fail,E0505
fn borrowed(file: vnfs::NfsFile) {
    let request = file.read_request_at(0, 1);
    drop(file);
    drop(request);
}
```

```compile_fail,E0308
fn mounted(client: &vnfs::Mounted) -> &vnfs::backend::FsClient<vnfs::backend::DummyVecFs> { client }
```

```no_run
let backend = vnfs::backend::NfsVecFs::connect("server")?;
let client = vnfs::backend::FsClient::new(backend);
let _ = client.into_inner();
# Ok::<(), vnfs::Error>(())
```
