## Application boundary

Collection and visiting share `VisitOptions`; separate recursive collection
and concrete-client helper entry points are not retained:

```compile_fail,E0599
use vnfs::{Fs, FsExt};
fn old_collect(fs: &impl Fs) {
    let _ = fs.walks_with_options(&["/tree"], vnfs::MetadataFields::MODE, vnfs::WalkOptions::new());
}
```

```compile_fail,E0599
fn inherent_open(client: &vnfs::NfsClient) {
    // Without FsExt in scope there is no separate inherent implementation.
    let _ = client.open_with(vnfs::OpenRequest::new("/file", vnfs::OpenFlags::READ));
}
```

```compile_fail,E0599
fn inherent_read(client: &vnfs::NfsClient) {
    let _ = client.readv([vnfs::ReadOp::whole("/file")]);
}
```

```compile_fail
use vnfs::application::NativeHooks;
```

Shallow and recursive visits share one primitive, with borrowed entries:

```compile_fail,E0599
use vnfs::Fs;
fn old_walk(fs: &impl Fs) {
    let _ = fs.visit_walks_with_options(&["/tree"], vnfs::WalkOptions::new(),
        |_, _| Ok(vnfs::ControlFlow::Continue(())));
}
```

Default-policy read/write helpers belong to `FsExt`, not the backend contract.
Generic callers import the extension explicitly:

```compile_fail,E0599
use vnfs::Fs;
fn read(fs: &impl Fs) { let _ = fs.readv([vnfs::ReadOp::whole("/file")]); }
```

```compile_fail,E0599
use vnfs::Fs;
fn write(fs: &impl Fs) { let _ = fs.writev(&[]); }
```

Removal uses one explicit-mode vector primitive, not separate trait methods:

```compile_fail,E0599
use vnfs::Fs;
fn remove(fs: &impl Fs) { let _ = fs.remove_dirs_contents(&["/dir"]); }
```

Application clients and their handles are opaque. Backend construction and
protocol conversions live under `vnfs::backend` or in the backend crates.
There is no public backend extraction, lock access, or representation coercion.

```compile_fail,E0599
fn extract(client: vnfs::NfsClient) { let _ = client.into_inner(); }
```

```compile_fail,E0599
fn construct(backend: vfsi_nfs::NfsVecFs) { let _ = vnfs::NfsClient::new(backend); }
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
fn conversion(error: vfsi_sync::RpcError) { let _ = vnfs::Error::from_rpc(error, None); }
```

```compile_fail,E0277
fn conversion(attrs: vfsi_sync::VfAttrs) -> vnfs::Metadata { attrs.into() }
```

```compile_fail,E0277
fn conversion(result: vfsi_core::ReadResult) -> vnfs::ReadResult { result.into() }
```

```compile_fail,E0308
fn coercion(file: &vnfs::NfsFile) -> &vfsi_sync::FsFile<vfsi_nfs::NfsVecFs> { file }
```

Explicit backend users can still opt into the low-level layer:

```compile_fail,E0308
fn coercion(client: &vnfs::NfsClient) -> &vfsi_sync::FsClient<vfsi_nfs::NfsVecFs> { client }
```

```compile_fail,E0277
fn conversion(result: vfsi_sync::WriteResult) -> vnfs::WriteResult { result.into() }
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
fn mounted(client: &vnfs::Mounted) -> &vfsi_sync::FsClient<vfsi_local::DummyVecFs> { client }
```

```no_run
let backend = vfsi_nfs::NfsVecFs::connect("server")?;
let client = vfsi_sync::FsClient::new(backend);
let _ = client.into_inner();
# Ok::<(), vnfs::Error>(())
```

## Core versus extension methods

Write completion is selected by `WriteOptions`, not a separate public method.

```compile_fail,E0599
use vnfs::Fs;
fn old_complete(fs: &impl Fs) {
    let _ = fs.write_allv(&[]);
}
```

```compile_fail,E0599
fn old_concrete_complete(fs: &vnfs::NfsClient) { let _ = fs.write_allv(&[]); }
```

Namespace vectors use `vrename`, `vcopy`, and `vmkdir`; historical descriptive
names are not retained as aliases.

```compile_fail,E0599
use vnfs::Fs;
fn old_rename(fs: &impl Fs) { let _ = fs.rename_files(&[("/a", "/b")]); }
```

```compile_fail,E0599
use vnfs::Fs;
fn old_copy(fs: &impl Fs) { let _ = fs.copy_files(&[("/a", "/b")]); }
```

```compile_fail,E0599
use vnfs::Fs;
fn old_mkdir(fs: &impl Fs) { let _ = fs.create_dirs(&["/a", "/b"]); }
```

`Fs` contains vectorized filesystem operations and policy inspection.
Import `FsExt` to use scalar operations and convenience workflows. Its blanket
implementation applies to every `Fs`: generic code still needs only an `Fs` bound.

Single-target helpers use conventional names; prefer their vector counterparts for cohorts.

The previous suffixed application names are not retained as aliases:

```compile_fail,E0599
use vnfs::{Fs, FsExt};
fn old_open(fs: &impl Fs) { let _ = fs.open_one("/file"); }
```
Conventional scalar names are extensions requiring only an `Fs` bound:

```no_run
use vnfs::{Fs, FsExt};
fn scalar_workflow(fs: &impl Fs) -> vnfs::Result<()> {
    fs.write("/file", b"data")?;
    let file = fs.open("/file")?;
    let _ = fs.read_dir("/")?;
    fs.closev(vec![file])
}
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
    let file = fs.create("/output").unwrap();
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
    let _ = fs.open_with(vnfs::OpenRequest::new("/file", vnfs::OpenFlags::READ));
}
```

```compile_fail,E0599
use vnfs::Fs;
fn create_dir_all(fs: &impl Fs) {
    let _ = fs.create_dir_all("/dir");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn remove_file(fs: &impl Fs) {
    let _ = fs.remove_file("/file");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn remove_dir(fs: &impl Fs) {
    let _ = fs.remove_dir("/dir");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn remove_dir_all(fs: &impl Fs) {
    let _ = fs.remove_dir_all("/dir");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn remove_dir_contents(fs: &impl Fs) {
    let _ = fs.remove_dir_contents("/dir");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn rename(fs: &impl Fs) {
    let _ = fs.rename("/old", "/new");
}
```

```compile_fail,E0599
use vnfs::Fs;
fn walk_with_options(fs: &impl Fs) {
    let _ = fs.walk_with_options("/", vnfs::MetadataFields::MODE, vnfs::WalkOptions::new());
}
```

```compile_fail,E0599
use vnfs::Fs;
fn visit_walk_with_options(fs: &impl Fs) {
    let _ = fs.visit_walk_with_options("/", vnfs::VisitOptions::new(), |_| Ok(std::ops::ControlFlow::Continue(())));
}
```

```compile_fail,E0599
use vnfs::Fs;
fn visit_dir_with_options(fs: &impl Fs) {
    let _ = fs.visit_dir_with_options("/", vnfs::VisitOptions::new(), |_| Ok(std::ops::ControlFlow::Continue(())));
}
```

```compile_fail,E0599
use vnfs::Fs;
fn read_stream_with_options(fs: &impl Fs) {
    let _ = fs.read_stream_with_options("/file", vnfs::ReadStreamOptions::new(), |_, _| Ok(true));
}
```

Vector-only generic code needs no extension trait:

```no_run
use vnfs::{Fs, ReadOp, ReadOptions};
fn vector_only(fs: &impl Fs) -> vnfs::Result<Vec<vnfs::ReadResult>> {
    fs.vread([ReadOp::whole("/file-1"), ReadOp::whole("/file-2")], ReadOptions::new())
}
```


Backend construction is not republished by the application crate:

```compile_fail
use vnfs::backend::rpc::RpcClient;
```

Read-result fields cannot be independently mutated:

```compile_fail
let mut result = vnfs::ReadResult::owned(0, vec![1], true);
result.read = 100;
```
