## Application boundary

Attrs vectors use the canonical options-aware primitive; former aliases
are not retained, even when the extension trait is imported:

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn old_metadata(fs: &impl Vfsi) { let _ = fs.metadatav(&["/file"]); }
```

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn old_symlink_metadata(fs: &impl Vfsi) { let _ = fs.symlink_metadatav(&["/link"]); }
```

Consuming cleanup is named `close_files`, distinct from borrowing `vclose`:

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn old_close<C: Vfsi>(fs: &C, files: Vec<C::File>) {
    let _ = fs.closev(files);
}
```

Collection and visiting share `ListDirOptions`; separate recursive collection
and concrete-client helper entry points are not retained:

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn old_collect(fs: &impl Vfsi) {
    let _ = fs.walks_with_options(&["/tree"], vnfs::directory::Attributes::MODE, vnfs::directory::ListDirOptions::new().recursive(true));
}
```

```compile_fail,E0599
fn inherent_open(client: &vnfs::nfs::NfsClient) {
    // Without VfsiExt in scope there is no separate inherent implementation.
    let _ = client.open_with(vnfs::files::OpenOp::new("/file", vnfs::files::OpenFlags::READ));
}
```

```compile_fail,E0599
fn inherent_read(client: &vnfs::nfs::NfsClient) {
    let _ = client.readv([vnfs::files::ReadOp::whole("/file")]);
}
```

Shallow and recursive visits share one primitive, with borrowed entries:

```compile_fail,E0599
use vnfs::files::Vfsi;
fn old_walk(fs: &impl Vfsi) {
    let _ = fs.visit_walks_with_options(&["/tree"], vnfs::directory::ListDirOptions::new().recursive(true),
        |_, _| Ok(vnfs::directory::ControlFlow::Continue(())));
}
```

Vector reads, writes, and removal have one canonical options-aware entry point.
The former default-options aliases are not retained, even with `VfsiExt` in scope:

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn read(fs: &impl Vfsi) { let _ = fs.readv([vnfs::files::ReadOp::whole("/file")]); }
```

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn write(fs: &impl Vfsi) { let _ = fs.writev(&[]); }
```

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
use vnfs::directory::{RemoveMode};
fn remove(fs: &impl Vfsi) { let _ = fs.removev(&["/tree"], RemoveMode::Tree); }
```

Removal uses one explicit-mode vector primitive, not separate trait methods:

```compile_fail,E0599
use vnfs::files::Vfsi;
fn remove(fs: &impl Vfsi) { let _ = fs.remove_dirs_contents(&["/dir"]); }
```

Application clients and their handles are opaque. Backend construction and
protocol conversions live under `vnfs::backend` or in the backend crates.
There is no public backend extraction, lock access, or representation coercion.

```compile_fail,E0599
fn extract(client: vnfs::nfs::NfsClient) { let _ = client.into_inner(); }
```

```compile_fail,E0599
fn construct(backend: vfsi_nfs::NfsVecFs) { let _ = vnfs::nfs::NfsClient::new(backend); }
```

```compile_fail,E0599
fn connect() { let _ = vnfs::nfs::Nfs::builder("server").connect_backend(); }
```

```compile_fail,E0616
fn representation(client: vnfs::nfs::NfsClient) { let _ = client.inner; }
```

```compile_fail,E0599
fn lock(client: &vnfs::nfs::NfsClient) { let _ = client.lock(); }
```

```compile_fail,E0432
use vnfs::FsClient;
use vnfs::FsFile;
use vnfs::FsDir;
use vnfs::FsRead;
use vnfs::FsWrite;
```

```compile_fail,E0599
fn conversion() { let _ = vnfs::files::OpenFlags::READ.to_libc(); }
```

```compile_fail,E0599
fn conversion() { let _ = vnfs::directory::FileType::from_nfs(1); }
```

```compile_fail,E0599
fn conversion() { let _ = vnfs::directory::FileType::Regular.as_nfs(); }
```

```compile_fail,E0599
fn conversion(error: vfsi_sync::RpcError) { let _ = vnfs::Error::from_rpc(error, None); }
```

```compile_fail,E0277
fn conversion(attrs: vfsi_sync::VfAttrs) -> vnfs::directory::Attrs { attrs.into() }
```

```compile_fail,E0277
fn conversion(result: vfsi_core::ReadResult) -> vnfs::files::ReadResult { result.into() }
```

```compile_fail,E0308
fn coercion(file: &vnfs::nfs::NfsFile) -> &vfsi_sync::FsFile<vfsi_nfs::NfsVecFs> { file }
```

Explicit backend users can still opt into the low-level layer:

```compile_fail,E0308
fn coercion(client: &vnfs::nfs::NfsClient) -> &vfsi_sync::FsClient<vfsi_nfs::NfsVecFs> { client }
```

```compile_fail,E0277
fn conversion(result: vfsi_sync::WriteResult) -> vnfs::files::WriteResult { result.into() }
```

```compile_fail,E0599
fn descriptor(file: &vnfs::nfs::NfsFile) { let _ = file.descriptor(); }
```

```compile_fail,E0505
fn borrowed(file: vnfs::nfs::NfsFile) {
    let request = vnfs::files::ReadOp::range(&file, 0, 1);
    drop(file);
    drop(request);
}
```

```compile_fail,E0382
use vnfs::files::Vfsi;
fn consumed(client: &vnfs::nfs::NfsClient, file: &vnfs::nfs::NfsFile) {
    let ops = [vnfs::files::ReadOp::range(file, 0, 1)];
    let _ = client.vread(ops, Default::default());
    let _ = client.vread(ops, Default::default());
}
```

```compile_fail
fn exclusive(file: &vnfs::nfs::NfsFile) {
    let mut buffer = [0; 4];
    let first = vnfs::files::ReadOp::into(file, 0, &mut buffer);
    let second = vnfs::files::ReadOp::into(file, 4, &mut buffer);
    drop((first, second));
}
```

```compile_fail,E0308
fn mounted(client: &vnfs::posix::Posix) -> &vfsi_sync::FsClient<vfsi_local::LocalBackend> { client }
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
use vnfs::files::Vfsi;
fn old_complete(fs: &impl Vfsi) {
    let _ = fs.write_allv(&[]);
}
```

```compile_fail,E0599
fn old_concrete_complete(fs: &vnfs::nfs::NfsClient) { let _ = fs.write_allv(&[]); }
```

Namespace vectors use `vrename`, `vcopy`, and `vmkdir`; historical descriptive
names are not retained as aliases.

```compile_fail,E0599
use vnfs::files::Vfsi;
fn old_rename(fs: &impl Vfsi) { let _ = fs.rename_files(&[("/a", "/b")]); }
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn old_copy(fs: &impl Vfsi) { let _ = fs.copy_files(&[("/a", "/b")]); }
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn old_mkdir(fs: &impl Vfsi) { let _ = fs.create_dirs(&["/a", "/b"]); }
```

`Vfsi` contains vectorized filesystem operations and policy inspection.
Import `VfsiExt` to use scalar operations and convenience workflows. Its blanket
implementation applies to every `Vfsi`: generic code still needs only an `Vfsi` bound.

Single-target helpers use conventional names; prefer their vector counterparts for cohorts.

The previous suffixed application names are not retained as aliases:

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn old_open(fs: &impl Vfsi) { let _ = fs.open_one("/file"); }
```
Conventional scalar names are extensions requiring only an `Vfsi` bound:

```no_run
use vnfs::files::{Vfsi, VfsiExt};
fn scalar_workflow(fs: &impl Vfsi) -> vnfs::Result<()> {
    fs.write("/file", b"data")?;
    let file = fs.open("/file")?;
    let _ = fs.read_dir("/")?;
    fs.close_files(vec![file])
}
```

```compile_fail
use vnfs::files::Vfsi;
fn missing_extension(fs: &impl Vfsi) {
    let _ = fs.read_files(&["/file-1"]);
}
```

```no_run
use vnfs::files::{Vfsi, VfsiExt};
fn convenience(fs: &impl Vfsi) -> vnfs::Result<Vec<Vec<u8>>> {
    fs.read_files(&["/file-1", "/file-2"])
}
```

Portable writes retain their handle/payload borrows and do not expose backend
request types or a write-request associated type.

```compile_fail,E0505
use vnfs::files::{Vfsi, VfsiExt, WriteOp};
fn borrowed(fs: &impl Vfsi) {
    let file = fs.create("/output").unwrap();
    let ops = [WriteOp::at(&file, 0, b"hello")];
    drop(file);
    let _ = fs.vwrite(&ops, Default::default());
}
```

```compile_fail,E0432
use vnfs::NfsWrite;
use vnfs::MountedWrite;
use vnfs::AutoWrite;
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
use vnfs::files::Vfsi;
fn open_with(fs: &impl Vfsi) {
    let _ = fs.open_with(vnfs::files::OpenOp::new("/file", vnfs::files::OpenFlags::READ));
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn create_dir_all(fs: &impl Vfsi) {
    let _ = fs.create_dir_all("/dir");
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn remove_file(fs: &impl Vfsi) {
    let _ = fs.remove_file("/file");
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn remove_dir(fs: &impl Vfsi) {
    let _ = fs.remove_dir("/dir");
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn remove_dir_all(fs: &impl Vfsi) {
    let _ = fs.remove_dir_all("/dir");
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn remove_dir_contents(fs: &impl Vfsi) {
    let _ = fs.remove_dir_contents("/dir");
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn rename(fs: &impl Vfsi) {
    let _ = fs.rename("/old", "/new");
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn walk_with_options(fs: &impl Vfsi) {
    let _ = fs.walk_with_options("/", vnfs::directory::ListDirOptions::new().recursive(true).fields(vnfs::directory::Attributes::MODE));
}
```

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn visit_walk_with_options(fs: &impl Vfsi) {
    let _ = fs.visit_walk_with_options("/", vnfs::directory::ListDirOptions::new(), |_| Ok(std::ops::ControlFlow::Continue(())));
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn listdir(fs: &impl Vfsi) {
    let _ = fs.listdir("/", vnfs::directory::ListDirOptions::new(), |_| {
        Ok(vnfs::directory::WalkControl::Continue)
    });
}
```

```compile_fail,E0599
use vnfs::files::Vfsi;
fn read_stream_with_options(fs: &impl Vfsi) {
    let _ = fs.read_stream_with_options("/file", vnfs::files::StreamOptions::new(), |_, _| Ok(std::ops::ControlFlow::Continue(())));
}
```

Vector-only generic code needs no extension trait:

```no_run
use vnfs::files::{Vfsi, ReadOp, ReadOptions};
fn vector_only(fs: &impl Vfsi) -> vnfs::Result<Vec<vnfs::files::ReadResult>> {
    fs.vread([ReadOp::whole("/file-1"), ReadOp::whole("/file-2")], ReadOptions::new())
}
```


Backend construction is not republished by the application crate:

```compile_fail
use vnfs::backend::rpc::RpcClient;
```

Read-result fields cannot be independently mutated:

```compile_fail
let mut result = vnfs::files::ReadResult::owned(0, vec![1], true);
result.read = 100;
```

Filesystem operations require the portable traits; internal dispatch is private.

```compile_fail
fn read_without_trait(fs: &vnfs::nfs::NfsClient) {
    fs.vread([vnfs::files::ReadOp::path("/file")], vnfs::files::ReadOptions::new());
}
```

```compile_fail
fn internal_dispatch(fs: &vnfs::nfs::NfsClient) {
    fs.vopen_impl(&[]);
}
```

Removal dispatch stays private; applications use `Vfsi::vremove`.

```compile_fail
use vnfs::files::Vfsi;
fn removal_backend(fs: &vnfs::nfs::NfsClient) {
    fs.vremove_impl(&["/file"], false, Default::default());
}
```

`Auto` is the single routed client, with no public inner-client type.

```compile_fail
use vnfs::AutoClient;
```

Directory collection and traversal share one options type.

```compile_fail
use vnfs::ReadDirOptions;
use vnfs::WalkOptions;
```

File I/O requires an explicit client adapter; handles expose lifecycle only.

```compile_fail
fn read_direct(file: vnfs::nfs::NfsFile) {
    fn reader(_: impl std::io::Read) {}
    reader(file);
}
```

```compile_fail
fn metadata_direct(file: &vnfs::nfs::NfsFile) { let _ = file.attrs(); }
```

Destructive initialization is explicit.

```compile_fail
fn empty(fs: &vnfs::nfs::NfsClient) { let _ = fs.ensure_empty_dir("/data"); }
```


Portable reads have one result type and metadata updates use vector requests.

```compile_fail,E0603
use vnfs::ReadIntoResult;
```

```compile_fail,E0432
use vnfs::files::ReadIntoResult;
```

```compile_fail,E0432
use vnfs::SetMetadata;
```

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn update(fs: &impl Vfsi) { let _ = fs.set_metadata("/file"); }
```

Recursive collection uses `read_dirs_with_options`.

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn collect(fs: &impl Vfsi) { let _ = fs.walk("/"); }
```

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn collect(fs: &impl Vfsi) {
    let _ = fs.walk_with_options("/", vnfs::directory::ListDirOptions::new());
}
```

Portable writes do not expose native preparation storage or path creation.

```compile_fail,E0308
fn native_storage<H>(op: vnfs::files::WriteOp<'_, H>) {
    let _: vfsi_core::internal::WriteRequest<&H, &[u8], u64, ()> = op;
}
```

```compile_fail,E0599
fn prepare<H>(op: vnfs::files::WriteOp<'_, H>) { let _ = op.borrowed(); }
```

Policy fields are configured only through builders.

```compile_fail,E0616
let mut op = vnfs::files::OpenOp::new("/file", vnfs::files::OpenFlags::READ);
op.path = "/other".into();
```

```compile_fail,E0616
let mut limits = vnfs::files::ResourceLimits::new();
limits.max_read_bytes = 0;
```

```compile_fail,E0308
let _ = vnfs::files::ResourceLimits::new().stream_chunk_bytes(0);
```

```compile_fail,E0616
let mut options = vnfs::directory::RemoveOptions::new();
options.retries = 0;
```

```compile_fail,E0616
let mut policy = vnfs::nfs::NfsRecoveryPolicy::new();
policy.reconnect_attempts = 0;
```

Ordering policy compares immutable entries; it cannot replace validated paths:

```compile_fail,E0594
use vnfs::files::{Vfsi, VfsiExt};
use vnfs::directory::{DirEntry, ListDirOptions, WalkControl};
fn rewrite(fs: &impl Vfsi, replacement: DirEntry) {
    let _ = fs.visit_dirs_ordered("/", ListDirOptions::new(),
        |a, b| { *a = replacement.clone(); a.path().cmp(b.path()) },
        |_| true, |_, _| Ok(WalkControl::Continue));
}
```

Directory mutation is submitted to the client's portable vector engine:

```compile_fail,E0599
use vnfs::files::VfsiExt;
fn bypass_client(dir: &vnfs::nfs::NfsDir) { let _ = dir.remove_contents(); }
```

```compile_fail,E0599
fn inherent_directory_open(client: &vnfs::nfs::NfsClient) {
    let _ = client.open_dir_handle("/dir");
}
```

Path-only mapping cannot claim that an arbitrary client owns its namespace:

```compile_fail,E0432
use vnfs::helpers::MountSession;
```

Streaming callbacks use the same control type as directory visitors:

```compile_fail,E0308
use vnfs::files::{Vfsi, VfsiExt};
fn bool_callback(fs: &impl Vfsi) { let _ = fs.read_stream("/file", |_, _| Ok(false)); }
```

Standard I/O has one opaque adapter, obtained through `VfsiExt::std_io`.
The old method, public adapter types, and native borrowed builder are absent:

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn old_adapter<C: Vfsi>(fs: &C, file: &C::File) { let _ = fs.file_io(file); }
```

```compile_fail,E0432
use vnfs::FileIo;
```

```compile_fail,E0432
use vfsi_core::api::FileIo;
```

```compile_fail,E0432
use vnfs::StdIo;
```

```compile_fail,E0432
use vfsi_sync::VfOpenOptions;
```

```compile_fail,E0432
use vfsi_sync::VfFileHandle;
```

```compile_fail,E0599
use vnfs::files::{Vfsi, VfsiExt};
fn custom_cursor<C: Vfsi>(fs: &C, file: &C::File) {
    let _ = fs.std_io(file).position();
}
```

Native owned handles provide ownership and cleanup; I/O uses the client:

```compile_fail,E0277
fn read<F: vfsi_sync::backend::HandleBackend>(file: &mut vfsi_sync::FsFile<F>) {
    let _ = std::io::Read::read(file, &mut [0]);
}
```

```compile_fail,E0277
fn write<F: vfsi_sync::backend::HandleBackend>(file: &mut vfsi_sync::FsFile<F>) {
    let _ = std::io::Write::write(file, b"data");
}
```

```compile_fail,E0277
fn seek<F: vfsi_sync::backend::HandleBackend>(file: &mut vfsi_sync::FsFile<F>) {
    let _ = std::io::Seek::seek(file, std::io::SeekFrom::Start(0));
}
```

```compile_fail,E0599
fn positional_read<F: vfsi_sync::backend::HandleBackend>(file: &vfsi_sync::FsFile<F>) {
    let _ = file.read_at(&mut [0], 0);
}
```

The open builder is supplied by `VfsiExt`, with no inherent native alternative:

```compile_fail,E0599
fn native_builder<F: vfsi_sync::backend::VectorBackend + 'static>(client: &vfsi_sync::FsClient<F>) {
    let _ = client.open_options();
}
```
