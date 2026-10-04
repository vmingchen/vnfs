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

```compile_fail,E0308
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
    let request = vnfs::ReadOp::range(&file, 0, 1);
    drop(file);
    drop(request);
}
```

```compile_fail,E0382
fn consumed(client: &vnfs::NfsClient, file: &vnfs::NfsFile) {
    let ops = [vnfs::ReadOp::range(file, 0, 1)];
    let _ = client.readv(ops);
    let _ = client.readv(ops);
}
```

```compile_fail
fn exclusive(file: &vnfs::NfsFile) {
    let mut buffer = [0; 4];
    let first = vnfs::ReadOp::into(file, 0, &mut buffer);
    let second = vnfs::ReadOp::into(file, 4, &mut buffer);
    drop((first, second));
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

## Core versus extension methods

`Fs` contains vectorized filesystem operations and policy inspection.
Import `FsExt` to use scalar operations and convenience workflows. Its blanket
implementation applies to every `Fs`: generic code still needs only an `Fs` bound.

Single-target helpers use `_one`; prefer their vector counterparts for cohorts.
The historical unsuffixed scalar names are not extension methods.

```compile_fail,E0599
use vnfs::{Fs, FsExt};
fn old_open(fs: &impl Fs) { let _ = fs.open("/file"); }
```

```compile_fail,E0599
use vnfs::{Fs, FsExt};
fn old_listing(fs: &impl Fs) { let _ = fs.read_dir("/dir"); }
```

```compile_fail,E0599
use vnfs::{Fs, FsExt};
fn old_write(fs: &impl Fs) { let _ = fs.write("/file", b"data"); }
```

```compile_fail
use vnfs::Fs;
fn missing_extension(fs: &impl Fs) {
    let _ = fs.read_files(&["/file-1"]);
}
```

```no_run
use vnfs::{Fs, FsExt};
fn convenience(fs: &impl Fs) -> vnfs::Result<Vec<Vec<u8>>> {
    fs.read_files(&["/file-1", "/file-2"])
}
```

Portable writes retain their handle/payload borrows and do not expose backend
request types or a write-request associated type.

```compile_fail,E0505
use vnfs::{Fs, FsExt, WriteOp};
fn borrowed(fs: &impl Fs) {
    let file = fs.create_one("/output").unwrap();
    let ops = [WriteOp::at(&file, 0, b"hello")];
    drop(file);
    let _ = fs.writev(&ops);
}
```

```compile_fail,E0432
use vnfs::{NfsWrite, MountedWrite, AutoWrite};
```

The filesystem traits are named `Fs` and `FsExt`; no historical client-trait
aliases are exported.

```compile_fail,E0432
use vnfs::Client;
```

```compile_fail,E0432
use vnfs::ClientExt;
```

Each single-target execution method belongs to `FsExt`, not `Fs`.

```compile_fail,E0599
use vnfs::Fs;
fn open_with(fs: &impl Fs) {
    let _ = fs.open_with_one(vnfs::OpenRequest::new("/file", vnfs::OpenFlags::READ));
}
```

```compile_fail,E0599
use vnfs::Fs;
fn create_dir_all(fs: &impl Fs) {
    let _ = fs.create_dir_all_one("/dir");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn remove_file(fs: &impl Fs) {
    let _ = fs.remove_file_one("/file");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn remove_dir(fs: &impl Fs) {
    let _ = fs.remove_dir_one("/dir");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn remove_dir_all(fs: &impl Fs) {
    let _ = fs.remove_dir_all_one("/dir");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn remove_dir_contents(fs: &impl Fs) {
    let _ = fs.remove_dir_contents_one("/dir");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn rename(fs: &impl Fs) {
    let _ = fs.rename_one("/old", "/new");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn walk_with_options(fs: &impl Fs) {
    let _ = fs.walk_with_options_one("/", vnfs::MetadataFields::MODE, vnfs::WalkOptions::new());
}
```

```compile_fail,E0599
use vnfs::Fs;
fn visit_walk_with_options(fs: &impl Fs) {
    let _ = fs.visit_walk_with_options_one("/", vnfs::WalkOptions::new(), |_| Ok(std::ops::ControlFlow::Continue(())));
}
```

```compile_fail,E0599
use vnfs::Fs;
fn visit_dir_with_options(fs: &impl Fs) {
    let _ = fs.visit_dir_with_options_one("/", vnfs::ReadDirOptions::new(), |_| Ok(std::ops::ControlFlow::Continue(())));
}
```

```compile_fail,E0599
use vnfs::Fs;
fn read_stream_with_options(fs: &impl Fs) {
    let _ = fs.read_stream_with_options_one("/file", vnfs::ReadStreamOptions::new(), |_, _| Ok(true));
}
```

Vector-only generic code needs no extension trait:

```no_run
use vnfs::{Fs, ReadOp};
fn vector_only(fs: &impl Fs) -> vnfs::Result<Vec<vnfs::ReadResult>> {
    fs.readv([ReadOp::whole("/file-1"), ReadOp::whole("/file-2")])
}
```
