## Application boundary

Attrs vectors use the canonical options-aware primitive; former aliases
are not retained, even when the extension trait is imported:

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt};
fn old_metadata(fs: &impl Vfsi) { let _ = fs.metadatav(&["/file"]); }
```

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt};
fn old_symlink_metadata(fs: &impl Vfsi) { let _ = fs.symlink_metadatav(&["/link"]); }
```

Consuming cleanup is named `close_files`, distinct from borrowing `vclose`:

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt};
fn old_close<C: Vfsi>(fs: &C, files: Vec<C::File>) {
    let _ = fs.closev(files);
}
```

Collection and visiting share `ListDirOptions`; separate recursive collection
and concrete-client helper entry points are not retained:

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt};
fn old_collect(fs: &impl Vfsi) {
    let _ = fs.walks_with_options(&["/tree"], vnfs::Attributes::MODE, vnfs::WalkOptions::new());
}
```

```compile_fail,E0599
fn inherent_open(client: &vnfs::NfsClient) {
    // Without VfsiExt in scope there is no separate inherent implementation.
    let _ = client.open_with(vnfs::OpenOp::new("/file", vnfs::OpenFlags::READ));
}
```

```compile_fail,E0599
fn inherent_read(client: &vnfs::NfsClient) {
    let _ = client.readv([vnfs::ReadOp::whole("/file")]);
}
```

Shallow and recursive visits share one primitive, with borrowed entries:

```compile_fail,E0599
use vnfs::Vfsi;
fn old_walk(fs: &impl Vfsi) {
    let _ = fs.visit_walks_with_options(&["/tree"], vnfs::WalkOptions::new(),
        |_, _| Ok(vnfs::ControlFlow::Continue(())));
}
```

Vector reads, writes, and removal have one canonical options-aware entry point.
The former default-options aliases are not retained, even with `VfsiExt` in scope:

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt};
fn read(fs: &impl Vfsi) { let _ = fs.readv([vnfs::ReadOp::whole("/file")]); }
```

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt};
fn write(fs: &impl Vfsi) { let _ = fs.writev(&[]); }
```

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt, RemoveMode};
fn remove(fs: &impl Vfsi) { let _ = fs.removev(&["/tree"], RemoveMode::Tree); }
```

Removal uses one explicit-mode vector primitive, not separate trait methods:

```compile_fail,E0599
use vnfs::Vfsi;
fn remove(fs: &impl Vfsi) { let _ = fs.remove_dirs_contents(&["/dir"]); }
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
fn conversion(attrs: vfsi_sync::VfAttrs) -> vnfs::Attrs { attrs.into() }
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
    let _ = client.vread(ops, Default::default());
    let _ = client.vread(ops, Default::default());
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
use vnfs::Vfsi;
fn old_complete(fs: &impl Vfsi) {
    let _ = fs.write_allv(&[]);
}
```

```compile_fail,E0599
fn old_concrete_complete(fs: &vnfs::NfsClient) { let _ = fs.write_allv(&[]); }
```

Namespace vectors use `vrename`, `vcopy`, and `vmkdir`; historical descriptive
names are not retained as aliases.

```compile_fail,E0599
use vnfs::Vfsi;
fn old_rename(fs: &impl Vfsi) { let _ = fs.rename_files(&[("/a", "/b")]); }
```

```compile_fail,E0599
use vnfs::Vfsi;
fn old_copy(fs: &impl Vfsi) { let _ = fs.copy_files(&[("/a", "/b")]); }
```

```compile_fail,E0599
use vnfs::Vfsi;
fn old_mkdir(fs: &impl Vfsi) { let _ = fs.create_dirs(&["/a", "/b"]); }
```

`Vfsi` contains vectorized filesystem operations and policy inspection.
Import `VfsiExt` to use scalar operations and convenience workflows. Its blanket
implementation applies to every `Vfsi`: generic code still needs only an `Vfsi` bound.

Single-target helpers use conventional names; prefer their vector counterparts for cohorts.

The previous suffixed application names are not retained as aliases:

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt};
fn old_open(fs: &impl Vfsi) { let _ = fs.open_one("/file"); }
```
Conventional scalar names are extensions requiring only an `Vfsi` bound:

```no_run
use vnfs::{Vfsi, VfsiExt};
fn scalar_workflow(fs: &impl Vfsi) -> vnfs::Result<()> {
    fs.write("/file", b"data")?;
    let file = fs.open("/file")?;
    let _ = fs.read_dir("/")?;
    fs.close_files(vec![file])
}
```

```compile_fail
use vnfs::Vfsi;
fn missing_extension(fs: &impl Vfsi) {
    let _ = fs.read_files(&["/file-1"]);
}
```

```no_run
use vnfs::{Vfsi, VfsiExt};
fn convenience(fs: &impl Vfsi) -> vnfs::Result<Vec<Vec<u8>>> {
    fs.read_files(&["/file-1", "/file-2"])
}
```

Portable writes retain their handle/payload borrows and do not expose backend
request types or a write-request associated type.

```compile_fail,E0505
use vnfs::{Vfsi, VfsiExt, WriteOp};
fn borrowed(fs: &impl Vfsi) {
    let file = fs.create("/output").unwrap();
    let ops = [WriteOp::at(&file, 0, b"hello")];
    drop(file);
    let _ = fs.vwrite(&ops, Default::default());
}
```

```compile_fail,E0432
use vnfs::{NfsWrite, MountedWrite, AutoWrite};
```

The filesystem traits are named `Vfsi` and `VfsiExt`; no historical client-trait
aliases are exported.

```compile_fail,E0432
use vnfs::Client;
```

```compile_fail,E0432
use vnfs::ClientExt;
```

Each single-target execution method belongs to `VfsiExt`, not `Vfsi`.

```compile_fail,E0599
use vnfs::Vfsi;
fn open_with(fs: &impl Vfsi) {
    let _ = fs.open_with(vnfs::OpenOp::new("/file", vnfs::OpenFlags::READ));
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn create_dir_all(fs: &impl Vfsi) {
    let _ = fs.create_dir_all("/dir");
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn remove_file(fs: &impl Vfsi) {
    let _ = fs.remove_file("/file");
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn remove_dir(fs: &impl Vfsi) {
    let _ = fs.remove_dir("/dir");
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn remove_dir_all(fs: &impl Vfsi) {
    let _ = fs.remove_dir_all("/dir");
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn remove_dir_contents(fs: &impl Vfsi) {
    let _ = fs.remove_dir_contents("/dir");
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn rename(fs: &impl Vfsi) {
    let _ = fs.rename("/old", "/new");
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn walk_with_options(fs: &impl Vfsi) {
    let _ = fs.walk_with_options("/", vnfs::ListDirOptions::from(vnfs::WalkOptions::new()).fields(vnfs::Attributes::MODE));
}
```

```compile_fail,E0599
use vnfs::{Vfsi, VfsiExt};
fn visit_walk_with_options(fs: &impl Vfsi) {
    let _ = fs.visit_walk_with_options("/", vnfs::ListDirOptions::new(), |_| Ok(std::ops::ControlFlow::Continue(())));
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn listdir(fs: &impl Vfsi) {
    let _ = fs.listdir("/", vnfs::ListDirOptions::new(), |_| {
        Ok(vnfs::WalkControl::Continue)
    });
}
```

```compile_fail,E0599
use vnfs::Vfsi;
fn read_stream_with_options(fs: &impl Vfsi) {
    let _ = fs.read_stream_with_options("/file", vnfs::StreamOptions::new(), |_, _| Ok(true));
}
```

Vector-only generic code needs no extension trait:

```no_run
use vnfs::{Vfsi, ReadOp, ReadOptions};
fn vector_only(fs: &impl Vfsi) -> vnfs::Result<Vec<vnfs::ReadResult>> {
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

Filesystem operations require the portable traits; internal dispatch is private.

```compile_fail
fn read_without_trait(fs: &vnfs::NfsClient) {
    fs.vread([vnfs::ReadOp::path("/file")], vnfs::ReadOptions::new());
}
```

```compile_fail
fn internal_dispatch(fs: &vnfs::NfsClient) {
    fs.vopen_impl(&[]);
}
```

Removal dispatch stays private; applications use `Vfsi::vremove`.

```compile_fail
use vnfs::Vfsi;
fn removal_backend(fs: &vnfs::NfsClient) {
    fs.vremove_impl(&["/file"], false, Default::default());
}
```
