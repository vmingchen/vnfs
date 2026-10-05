//! Native and high-level NFS integration tests. The server defaults to
//! 127.0.0.1 with export `/`; configure VFSI_NFS_SERVER and VFSI_NFS_EXPORT
//! for another writable fixture. Select NFSv4.1 or v4.2 with VNFS_TEST_MINOR.
//!
//! ```sh
//! cargo test -p vnfs --test nfs --all-features -- --test-threads=1
//! ```

use nfsv41_sys::nfsstat4_NFS4ERR_EXIST;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use vfsi_nfs::NfsVecFs;
use vfsi_nfs::nfs::*;
use vnfs::VfsiExt;
use vnfs::{Nfs, NfsReadPoolOptions};

#[cfg(feature = "rpcsec-gss")]
#[test]
fn failed_gss_negotiation_does_not_fall_back_to_a_working_auth_sys_export() {
    // Establish that a downgrade could actually succeed on this fixture.
    let mut ordinary = client();
    assert!(ordinary.exists_impl(Path::new("/")).unwrap());
    let error = NfsVecFs::builder(test_host())
        .minor_version(
            std::env::var("VNFS_TEST_MINOR")
                .ok()
                .map(|minor| minor.parse().unwrap()),
        )
        .connect_timeout(std::time::Duration::from_secs(2))
        .request_timeout(std::time::Duration::from_secs(2))
        .authentication(vnfs::NfsAuthentication::RpcsecGss {
            service_principal: Some("nfs@vnfs-nonexistent-service.invalid".into()),
            protection: vnfs::RpcsecGssProtection::Integrity,
        })
        .connect()
        .err()
        .expect("failed GSS must not silently connect with AUTH_SYS");
    assert!(
        error.is_transport(),
        "authentication setup must fail: {error}"
    );
    assert_eq!(error.status(), None);
    let message = error.to_string();
    assert!(
        message.contains("RPCSEC_GSS") || message.contains("RPC authentication setup failed"),
        "{message}"
    );
}

#[cfg(feature = "test-faults")]
use std::io::{self, Read, Write};
#[cfg(feature = "test-faults")]
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
#[cfg(feature = "test-faults")]
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "test-faults")]
use std::thread::JoinHandle;
#[cfg(feature = "test-faults")]
use std::time::Duration;
#[cfg(feature = "test-faults")]
use vfsi_sync::internal::faults::{FaultScript, OpenFaultPoint};

/// Test-only ONC-RPC record proxy. It forwards complete TCP records until
/// armed, then consumes and drops exactly one server reply before closing the
/// connection. That models a reply lost after the server executed a request.
#[cfg(feature = "test-faults")]
struct DropReplyProxy {
    address: SocketAddr,
    armed: Arc<AtomicBool>,
    dropped: Arc<(Mutex<bool>, Condvar)>,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

#[cfg(feature = "test-faults")]
impl DropReplyProxy {
    fn start(target: SocketAddr) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind NFS fault proxy");
        let address = listener.local_addr().expect("proxy local address");
        let armed = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let accept_armed = Arc::clone(&armed);
        let accept_dropped = Arc::clone(&dropped);
        let accept_stop = Arc::clone(&stop);
        let accept_thread = std::thread::spawn(move || {
            while let Ok((client, _)) = listener.accept() {
                if accept_stop.load(Ordering::SeqCst) {
                    break;
                }
                let server = TcpStream::connect(target).expect("connect proxy to NFS");
                client.set_nodelay(true).ok();
                server.set_nodelay(true).ok();
                let request_client = client.try_clone().expect("clone proxy client");
                let request_server = server.try_clone().expect("clone proxy server");
                std::thread::spawn(move || {
                    let mut source = request_client;
                    let mut destination = request_server;
                    let _ = io::copy(&mut source, &mut destination);
                });
                let armed = Arc::clone(&accept_armed);
                let dropped = Arc::clone(&accept_dropped);
                std::thread::spawn(move || forward_rpc_replies(server, client, &armed, &dropped));
            }
        });
        Self {
            address,
            armed,
            dropped,
            stop,
            accept_thread: Some(accept_thread),
        }
    }

    fn endpoint(&self) -> String {
        self.address.to_string()
    }

    fn arm(&self) {
        assert!(!self.armed.swap(true, Ordering::SeqCst));
    }

    fn wait_for_drop(&self) {
        let (state, changed) = &*self.dropped;
        let state = state.lock().expect("reply-loss state poisoned");
        let (state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(5), |dropped| !*dropped)
            .expect("reply-loss state poisoned");
        assert!(*state && !timeout.timed_out(), "proxy did not drop a reply");
    }
}

#[cfg(feature = "test-faults")]
fn reply_loss_target() -> SocketAddr {
    let host = std::env::var("VNFS_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    resolve_reply_loss_target(&host)
}

#[cfg(feature = "test-faults")]
fn resolve_reply_loss_target(host: &str) -> SocketAddr {
    let mut addresses = host
        .to_socket_addrs()
        .or_else(|_| (host, 2049).to_socket_addrs())
        .expect("resolve NFS fault proxy target");
    addresses
        .next()
        .expect("NFS fault proxy target has no address")
}

#[cfg(feature = "test-faults")]
#[test]
fn reply_loss_target_accepts_hostname_with_explicit_port() {
    let target = resolve_reply_loss_target("localhost:2050");
    assert_eq!(target.port(), 2050);
    assert!(target.ip().is_loopback());
}

#[cfg(feature = "test-faults")]
impl Drop for DropReplyProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(feature = "test-faults")]
fn forward_rpc_replies(
    mut server: TcpStream,
    mut client: TcpStream,
    armed: &AtomicBool,
    dropped: &(Mutex<bool>, Condvar),
) {
    loop {
        let mut header = [0u8; 4];
        if server.read_exact(&mut header).is_err() {
            return;
        }
        let discard = armed.swap(false, Ordering::SeqCst);
        loop {
            let marker = u32::from_be_bytes(header);
            let final_fragment = marker & 0x8000_0000 != 0;
            let length = (marker & 0x7fff_ffff) as usize;
            let mut fragment = vec![0; length];
            if server.read_exact(&mut fragment).is_err() {
                return;
            }
            if !discard
                && (client.write_all(&header).is_err() || client.write_all(&fragment).is_err())
            {
                return;
            }
            if final_fragment {
                break;
            }
            if server.read_exact(&mut header).is_err() {
                return;
            }
        }
        if discard {
            let (state, changed) = dropped;
            *state.lock().expect("reply-loss state poisoned") = true;
            changed.notify_all();
            let _ = server.shutdown(Shutdown::Both);
            let _ = client.shutdown(Shutdown::Both);
            return;
        }
    }
}

#[test]
fn builder_root_confines_absolute_and_parent_paths() {
    let mut admin = client();
    let root = setup_dir("builder_root");
    let mut confined = NfsVecFs::builder(test_host())
        .root(&root)
        .max_compound_bytes(128 * 1024)
        .connect()
        .expect("connect with namespace root");
    confined
        .vwrite_owned_impl(&[
            WriteOp::from_path("/absolute", VfOffset::At(0), b"a".to_vec()).with_creation(),
            WriteOp::from_path("../../clamped", VfOffset::At(0), b"b".to_vec()).with_creation(),
        ])
        .unwrap();
    assert!(
        admin
            .exists_impl(Path::new(&format!("{root}/absolute")))
            .unwrap()
    );
    assert!(
        admin
            .exists_impl(Path::new(&format!("{root}/clamped")))
            .unwrap()
    );
    confined
        .symlink_raw_impl(Path::new("/absolute"), Path::new("/absolute-link"))
        .unwrap();
    let linked = confined
        .vread_impl(&[ReadOp::from_path("/absolute-link", VfOffset::At(0), 1)])
        .unwrap();
    assert_eq!(linked[0].data, b"a");
    confined
        .symlink_raw_impl(Path::new("../../clamped"), Path::new("/relative-link"))
        .unwrap();
    let linked = confined
        .vread_impl(&[ReadOp::from_path("/relative-link", VfOffset::At(0), 1)])
        .unwrap();
    assert_eq!(linked[0].data, b"b");
    let descriptor = confined
        .open_raw_impl(Path::new("/absolute"), libc::O_RDONLY, 0)
        .unwrap();
    confined.reconnect().unwrap();
    let reopened = confined
        .vread_impl(&[ReadOp::new(descriptor.clone(), VfOffset::At(0), 1)])
        .unwrap();
    assert_eq!(reopened[0].data, b"a");
    confined.close_impl(&descriptor).unwrap();
    assert_eq!(confined.getcwd(), Path::new("/"));
    confined.shutdown().unwrap();
    admin.remove_paths_impl(&[Path::new(&root)], true).unwrap();
}

#[test]
fn builder_observer_receives_lifecycle_events() {
    #[derive(Default)]
    struct Observer(std::sync::Mutex<Vec<&'static str>>);
    impl NfsObserver for Observer {
        fn on_event(&self, event: &NfsEvent) {
            let name = match event {
                NfsEvent::Connected { .. } => "connected",
                NfsEvent::ReconnectStarted => "reconnect-started",
                NfsEvent::ReconnectSucceeded => "reconnect-succeeded",
                NfsEvent::ReconnectFailed { .. } => "reconnect-failed",
                NfsEvent::Shutdown { .. } => "shutdown",
                _ => "other",
            };
            self.0.lock().unwrap().push(name);
        }
    }

    let observer = std::sync::Arc::new(Observer::default());
    let filesystem = NfsVecFs::builder(test_host())
        .observer(observer.clone())
        .connect()
        .unwrap();
    filesystem.shutdown().unwrap();
    assert_eq!(*observer.0.lock().unwrap(), ["connected", "shutdown"]);
}

#[test]
fn rust_native_client_workflow_on_nfs() {
    let dir = setup_dir("rust_native_client");
    let host = std::env::var("VNFS_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let builder =
        vnfs::Nfs::builder(&host).version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        });
    let client = builder.connect().unwrap();
    let nested = format!("{dir}/nested");
    client.create_dir_all(&nested).unwrap();
    let paths = [format!("{nested}/one"), format!("{nested}/two")];
    let files = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .vopen(&paths)
        .unwrap();
    client
        .vwrite(
            &[
                vnfs::WriteOp::at(&files[0], 0, b"one"),
                vnfs::WriteOp::at(&files[1], 0, b"two"),
            ],
            Default::default(),
        )
        .unwrap();
    let values = client
        .vread(
            [
                vnfs::ReadOp::range(&files[0], 0, 3),
                vnfs::ReadOp::range(&files[1], 0, 3),
            ],
            vnfs::ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(values[0].data().unwrap(), b"one");
    assert_eq!(values[1].data().unwrap(), b"two");
    let mut first = [0u8; 8];
    let mut second = [0u8; 8];
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let lengths = client
        .vread(
            [
                vnfs::ReadOp::into(&files[0], 0, &mut first),
                vnfs::ReadOp::into(&files[1], 0, &mut second),
            ],
            Default::default(),
        )
        .unwrap();
    assert_eq!(
        lengths
            .iter()
            .map(|result| result.read())
            .collect::<Vec<_>>(),
        [3, 3]
    );
    assert_eq!(&first[..3], b"one");
    assert_eq!(&second[..3], b"two");
    assert!(lengths.iter().all(|result| result.is_buffered()));
    assert_eq!(vfsi_nfs::compound::thread_compound_stats().0, 1);
    assert_eq!(client.metadata(&paths[0]).unwrap().len(), 3);
    assert_eq!(client.read_dir(&nested).unwrap().len(), 2);
    let mut visited = Vec::new();
    let mut held_during_visit = Some(client.open(&paths[0]).unwrap());
    client
        .visit_dir(&nested, |entry| {
            // The application callback must run outside the NFS session lock.
            assert_eq!(client.metadata(entry.path())?.len(), 3);
            drop(held_during_visit.take());
            visited.push(entry.path().to_path_buf());
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    assert_eq!(visited.len(), 2);
    let mut early_count = 0;
    client
        .visit_dir(&nested, |_| {
            early_count += 1;
            Ok(std::ops::ControlFlow::Break(()))
        })
        .unwrap();
    assert_eq!(early_count, 1);
    let large = vec![0xa5; 2 * 1024 * 1024];
    files[0].write_at(&large, 0).unwrap();
    let mut large_buffer = vec![0; large.len() + 16];
    let read = files[0].read_at(&mut large_buffer, 0).unwrap();
    assert_eq!(read, large.len());
    assert_eq!(&large_buffer[..read], large);
    client.close_files(files).unwrap();

    // The short application API must work through the same native NFS backend.
    client
        .write_files(&[
            (paths[0].as_str(), b"first".as_slice()),
            (paths[1].as_str(), b"second".as_slice()),
        ])
        .unwrap();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    assert_eq!(
        client.read_files(&paths).unwrap(),
        vec![b"first".to_vec(), b"second".to_vec()]
    );
    assert_eq!(
        vfsi_nfs::compound::thread_compound_stats().0,
        1,
        "small path-based whole-file reads should share one compound"
    );

    for follow in [true, false] {
        let _ = vfsi_nfs::compound::thread_compound_stats();
        let attrs = client
            .vgetattrs(
                &paths,
                vnfs::MetadataOptions::new()
                    .fields(vnfs::MetadataFields::MODE | vnfs::MetadataFields::SIZE)
                    .follow_symlinks(follow),
            )
            .unwrap();
        assert_eq!(attrs.iter().map(|m| m.len()).collect::<Vec<_>>(), [5, 6]);
        assert_eq!(
            vfsi_nfs::compound::thread_compound_stats().0,
            1,
            "metadata must remain one vector compound"
        );
    }
    let link = format!("{nested}/link");
    let dangling = format!("{nested}/dangling");
    let mut backend = self::client();
    backend
        .symlink_raw_impl(Path::new("one"), Path::new(&link))
        .unwrap();
    backend
        .symlink_raw_impl(Path::new("missing"), Path::new(&dangling))
        .unwrap();
    assert!(client.metadata(&link).unwrap().is_file());
    assert!(client.symlink_metadata(&link).unwrap().is_symlink());
    assert!(client.symlink_metadata(&dangling).unwrap().is_symlink());
    assert_eq!(
        client
            .vgetattrs(
                &[paths[0].as_str(), dangling.as_str()],
                vnfs::MetadataOptions::new()
            )
            .unwrap_err()
            .index(),
        Some(1)
    );
    let renamed = [
        format!("{nested}/renamed-one"),
        format!("{nested}/renamed-two"),
    ];
    let _ = vfsi_nfs::compound::thread_compound_stats();
    vnfs::Vfsi::vrename(
        &client,
        &[(&paths[0], &renamed[0]), (&paths[1], &renamed[1])],
    )
    .unwrap();
    assert_eq!(
        vfsi_nfs::compound::thread_compound_stats().0,
        1,
        "the new rename vector must not become a scalar loop"
    );
    assert_eq!(
        client.read_files(&renamed).unwrap(),
        [b"first".to_vec(), b"second".to_vec()]
    );
    client.remove_dir_all(&dir).unwrap();
}

#[test]
fn native_removal_modes_forward_policy_across_roots() {
    use vnfs::{RemoveMode, RemoveOptions, Vfsi};
    let dir = setup_dir("native_removal_modes");
    let client = Nfs::builder(test_host())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .connect()
        .unwrap();
    let keep = format!("{dir}/keep");
    let file = format!("{keep}/file");
    let missing = format!("{dir}/missing");
    client.create_dir_all(&keep).unwrap();
    client.write(&file, b"payload").unwrap();
    let roots = [missing.as_str(), keep.as_str()];
    let error = client
        .vremove(&roots, RemoveMode::Contents, RemoveOptions::new())
        .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert!(client.metadata(&file).is_ok());
    let error = client
        .vremove(
            &roots,
            RemoveMode::Contents,
            RemoveOptions::new().continue_on_error(true),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert!(client.read_dir(&keep).unwrap().is_empty());
    assert!(client.metadata(&keep).unwrap().is_dir());
    client
        .vremove(&[&dir], RemoveMode::Tree, Default::default())
        .unwrap();
}

#[test]
fn unified_directory_visits_on_nfs() {
    use vnfs::{ControlFlow, MetadataFields, TraversalCompletion, VisitOptions};
    let dir = setup_dir("unified_directory_visits");
    let client = Nfs::builder(test_host())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .connect()
        .unwrap();
    let sub = format!("{dir}/sub");
    let file = format!("{sub}/file");
    client.create_dir_all(&sub).unwrap();
    client.write(&file, b"payload").unwrap();
    let options = VisitOptions::new()
        .fields(MetadataFields::SIZE)
        .max_entries(2);
    for (recursive, count) in [(false, 1), (true, 2)] {
        let mut seen = 0;
        client
            .visit_entries_with_options(&[&dir], options.recursive(recursive), |index, entry| {
                assert_eq!(index, 0);
                if entry.path() == Path::new(&file) {
                    assert_eq!(entry.metadata().len(), 7);
                }
                seen += 1;
                assert!(client.metadata(entry.path()).is_ok());
                Ok(ControlFlow::Continue(()))
            })
            .unwrap();
        assert_eq!(seen, count);
    }
    assert_eq!(
        client
            .visit_entries_with_options(
                &[dir.as_str(), "/absent-visitor-root"],
                options.recursive(true),
                |_, _| Ok(ControlFlow::Break(()))
            )
            .unwrap(),
        [TraversalCompletion::Stopped]
    );
    client.remove_dir_all(&dir).unwrap();
}

#[test]
fn application_collection_preserves_native_batching_and_root_groups() {
    use vnfs::{MetadataFields, VisitOptions};
    let dir = setup_dir("application_collection_batch");
    let client = Nfs::builder(test_host())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .connect()
        .unwrap();
    let roots: Vec<_> = (0..10).map(|index| format!("{dir}/d{index}")).collect();
    client
        .vmkdir(&roots.iter().map(|path| (path, 0o777)).collect::<Vec<_>>())
        .unwrap();
    let files: Vec<_> = roots
        .iter()
        .map(|root| (format!("{root}/file"), b"data"))
        .collect();
    client.write_files(&files).unwrap();
    let fields = MetadataFields::MODE | MetadataFields::SIZE;
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let listings = client
        .read_dirs_with_options(&roots, VisitOptions::new().fields(fields))
        .unwrap();
    let count = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        count <= 6,
        "shallow collection lost batching: {count} compounds"
    );
    assert_eq!(listings.len(), roots.len());
    for (index, tree) in listings.iter().enumerate() {
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].path, Path::new(&roots[index]));
        assert_eq!(tree[0].entries[0].metadata().len(), 4);
    }
    let error = client
        .read_dirs_with_options(&roots, VisitOptions::new().fields(fields).max_entries(5))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let trees = client
        .read_dirs_with_options(&[&dir], VisitOptions::new().recursive(true).fields(fields))
        .unwrap();
    let count = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(trees.len(), 1);
    assert_eq!(trees[0].len(), 11);
    assert!(
        count <= 9,
        "recursive collection lost native directory batching: {count}"
    );
    client.remove_dir_all(&dir).unwrap();
}

#[test]
fn whole_file_readv_honors_the_client_budget() {
    let dir = setup_dir("whole_file_readv_budget");
    let host = std::env::var("VNFS_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let version = match std::env::var("VNFS_TEST_MINOR").as_deref() {
        Ok("1") => vnfs::NfsVersion::V4_1,
        Ok("2") => vnfs::NfsVersion::V4_2,
        _ => vnfs::NfsVersion::Auto,
    };
    const BYTES: usize = 1024 * 1024 + 17;
    let client = Nfs::builder(host)
        .version(version)
        .limits(vnfs::ResourceLimits {
            max_read_bytes: BYTES,
            ..Default::default()
        })
        .connect()
        .unwrap();
    let path = format!("{dir}/file");
    let data = vec![b'x'; BYTES];
    client.write(&path, &data).unwrap();
    assert_eq!(
        client
            .vread([vnfs::ReadOp::whole(&path)], vnfs::ReadOptions::default())
            .unwrap()[0]
            .data()
            .unwrap(),
        data
    );
    assert_eq!(
        client
            .vread(
                [vnfs::ReadOp::whole(&path)],
                vnfs::ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(BYTES - 1))
            )
            .unwrap_err()
            .kind(),
        vnfs::ErrorKind::FileTooLarge
    );
    let mut dir_handle = client.open_dir_handle(&dir).unwrap();
    dir_handle.try_close().unwrap();
    dir_handle.try_close().unwrap();
    client.remove_dir_all(&dir).unwrap();
}

#[test]
fn rust_native_client_pool_uses_independent_sessions() {
    let dir = setup_dir("rust_native_client_pool");
    let host = std::env::var("VNFS_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let pool = Nfs::builder(host)
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .client_owner(b"vnfs-integration-pool".to_vec())
        .connect_pool(2)
        .unwrap();
    assert_eq!(pool.len(), 2);
    assert!(!pool.is_empty());
    assert!(pool.client(2).is_none());
    let first = pool.client(0).unwrap();
    use std::io::Write;
    let paths: Vec<_> = (0..2).map(|index| format!("{dir}/file-{index}")).collect();
    for path in &paths {
        let mut file = first
            .open_options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap();
        file.write_all(b"pool").unwrap();
        file.close().unwrap();
    }
    let threads: Vec<_> = paths
        .into_iter()
        .map(|path| {
            let client = pool.next_client();
            std::thread::spawn(move || {
                let file = client.open(&path).unwrap();
                let mut bytes = [0; 4];
                assert_eq!(file.read_at(&mut bytes, 0).unwrap(), 4);
                assert_eq!(&bytes, b"pool");
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    first.remove_dir_all(&dir).unwrap();
}

fn test_host() -> String {
    std::env::var("VNFS_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn client() -> NfsVecFs {
    // CI servers normally register NFS with rpcbind. Local test daemons often
    // listen directly on 2049 without registration, so allow an explicit
    // endpoint (for example, `127.0.0.1:2049`) for those environments.
    let host = std::env::var("VNFS_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    match std::env::var("VNFS_TEST_MINOR").as_deref() {
        Ok("1") => NfsVecFs::connect_minor(&host, 1),
        Ok("2") => NfsVecFs::connect_minor(&host, 2),
        _ => NfsVecFs::connect(&host),
    }
    .expect("connect to local nfs server")
}

/// Unique export-relative prefix for this test-binary invocation. A caller can
/// provide a stable identifier for diagnostics; otherwise PID plus wall-clock
/// nanoseconds keeps concurrent and repeated runs disjoint.
fn test_run_prefix() -> &'static str {
    static PREFIX: OnceLock<String> = OnceLock::new();
    PREFIX.get_or_init(|| {
        let run_id = std::env::var("VNFS_TEST_RUN_ID").unwrap_or_else(|_| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos();
            format!("{}-{nanos}", std::process::id())
        });
        assert!(
            !run_id.is_empty()
                && run_id.len() <= 128
                && run_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "VNFS_TEST_RUN_ID must be 1-128 ASCII letters, digits, '-' or '_'"
        );
        let prefix = format!("/.vnfs-test-{run_id}");
        let mut c = client();
        c.ensure_dir_impl(Path::new(&prefix), 0o755)
            .expect("create per-run test prefix");
        prefix
    })
}

/// Unique working directory for one test inside this run's prefix.
fn workdir(name: &str) -> String {
    format!("{}/{}", test_run_prefix(), name)
}

/// Make sure the work directory exists (create ancestors, then itself).
fn setup_dir(name: &str) -> String {
    let dir = workdir(name);
    let c = client();
    let mut c = c;
    c.ensure_dir_impl(Path::new(&dir), 0o755)
        .expect("ensure_dir workdir");
    dir
}

// ---------------------------------------------------------------------------
// init / deinit
// ---------------------------------------------------------------------------

#[test]
fn init_deinit() {
    // tc_init / tc_deinit: connect must succeed and drop must not panic; the
    // session cleanup (DESTROY_SESSION + DESTROY_CLIENTID) runs in Drop.
    let c = client();
    let cwd = c.getcwd();
    assert_eq!(cwd, Path::new("/"));
    drop(c);
}

#[test]
fn read_pool_streams_ordered_ranges_and_recovers_after_cancellation() {
    let dir = setup_dir("read_pool");
    let path = format!("{dir}/large.bin");
    let expected: Vec<u8> = (0usize..(3 * 1024 * 1024 + 173))
        .map(|index| (index.wrapping_mul(31) % 251) as u8)
        .collect();
    let client = Nfs::connect(test_host()).expect("connect NFS client");
    client.write(&path, &expected).expect("write fixture");

    let options = NfsReadPoolOptions::new()
        .worker_count(3)
        .chunk_size(64 * 1024)
        .max_in_flight(5)
        .max_buffered_bytes(5 * 64 * 1024);
    let mut pool = Nfs::builder(test_host())
        .connect_read_pool(options)
        .expect("connect read pool");

    let mut actual = Vec::with_capacity(expected.len());
    let mut next_offset = 0u64;
    let completion = pool
        .read_stream(&path, |offset, data| {
            assert_eq!(offset, next_offset, "callbacks are delivered in order");
            next_offset += data.len() as u64;
            actual.extend_from_slice(data);
            Ok(true)
        })
        .expect("stream complete file");
    assert_eq!(completion, vnfs::StreamCompletion::Complete);
    assert_eq!(actual, expected);

    let mut callbacks = 0;
    let completion = pool
        .read_stream(&path, |_, _| {
            callbacks += 1;
            Ok(false)
        })
        .expect("cancel stream");
    assert_eq!(
        completion,
        vnfs::StreamCompletion::Stopped {
            next_offset: 64 * 1024
        }
    );
    assert_eq!(callbacks, 1);

    let callback_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = pool.read_stream(&path, |_, _| -> vnfs::Result<bool> {
            panic!("injected callback panic")
        });
    }));
    assert!(
        callback_panic.is_err(),
        "callback panic should resume to caller"
    );

    // Cancellation and callback unwinding must close worker descriptors and
    // leave the pool usable for another complete stream.
    let mut reread = Vec::with_capacity(expected.len());
    pool.read_stream(&path, |_, data| {
        reread.extend_from_slice(data);
        Ok(true)
    })
    .expect("reuse pool after cancellation");
    assert_eq!(reread, expected);
    client.remove_file(&path).expect("remove fixture");
}

// ---------------------------------------------------------------------------
// open / close
// ---------------------------------------------------------------------------

#[test]
fn open_by_path_and_close() {
    let dir = setup_dir("open_close");
    let f = format!("{}/f.txt", dir);
    let mut c = client();
    let tf = c
        .open_raw_impl(Path::new(&f), libc::O_CREAT | libc::O_RDWR, 0o644)
        .expect("open/create");
    assert!(tf.is_descriptor());
    c.close_impl(&tf).expect("close");
}

#[test]
fn open_excl_fails_if_exists() {
    let dir = setup_dir("open_excl");
    let f = format!("{}/exists.txt", dir);
    let mut c = client();
    let first = c
        .open_raw_impl(Path::new(&f), libc::O_CREAT | libc::O_WRONLY, 0o644)
        .unwrap();
    c.close_impl(&first).unwrap();
    let r = c.open_raw_impl(
        Path::new(&f),
        libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
        0o644,
    );
    // O_EXCL on an existing file must fail with NFS4ERR_EXIST (17), surfaced
    // directly by the structured error (no string parsing).
    match r {
        Err(e) => assert_eq!(e.err_no(), nfsstat4_NFS4ERR_EXIST, "got {:?}", e),
        Ok(_) => panic!("O_EXCL on existing file must fail"),
    }
}

#[test]
fn openv_closev() {
    let dir = setup_dir("openv");
    let paths = [
        format!("{}/a.txt", dir),
        format!("{}/b.txt", dir),
        format!("{}/c.txt", dir),
    ];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let mut c = client();
    let files = c
        .vopen_raw_simple_impl(&refs, libc::O_CREAT | libc::O_RDWR, 0o644)
        .expect("openv_simple");
    assert_eq!(files.len(), 3);
    c.vclose_impl(&files).expect("closev");
}

#[test]
fn openv_append_writes_at_end() {
    let dir = setup_dir("openv_append");
    let paths = [
        format!("{}/a.txt", dir),
        format!("{}/b.txt", dir),
        format!("{}/c.txt", dir),
    ];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let mut c = client();
    let files = c
        .vopen_raw_simple_impl(&refs, libc::O_CREAT | libc::O_RDWR | libc::O_APPEND, 0o644)
        .expect("openv_simple append");
    for f in &files {
        c.vwrite_owned_impl(&[WriteOp::new(f.clone(), VfOffset::At(0), b"ab".to_vec())])
            .unwrap();
        c.vwrite_owned_impl(&[WriteOp::new(f.clone(), VfOffset::At(0), b"cd".to_vec())])
            .unwrap();
    }
    c.vclose_impl(&files).expect("closev");
    for p in &paths {
        assert_eq!(
            c.stat_impl(Path::new(p)).unwrap().size,
            4,
            "append via openv"
        );
        assert_eq!(
            c.read_raw_impl(&VfFile::from_path(p), 0, 4).unwrap(),
            b"abcd"
        );
    }
}

// ---------------------------------------------------------------------------
// chdir / getcwd
// ---------------------------------------------------------------------------

#[test]
fn chdir_getcwd() {
    let dir = setup_dir("chdir");
    let mut c = client();
    assert_eq!(c.getcwd(), Path::new("/"));
    c.chdir(Path::new(&dir)).expect("chdir");
    assert_eq!(c.getcwd(), Path::new(&dir));

    // Relative resolution now happens against the new cwd.
    c.mkdir_raw_impl(Path::new("rel"), 0o755)
        .expect("mkdir relative");
    let st = c
        .stat_impl(Path::new("/"))
        .expect("stat export root via absolute path");
    assert_eq!(st.ftype, VfType::Directory, "root is a directory");
}

// ---------------------------------------------------------------------------
// readv / writev
// ---------------------------------------------------------------------------

#[test]
fn writev_then_readv() {
    let dir = setup_dir("rwv");
    let f = format!("{}/data.bin", dir);
    let mut c = client();

    let payload = b"the quick brown fox jumps over the lazy dog\n".to_vec();
    let wr = &c
        .vwrite_owned_impl(&[
            WriteOp::from_path(&f, VfOffset::At(0), payload.clone()).with_creation()
        ])
        .expect("writev")[0];
    assert_eq!(wr.written, payload.len(), "all bytes written");

    let r = &c
        .vread_impl(&[ReadOp::from_path(&f, VfOffset::At(0), payload.len())])
        .expect("readv")[0];
    assert_eq!(r.data, payload, "read back what was written");
}

#[test]
fn non_utf8_filenames_roundtrip() {
    let dir = setup_dir("non_utf8");
    let mut c = client();
    let raw = b"n\xffb";
    let child =
        PathBuf::from(dir.clone()).join(PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec())));
    write_file(&mut c, &child, b"data");
    assert_eq!(read_all(&mut c, &child), b"data");
    let entries = c
        .listdir_impl(Path::new(&dir), AttrMask::stat(), 0, false)
        .unwrap();
    assert_eq!(entries.len(), 1);
    let name = entries[0].file.path().unwrap().file_name().unwrap();
    assert_eq!(name.as_bytes(), raw);
}

#[test]
fn readv_eof() {
    let dir = setup_dir("readv_eof");
    let f = format!("{}/short.txt", dir);
    let mut c = client();
    let payload = b"abcdefghij".to_vec();
    c.vwrite_owned_impl(
        &[WriteOp::from_path(&f, VfOffset::At(0), payload.clone()).with_creation()],
    )
    .unwrap();

    // Ask for more than exists: must not fail and must hit EOF.
    let r = &c
        .vread_impl(&[ReadOp::from_path(&f, VfOffset::At(0), 100)])
        .expect("readv")[0];
    assert_eq!(r.data.len(), payload.len());
    assert!(r.eof, "short read must set eof");
}

#[test]
fn readv_multiple_files() {
    let dir = setup_dir("readv_multi");
    let mut c = client();
    let mut iovs = Vec::new();
    for (i, name) in ["x", "y", "z"].iter().enumerate() {
        let f = format!("{}/{}.txt", dir, name);
        let payload = vec![b'a' + i as u8; 8];
        c.vwrite_owned_impl(&[WriteOp::from_path(&f, VfOffset::At(0), payload).with_creation()])
            .unwrap();
        let r = &c
            .vread_impl(&[ReadOp::from_path(&f, VfOffset::At(0), 8)])
            .unwrap()[0];
        iovs.push(r.data.clone());
    }
    assert_eq!(iovs[0], vec![b'a'; 8]);
    assert_eq!(iovs[1], vec![b'b'; 8]);
    assert_eq!(iovs[2], vec![b'c'; 8]);
}

// ---------------------------------------------------------------------------
// getattrs / stat / lstat / fstat / exists
// ---------------------------------------------------------------------------

fn make_file(path: &str, content: &[u8]) -> NfsVecFs {
    let mut c = client();
    c.vwrite_owned_impl(&[
        WriteOp::from_path(path, VfOffset::At(0), content.to_vec()).with_creation()
    ])
    .unwrap();
    c
}

#[test]
fn stat_and_lstat() {
    let dir = setup_dir("stat");
    let f = format!("{}/s.txt", dir);
    let content = b"1234567890".to_vec();
    let mut c = make_file(&f, &content);

    let st = c.stat_impl(Path::new(&f)).expect("stat");
    assert_eq!(st.size, content.len() as u64);
    assert!(st.nlink >= 1);
    assert!(st.fileid != 0);

    let lst = c.lstat_impl(Path::new(&f)).expect("lstat");
    assert_eq!(lst.fileid, st.fileid);
}

#[test]
fn fstat() {
    let dir = setup_dir("fstat");
    let f = format!("{}/fs.txt", dir);
    let content = b"fstat me".to_vec();
    let mut c = make_file(&f, &content);

    let tf = c
        .open_raw_impl(Path::new(&f), libc::O_RDONLY, 0)
        .expect("open");
    let st = c.fstat_impl(&tf).expect("fstat");
    assert_eq!(st.size, content.len() as u64);
    c.close_impl(&tf).unwrap();
}

#[test]
fn exists() {
    let dir = setup_dir("exists");
    let f = format!("{}/e.txt", dir);
    let mut c = make_file(&f, b"hi");
    assert!(c.exists_impl(Path::new(&f)).unwrap());
    assert!(
        !c.exists_impl(Path::new(&format!("{}/missing.txt", dir)))
            .unwrap()
    );
}

#[test]
fn getattrsv() {
    let dir = setup_dir("getattrsv");
    let mut c = client();
    let mut attrs = Vec::new();
    for name in ["g0", "g1", "g2"] {
        let f = format!("{}/{}.txt", dir, name);
        c.vwrite_owned_impl(&[
            WriteOp::from_path(&f, VfOffset::At(0), vec![b'x'; 3]).with_creation()
        ])
        .unwrap();
        attrs.push(VfAttrs {
            file: VfFile::from_path(&f),
            masks: AttrMask::MODE | AttrMask::SIZE | AttrMask::NLINK | AttrMask::FILEID,
            ..VfAttrs::default()
        });
    }
    c.vgetattrs_impl(&mut attrs).expect("getattrsv");
    for a in &attrs {
        assert_eq!(a.size, 3);
        assert!(a.fileid != 0);
    }
}

// ---------------------------------------------------------------------------
// setattrs / lsetattrs
// ---------------------------------------------------------------------------

#[test]
fn setattrsv_mode() {
    let dir = setup_dir("setattrs");
    let f = format!("{}/perm.txt", dir);
    let mut c = make_file(&f, b"data");
    c.vsetattrs_raw_impl(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask::MODE,
        mode: 0o600,
        ..VfAttrs::default()
    }])
    .expect("setattrsv");
    let st = c.stat_impl(Path::new(&f)).expect("stat");
    assert_eq!(st.mode & 0o777, 0o600);
}

#[test]
fn setattrsv_size_truncate() {
    let dir = setup_dir("setattrs_size");
    let f = format!("{}/trunc.txt", dir);
    let content = b"abcdefghijklmnop".to_vec();
    let mut c = make_file(&f, &content);
    c.vsetattrs_raw_impl(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask::SIZE,
        size: 5,
        ..VfAttrs::default()
    }])
    .expect("setattrsv");
    let st = c.stat_impl(Path::new(&f)).expect("stat");
    assert_eq!(st.size, 5);
}

#[test]
fn lsetattrsv() {
    let dir = setup_dir("lsetattrs");
    let f = format!("{}/l.txt", dir);
    let mut c = make_file(&f, b"data");
    c.vsetattrs_raw_nofollow_impl(&[VfAttrs {
        file: VfFile::from_path(&f),
        masks: AttrMask::MODE,
        mode: 0o640,
        ..VfAttrs::default()
    }])
    .expect("lsetattrsv");
    assert_eq!(c.stat_impl(Path::new(&f)).unwrap().mode & 0o777, 0o640);
}

// ---------------------------------------------------------------------------
// listdir
// ---------------------------------------------------------------------------

#[test]
fn listdir() {
    let dir = setup_dir("listdir");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/sub", dir)), 0o755)
        .unwrap();
    for (i, name) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
        let f = format!("{}/{}", dir, name);
        c.vwrite_owned_impl(&[
            WriteOp::from_path(&f, VfOffset::At(0), vec![b'a' + i as u8; 4]).with_creation(),
        ])
        .unwrap();
    }
    let contents = c
        .listdir_impl(Path::new(&dir), AttrMask::default(), 0, false)
        .expect("listdir");
    let names: Vec<&Path> = contents.iter().map(|a| a.file.path().unwrap()).collect();
    assert!(names.iter().any(|n| n.ends_with("a.txt")));
    assert!(names.iter().any(|n| n.ends_with("sub")));
}

#[test]
fn listdir_recursive() {
    let dir = setup_dir("listdir_rec");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/d1/d2", dir)), 0o755)
        .unwrap();
    for f in [format!("{}/top.txt", dir), format!("{}/d1/deep.txt", dir)] {
        c.vwrite_owned_impl(&[
            WriteOp::from_path(&f, VfOffset::At(0), b"ok".to_vec()).with_creation()
        ])
        .unwrap();
    }
    let contents = c
        .listdir_impl(Path::new(&dir), AttrMask::default(), 0, true)
        .expect("listdir recursive");
    let joined: Vec<String> = contents
        .iter()
        .map(|a| a.file.path().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        joined.iter().any(|p| p.ends_with("deep.txt")),
        "recursive listing finds deep.txt: {:?}",
        joined
    );
    assert!(joined.iter().any(|p| p.ends_with("top.txt")));
}

#[test]
fn bounded_walk_limits_nfs_accumulation() {
    use vnfs::WalkOptions;

    let dir = setup_dir("bounded_walk");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/d1/d2", dir)), 0o755)
        .unwrap();
    let one = format!("{}/one", dir);
    c.vwrite_owned_impl(&[WriteOp::from_path(&one, VfOffset::At(0), Vec::new()).with_creation()])
        .unwrap();

    let error = c
        .walk_with_options_impl(
            Path::new(&dir),
            AttrMask::stat(),
            WalkOptions::new().max_entries(1),
            &mut |_, _| {},
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let error = c
        .walk_with_options_impl(
            Path::new(&dir),
            AttrMask::stat(),
            WalkOptions::new().max_depth(0),
            &mut |_, _| {},
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let walked = c
        .walk_with_options_impl(
            Path::new(&dir),
            AttrMask::stat(),
            WalkOptions::new()
                .max_entries(16)
                .max_path_bytes(4096)
                .max_depth(4),
            &mut |_, entries| entries.sort_by(|a, b| a.file.path().cmp(&b.file.path())),
        )
        .unwrap();
    assert_eq!(walked.len(), 3);
}

// ---------------------------------------------------------------------------
// rename
// ---------------------------------------------------------------------------

#[test]
fn renamev() {
    let dir = setup_dir("rename");
    let f = format!("{}/old.txt", dir);
    let g = format!("{}/new.txt", dir);
    let mut c = make_file(&f, b"rename me");
    let pairs = [(VfFile::from_path(&f), VfFile::from_path(&g))];
    c.vrename_impl(&pairs).expect("renamev");
    assert!(!c.exists_impl(Path::new(&f)).unwrap(), "old name gone");
    assert!(c.exists_impl(Path::new(&g)).unwrap(), "new name present");
    assert_eq!(c.stat_impl(Path::new(&g)).unwrap().size, 9);
}

// ---------------------------------------------------------------------------
// remove / unlink
// ---------------------------------------------------------------------------

#[test]
fn unlink_and_exists() {
    let dir = setup_dir("unlink");
    let f = format!("{}/u.txt", dir);
    let mut c = make_file(&f, b"bye");
    assert!(c.exists_impl(Path::new(&f)).unwrap());
    c.unlink_impl(Path::new(&f)).expect("unlink");
    assert!(!c.exists_impl(Path::new(&f)).unwrap());
}

#[test]
fn unlinkv() {
    let dir = setup_dir("unlinkv");
    let mut c = client();
    let files = [
        format!("{}/u1", dir),
        format!("{}/u2", dir),
        format!("{}/u3", dir),
    ];
    let refs: Vec<&Path> = files.iter().map(Path::new).collect();
    for f in &files {
        c.vwrite_owned_impl(&[
            WriteOp::from_path(f, VfOffset::At(0), b"z".to_vec()).with_creation()
        ])
        .unwrap();
    }
    c.vunlink_impl(&refs).expect("unlinkv");
    for f in &files {
        assert!(!c.exists_impl(Path::new(f)).unwrap());
    }
}

#[test]
fn removev() {
    let dir = setup_dir("removev");
    let mut c = client();
    let f = format!("{}/rv.txt", dir);
    c.vwrite_owned_impl(&[WriteOp::from_path(&f, VfOffset::At(0), b"r".to_vec()).with_creation()])
        .unwrap();
    c.vremove_impl(&[VfFile::from_path(&f)]).expect("removev");
    assert!(!c.exists_impl(Path::new(&f)).unwrap());
}

// ---------------------------------------------------------------------------
// mkdir
// ---------------------------------------------------------------------------

#[test]
fn mkdirv() {
    let dir = setup_dir("mkdirv");
    let d = format!("{}/nd", dir);
    let mut c = client();
    c.vmkdir_impl(&[VfAttrs {
        file: VfFile::from_path(&d),
        masks: AttrMask::MODE,
        mode: 0o750,
        ..VfAttrs::default()
    }])
    .expect("mkdirv");
    let st = c.stat_impl(Path::new(&d)).expect("stat dir");
    assert_eq!(st.ftype, VfType::Directory, "NF4DIR");
    assert_eq!(st.mode & 0o777, 0o750);
}

#[test]
fn mkdir() {
    let dir = setup_dir("mkdir");
    let d = format!("{}/simple", dir);
    let mut c = client();
    c.mkdir_raw_impl(Path::new(&d), 0o755).expect("mkdir");
    assert!(c.exists_impl(Path::new(&d)).unwrap());
}

// ---------------------------------------------------------------------------
// symlink / readlink
// ---------------------------------------------------------------------------

#[test]
fn symlink_readlink() {
    let dir = setup_dir("symlink");
    let target = format!("{}/real.txt", dir);
    let link = format!("{}/link.txt", dir);
    let mut c = make_file(&target, b"real content");
    c.symlink_raw_impl(Path::new(&target), Path::new(&link))
        .expect("symlink");
    let got = c.readlink_raw_impl(Path::new(&link)).expect("readlink");
    assert_eq!(String::from_utf8_lossy(&got), target);
}

#[test]
fn symlinkv_readlinkv() {
    let dir = setup_dir("symlinkv");
    let mut c = client();
    let olds: Vec<String> = (0..2).map(|i| format!("{}/src{}", dir, i)).collect();
    let news: Vec<String> = (0..2).map(|i| format!("{}/ln{}", dir, i)).collect();
    let old_refs: Vec<&Path> = olds.iter().map(Path::new).collect();
    let new_refs: Vec<&Path> = news.iter().map(Path::new).collect();
    c.vsymlink_impl(&old_refs, &new_refs).expect("symlinkv");
    let targets = c.vreadlink_impl(&new_refs).expect("readlinkv");
    for (t, o) in targets.iter().zip(&olds) {
        assert_eq!(String::from_utf8_lossy(t), *o);
    }
}

#[test]
fn intermediate_symlink_components_followed() {
    let dir = setup_dir("intermediate_link");
    let mut c = client();
    c.mkdir_raw_impl(Path::new(&format!("{}/realdir", dir)), 0o755)
        .unwrap();
    write_file(&mut c, Path::new(&format!("{}/realdir/file", dir)), b"data");
    c.symlink_raw_impl(Path::new("realdir"), Path::new(&format!("{}/dirlink", dir)))
        .unwrap();

    let st = c
        .stat_impl(Path::new(&format!("{}/dirlink/file", dir)))
        .unwrap();
    assert_eq!(st.ftype, VfType::Regular);
    assert_eq!(st.size, 4);
    assert_eq!(
        read_all(&mut c, Path::new(&format!("{}/dirlink/file", dir))),
        b"data"
    );

    // Path-based operations (unlink) resolve through the intermediate link.
    let f = format!("{}/dirlink/other", dir);
    write_file(&mut c, Path::new(&f), b"x");
    c.unlink_impl(Path::new(&f)).unwrap();
    assert!(!c.exists_impl(Path::new(&f)).unwrap());

    // lstat of a path under the link still reports the final object.
    assert_eq!(
        c.lstat_impl(Path::new(&format!("{}/dirlink/file", dir)))
            .unwrap()
            .ftype,
        VfType::Regular
    );
}

#[test]
fn path_readv_is_batched() {
    let dir = setup_dir("path_batch");
    let mut c = client();
    let paths: Vec<String> = (0..5)
        .map(|i| {
            let p = format!("{}/f{}", dir, i);
            write_file(&mut c, Path::new(&p), b"x");
            p
        })
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let ops: Vec<ReadOp> = paths
        .iter()
        .map(|p| ReadOp::at(VfFile::from_path(p), 0, 1))
        .collect();
    let res = c.vread_impl(&ops).unwrap();
    assert_eq!(res.len(), 5);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    // Batching must beat the per-op open+read+close round trips.
    assert!(
        compounds < 3 * paths.len() as u64,
        "path readv used {} compounds for {} reads",
        compounds,
        paths.len()
    );
}

#[test]
fn writev_path_is_one_compound_per_dir() {
    let dir = setup_dir("writev1");
    let mut c = client();
    let paths: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let ops: Vec<WriteOp> = paths
        .iter()
        .map(|p| WriteOp::at(VfFile::from_path(p), 0, b"x".to_vec()).with_creation())
        .collect();
    let res = c.vwrite_owned_impl(&ops).unwrap();
    assert_eq!(res.len(), 5);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 1,
        "path writev of N files in one dir must be a single compound, got {}",
        compounds
    );
}

#[test]
fn readv_path_is_one_compound_per_dir() {
    let dir = setup_dir("readv1");
    let mut c = client();
    let paths: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    write_file(&mut c, Path::new(&paths[0]), b"hello world");
    let payloads: Vec<Vec<u8>> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let data = format!("data-{}", i).into_bytes();
            write_file(&mut c, Path::new(p), &data);
            data
        })
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let ops: Vec<ReadOp> = paths
        .iter()
        .zip(&payloads)
        .map(|(p, d)| ReadOp::at(VfFile::from_path(p), 0, d.len()))
        .collect();
    let res = c.vread_impl(&ops).unwrap();
    assert_eq!(res.len(), 5);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 1,
        "path readv of N files in one dir must be a single compound, got {}",
        compounds
    );
    for (r, d) in res.iter().zip(&payloads) {
        assert_eq!(&r.data, d);
    }
}

#[test]
fn writev_path_openwrite_form_is_two_compounds() {
    // The portable fallback: one open+write compound, one close compound.
    let dir = setup_dir("writev2");
    let mut c = client();
    c.set_merged_mode("openwrite");
    let paths: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    let payloads: Vec<Vec<u8>> = (0..5).map(|i| format!("data-{}", i).into_bytes()).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let ops: Vec<WriteOp> = paths
        .iter()
        .zip(&payloads)
        .map(|(p, d)| WriteOp::at(VfFile::from_path(p), 0, d.clone()).with_creation())
        .collect();
    let res = c.vwrite_owned_impl(&ops).unwrap();
    assert_eq!(res.len(), 5);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 2,
        "openwrite form must use 2 compounds, got {}",
        compounds
    );
    // Data round-trips.
    let back = c
        .vread_impl(
            &paths
                .iter()
                .zip(&payloads)
                .map(|(p, d)| ReadOp::at(VfFile::from_path(p), 0, d.len()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
    for (r, d) in back.iter().zip(&payloads) {
        assert_eq!(&r.data, d);
    }
}

#[test]
fn phased_create_batch_continues_after_missing_middle_lookup() {
    // A failed LOOKUP stops an NFS COMPOUND, so the third item is unexecuted,
    // not another NOENT. The failure-aware planner must submit that suffix in
    // a fresh compound or the existing third file is incorrectly opened with
    // CREATE_GUARDED and the vector fails with NFS4ERR_EXIST.
    let dir = setup_dir("planner_lookup_suffix");
    let paths = [
        format!("{dir}/existing-a"),
        format!("{dir}/missing-b"),
        format!("{dir}/existing-c"),
    ];
    let mut c = client();
    write_file(&mut c, Path::new(&paths[0]), b"old-a");
    write_file(&mut c, Path::new(&paths[2]), b"old-c");
    c.set_merged_mode("off");

    let payloads = [b"new-a".to_vec(), b"new-b".to_vec(), b"new-c".to_vec()];
    let writes: Vec<WriteOp> = paths
        .iter()
        .zip(&payloads)
        .map(|(path, data)| WriteOp::at(VfFile::from_path(path), 0, data.clone()).with_creation())
        .collect();
    let results = c
        .vwrite_owned_impl(&writes)
        .expect("mixed existence create batch");
    assert_eq!(results.len(), paths.len());

    let reads: Vec<ReadOp> = paths
        .iter()
        .zip(&payloads)
        .map(|(path, data)| ReadOp::at(VfFile::from_path(path), 0, data.len()))
        .collect();
    let roundtrip = c.vread_impl(&reads).expect("read mixed existence batch");
    for (result, expected) in roundtrip.iter().zip(&payloads) {
        assert_eq!(&result.data, expected);
    }
}

#[test]
fn readv_path_openwrite_form_is_two_compounds() {
    let dir = setup_dir("readv2");
    let mut c = client();
    let paths: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    let payloads: Vec<Vec<u8>> = (0..5).map(|i| format!("data-{}", i).into_bytes()).collect();
    for (p, d) in paths.iter().zip(&payloads) {
        write_file(&mut c, Path::new(p), d);
    }
    c.set_merged_mode("openwrite");
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let ops: Vec<ReadOp> = paths
        .iter()
        .zip(&payloads)
        .map(|(p, d)| ReadOp::at(VfFile::from_path(p), 0, d.len()))
        .collect();
    let res = c.vread_impl(&ops).unwrap();
    assert_eq!(res.len(), 5);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 2,
        "openwrite form must use 2 compounds, got {}",
        compounds
    );
    for (r, d) in res.iter().zip(&payloads) {
        assert_eq!(&r.data, d);
    }
}

#[test]
fn getattrsv_path_is_one_compound() {
    let dir = setup_dir("getattrv1");
    let mut c = client();
    let paths: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    for p in &paths {
        write_file(&mut c, Path::new(p), b"x");
    }
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let mut attrs: Vec<VfAttrs> = paths
        .iter()
        .map(|p| VfAttrs {
            file: VfFile::from_path(p),
            masks: AttrMask::stat(),
            ..VfAttrs::default()
        })
        .collect();
    c.vgetattrs_impl(&mut attrs).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 1,
        "getattrsv must be one compound, got {}",
        compounds
    );
    for a in &attrs {
        assert_eq!(a.ftype, VfType::Regular);
        assert!(a.returned.contains(AttrMask::SIZE));
    }
}

#[test]
fn setattrsv_path_is_one_compound() {
    let dir = setup_dir("setattrv1");
    let mut c = client();
    let paths: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    for p in &paths {
        write_file(&mut c, Path::new(p), b"long content");
    }
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let attrs: Vec<VfAttrs> = paths
        .iter()
        .map(|p| VfAttrs {
            file: VfFile::from_path(p),
            masks: AttrMask::SIZE,
            size: 3,
            ..VfAttrs::default()
        })
        .collect();
    c.vsetattrs_raw_impl(&attrs).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 1,
        "setattrsv must be one compound, got {}",
        compounds
    );
    for p in &paths {
        assert_eq!(c.stat_impl(Path::new(p)).unwrap().size, 3);
    }
}

#[test]
fn openv_closev_path_is_one_compound_each() {
    let dir = setup_dir("openv1");
    let mut c = client();
    let paths: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let files = Backend::vopen_raw_impl(
        &mut c,
        &refs,
        &[libc::O_CREAT | libc::O_RDWR; 5],
        &[0o644; 5],
    )
    .unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 1,
        "openv must be one compound, got {}",
        compounds
    );
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    c.vclose_impl(&files).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 1,
        "closev must be one compound, got {}",
        compounds
    );
}

#[test]
fn removev_path_is_one_compound() {
    let dir = setup_dir("removev1");
    let mut c = client();
    let paths: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    for p in &paths {
        write_file(&mut c, Path::new(p), b"x");
    }
    let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_path(p)).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    c.vremove_impl(&files).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 1,
        "removev must be one compound, got {}",
        compounds
    );
    for p in &paths {
        assert!(!c.exists_impl(Path::new(p)).unwrap());
    }
}

#[test]
fn renamev_path_is_one_compound() {
    let dir = setup_dir("renamev1");
    let mut c = client();
    let srcs: Vec<String> = (0..5).map(|i| format!("{}/f{}", dir, i)).collect();
    let dsts: Vec<String> = (0..5).map(|i| format!("{}/g{}", dir, i)).collect();
    for p in &srcs {
        write_file(&mut c, Path::new(p), b"x");
    }
    let pairs: Vec<(VfFile, VfFile)> = srcs
        .iter()
        .zip(&dsts)
        .map(|(s, d)| (VfFile::from_path(s), VfFile::from_path(d)))
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    c.vrename_impl(&pairs).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(
        compounds, 1,
        "renamev must be one compound, got {}",
        compounds
    );
    for d in &dsts {
        assert!(c.exists_impl(Path::new(d)).unwrap());
    }
}

#[test]
fn path_writev_follows_final_symlink() {
    // The merged compound cannot OPEN a symlink (NFS4ERR_SYMLINK); the
    // backend must fall back to the phased path and write the target.
    let dir = setup_dir("wrsymlink");
    let mut c = client();
    let target = format!("{}/target", dir);
    let link = format!("{}/link", dir);
    write_file(&mut c, Path::new(&target), b"");
    let rel = std::path::Path::new(&target)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    c.symlink_raw_impl(Path::new(&rel), Path::new(&link))
        .unwrap();
    c.vwrite_owned_impl(&[WriteOp::at(
        VfFile::from_path(&link),
        0,
        b"via-link".to_vec(),
    )])
    .unwrap();
    assert_eq!(
        c.read_raw_impl(&VfFile::from_path(&target), 0, 8).unwrap(),
        b"via-link"
    );
    // And a readv through the link reads the target.
    let r = c
        .vread_impl(&[ReadOp::at(VfFile::from_path(&link), 0, 8)])
        .unwrap();
    assert_eq!(r[0].data, b"via-link");
}

#[test]
fn scalar_write_follows_final_symlink_chains_and_creates_dangling_targets() {
    let dir = setup_dir("scalar_wrsymlink");
    let mut backend = self::client();
    let fs = Nfs::builder(test_host())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .connect()
        .unwrap();
    let target = format!("{dir}/target");
    let link = format!("{dir}/link");
    let chain = format!("{dir}/chain");
    let dangling = format!("{dir}/dangling");
    let missing = format!("{dir}/missing");
    fs.write(&target, b"old contents longer than replacement")
        .unwrap();
    backend
        .symlink_raw_impl(Path::new("target"), Path::new(&link))
        .unwrap();
    backend
        .symlink_raw_impl(Path::new("link"), Path::new(&chain))
        .unwrap();
    backend
        .symlink_raw_impl(Path::new("missing"), Path::new(&dangling))
        .unwrap();

    fs.write(&chain, b"new").unwrap();
    assert_eq!(fs.read_files(&[&target]).unwrap(), [b"new".to_vec()]);
    fs.write(&dangling, b"created").unwrap();
    assert_eq!(fs.read_files(&[&missing]).unwrap(), [b"created".to_vec()]);
    fs.write(&link, b"").unwrap();
    assert_eq!(fs.metadata(&target).unwrap().len(), 0);
    for path in [&link, &chain, &dangling] {
        assert!(fs.symlink_metadata(path).unwrap().is_symlink());
    }
    fs.remove_dir_all(&dir).unwrap();
}

#[test]
fn writev_path_compresses_shared_prefix() {
    // Files in nested directories sharing a prefix: the shared directory walk
    // happens once (relative LOOKUPs from the saved fh), not per file.
    let dir = setup_dir("compress");
    let mut c = client();
    let base = format!("{}/p/a", dir);
    let f0 = format!("{}/f0", base);
    let f1 = format!("{}/b/f1", base);
    let f2 = format!("{}/b/c/f2", base);
    c.ensure_dir_impl(Path::new(&format!("{}/b/c", base)), 0o755)
        .unwrap();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let res = c
        .vwrite_owned_impl(&[
            WriteOp::at(VfFile::from_path(&f0), 0, b"0".to_vec()).with_creation(),
            WriteOp::at(VfFile::from_path(&f1), 0, b"1".to_vec()).with_creation(),
            WriteOp::at(VfFile::from_path(&f2), 0, b"2".to_vec()).with_creation(),
        ])
        .unwrap();
    assert_eq!(res.len(), 3);
    let (compounds, ops, _, _) = vfsi_nfs::compound::thread_compound_stats();
    assert_eq!(compounds, 1, "compressed writev must stay one compound");
    // Re-walking /p/a from the root for each new directory would cost more
    // ops than the relative LOOKUP walk from the saved directory.
    assert!(
        ops < 24,
        "shared prefix should be walked once; used {} ops",
        ops
    );
    assert_eq!(
        c.read_raw_impl(&VfFile::from_path(&f2), 0, 1).unwrap(),
        b"2"
    );
}

#[test]
fn readv_writev_mixes_descriptors_and_paths() {
    let dir = setup_dir("mixed");
    let mut c = client();
    let fpath = format!("{}/pathfile", dir);
    let fdpath = format!("{}/fdfile", dir);
    let fd = c
        .open_raw_impl(Path::new(&fdpath), libc::O_CREAT | libc::O_RDWR, 0o644)
        .unwrap();

    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let w = c
        .vwrite_owned_impl(&[
            WriteOp::new(fd.clone(), VfOffset::At(0), b"fd-data".to_vec()),
            WriteOp::at(VfFile::from_path(&fpath), 0, b"path-data".to_vec()).with_creation(),
        ])
        .unwrap();
    assert_eq!(w.len(), 2);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(compounds, 1, "mixed writev must be one compound");

    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let r = c
        .vread_impl(&[
            ReadOp::new(fd.clone(), VfOffset::At(0), 7),
            ReadOp::at(VfFile::from_path(&fpath), 0, 9),
        ])
        .unwrap();
    assert_eq!(r[0].data, b"fd-data");
    assert_eq!(r[1].data, b"path-data");
    assert!(r[0].file.is_descriptor(), "result echoes the descriptor op");
    assert!(r[1].file.path().is_some(), "result echoes the path op");
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(compounds, 1, "mixed readv must be one compound");
    c.close_impl(&fd).unwrap();
}

#[test]
fn writev_respects_compound_size_limit() {
    let dir = setup_dir("payload");
    let mut c = client();
    // 4 files x 64 KiB; a 100 KiB cap allows one file per compound.
    c.set_max_compound_bytes(100 * 1024);
    let paths: Vec<String> = (0..4).map(|i| format!("{}/f{}", dir, i)).collect();
    let payload = vec![b'x'; 64 * 1024];
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let ops: Vec<WriteOp> = paths
        .iter()
        .map(|p| WriteOp::at(VfFile::from_path(p), 0, payload.clone()).with_creation())
        .collect();
    let res = c.vwrite_owned_impl(&ops).unwrap();
    assert_eq!(res.len(), 4);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(compounds, 4, "payload cap must split into 4 compounds");
    for p in &paths {
        assert_eq!(
            c.stat_impl(Path::new(p)).unwrap().size,
            payload.len() as u64
        );
    }
}

#[test]
fn writev_partial_failure_reports_failing_index() {
    // Contract test: the failing index is attributed correctly, the prefix
    // executed, and the suffix was not reached. (A whole-batch fallback would
    // also satisfy this; the openv/removev tests below discriminate resume.)
    let dir = setup_dir("resume");
    let mut c = client();
    let f0 = format!("{}/f0", dir);
    // A missing PARENT (not a missing file: UNCHECKED create would make one).
    let missing = format!("{}/no/such/dir/f1", dir);
    let f2 = format!("{}/f2", dir);
    let e = c
        .vwrite_owned_impl(&[
            WriteOp::at(VfFile::from_path(&f0), 0, b"x".to_vec()).with_creation(),
            WriteOp::at(VfFile::from_path(&missing), 0, b"y".to_vec()).with_creation(),
            WriteOp::at(VfFile::from_path(&f2), 0, b"z".to_vec()).with_creation(),
        ])
        .unwrap_err();
    assert_eq!(
        e.index(),
        Some(1),
        "failure must be attributed to the missing file"
    );
    // The prefix op executed before the failure; the suffix was not reached.
    assert!(c.exists_impl(Path::new(&f0)).unwrap());
    assert!(!c.exists_impl(Path::new(&format!("{}/no", dir))).unwrap());
    assert!(!c.exists_impl(Path::new(&f2)).unwrap());
}

#[test]
fn openv_partial_failure_resumes_from_failing_index() {
    // O_EXCL makes this discriminating: a whole-batch fallback would re-open
    // f0 (already created by the merged prefix) and fail with NFS4ERR_EXIST
    // at index 0. Resume keeps the prefix and fails at the missing parent,
    // index 1.
    let dir = setup_dir("openv_resume");
    let mut c = client();
    let f0 = format!("{}/f0", dir);
    let bad = format!("{}/no/such/dir/f1", dir);
    let f2 = format!("{}/f2", dir);
    let refs = [Path::new(&f0), Path::new(&bad), Path::new(&f2)];
    let flags = [libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 3];
    let modes = [0o644; 3];
    let e = Backend::vopen_raw_impl(&mut c, &refs, &flags, &modes).unwrap_err();
    assert_eq!(e.index(), Some(1), "resume must fail at the missing parent");
    assert_eq!(
        e.status(),
        Some(StatusCode::Nfs(e.err_no())),
        "the raw NFS status must retain its protocol domain"
    );
    assert!(
        c.exists_impl(Path::new(&f0)).unwrap(),
        "prefix open created f0"
    );
    assert!(
        !c.exists_impl(Path::new(&f2)).unwrap(),
        "suffix was not attempted"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_injected_registration_failure_closes_every_confirmed_handle() {
    let dir = setup_dir("openv_fault_register");
    let mut client = client();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRegister { index: 1 },
        VfError::transport(None, "injected registration failure"),
    ));
    client.set_fault_injector(script.clone());
    let paths = [
        format!("{dir}/f0"),
        format!("{dir}/f1"),
        format!("{dir}/f2"),
    ];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let error = Backend::vopen_raw_impl(
        &mut client,
        &refs,
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 3],
        &[0o644; 3],
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(client.test_open_handle_count(), 0);
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn closev_failure_keeps_handles_available_for_cleanup() {
    let dir = setup_dir("closev_fault_retains_handles");
    let mut client = client();
    let paths = [format!("{dir}/f0"), format!("{dir}/f1")];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let files = Backend::vopen_raw_impl(
        &mut client,
        &refs,
        &[libc::O_CREAT | libc::O_RDWR; 2],
        &[0o644; 2],
    )
    .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseDispatch { index: 0 },
        VfError::transport(None, "injected close failure"),
    ));
    client.set_fault_injector(script.clone());
    let error = client.vclose_impl(&files).unwrap_err();
    assert!(error.is_transport());
    assert_eq!(client.test_open_handle_count(), 2);
    assert!(script.is_consumed());
    client.vclose_impl(&files).unwrap();
    assert_eq!(client.test_open_handle_count(), 0);
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn closev_semantic_failure_removes_only_confirmed_prefix() {
    let dir = setup_dir("closev_semantic_prefix");
    let mut client = client();
    let paths = [
        format!("{dir}/f0"),
        format!("{dir}/f1"),
        format!("{dir}/f2"),
    ];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let files = Backend::vopen_raw_impl(
        &mut client,
        &refs,
        &[libc::O_CREAT | libc::O_RDWR; 3],
        &[0o644; 3],
    )
    .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseItem { index: 1 },
        VfError::nfs(1, nfsv41_sys::nfsstat4_NFS4ERR_IO),
    ));
    client.set_fault_injector(script.clone());
    let error = client.vclose_impl(&files).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(script.is_consumed());
    assert_eq!(client.test_open_handle_count(), 2);
    client.vclose_impl(&files[1..]).unwrap();
    assert_eq!(client.test_open_handle_count(), 0);
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn scalar_open_registration_failure_closes_remote_open() {
    let dir = setup_dir("scalar_open_fault_cleanup");
    let mut client = client();
    let path = format!("{dir}/file");
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRegister { index: 0 },
        VfError::transport(None, "injected scalar registration failure"),
    ));
    client.set_fault_injector(script.clone());
    let error = client
        .open_raw_impl(Path::new(&path), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap_err();
    assert!(error.is_transport());
    assert!(script.is_consumed());
    assert_eq!(client.test_open_handle_count(), 0);
    assert_eq!(client.test_confirmed_path_closes(), 1);
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn confirmed_write_chunk_advances_descriptor_after_later_failure() {
    let dir = setup_dir("partial_write_cursor");
    let mut client = client();
    // The request cap includes the RPC envelope and COMPOUND framing. This
    // value leaves a 4096-byte first WRITE chunk after those reservations.
    client.set_max_compound_bytes(6144);
    let path = format!("{dir}/file");
    let file = client
        .open_raw_impl(Path::new(&path), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterWriteChunk { chunk: 0 },
        VfError::transport(None, "injected failure after confirmed write"),
    ));
    client.set_fault_injector(script.clone());
    let error = client
        .vwrite_owned_impl(&[WriteOp::new(file.clone(), VfOffset::Cur, vec![b'a'; 8192])])
        .unwrap_err();
    assert!(error.is_transport());
    assert!(script.is_consumed());
    client
        .vwrite_owned_impl(&[WriteOp::new(file.clone(), VfOffset::Cur, vec![b'b'])])
        .unwrap();
    let read = client
        .vread_impl(&[ReadOp::new(file.clone(), VfOffset::At(4096), 1)])
        .unwrap();
    assert_eq!(read[0].data, b"b");
    client.close_impl(&file).unwrap();
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn recursive_remove_propagates_type_lookup_transport_failure() {
    let dir = setup_dir("remove_type_fault");
    let mut client = client();
    let path = format!("{dir}/kept");
    client
        .vwrite_owned_impl(&[
            WriteOp::from_path(&path, VfOffset::At(0), b"data".to_vec()).with_creation()
        ])
        .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRemoveType { index: 0 },
        VfError::transport(None, "injected type lookup failure"),
    ));
    client.set_fault_injector(script.clone());
    assert!(
        client
            .remove_paths_impl(&[Path::new(&path)], true)
            .unwrap_err()
            .is_transport()
    );
    assert!(script.is_consumed());
    assert!(client.exists_impl(Path::new(&path)).unwrap());
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_injected_post_reply_transport_failure_has_no_fabricated_index() {
    let dir = setup_dir("openv_fault_reply");
    let mut client = client();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterReply { chunk: 0 },
        VfError::transport(None, "injected lost reply"),
    ));
    client.set_fault_injector(script.clone());
    let paths = [format!("{dir}/f0"), format!("{dir}/f1")];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let error = Backend::vopen_raw_impl(
        &mut client,
        &refs,
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 2],
        &[0o644; 2],
    )
    .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), None);
    assert!(
        client.exists_impl(Path::new(&paths[0])).unwrap(),
        "the completed mutating open must not be replayed"
    );
    assert!(
        client.exists_impl(Path::new(&paths[1])).unwrap(),
        "the completed mutating open must not be replayed"
    );
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(client.test_open_handle_count(), 0);
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_later_chunk_failure_closes_confirmed_earlier_opens() {
    let dir = setup_dir("openv_later_chunk_cleanup");
    let mut client = client();
    client.set_max_compound_bytes(4096);
    let paths: Vec<String> = (0..32).map(|index| format!("{dir}/f{index}")).collect();
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeOpenChunk { chunk: 1 },
        VfError::transport(None, "injected failure before second open compound"),
    ));
    client.set_fault_injector(script.clone());
    let error = Backend::vopen_raw_impl(
        &mut client,
        &refs,
        &vec![libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; paths.len()],
        &vec![0o600; paths.len()],
    )
    .unwrap_err();
    assert!(error.is_transport(), "unexpected error: {error:?}");
    assert_eq!(error.index(), None);
    assert!(script.is_consumed());
    assert!(
        client.test_confirmed_path_closes() > 0,
        "confirmed opens from the first compound were not closed"
    );
    assert_eq!(client.test_open_handle_count(), 0);
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn failed_open_cleanup_is_retained_and_retried_before_the_next_openv() {
    let dir = setup_dir("openv_deferred_cleanup");
    let mut client = client();
    client.set_max_compound_bytes(4096);
    let paths: Vec<String> = (0..32).map(|index| format!("{dir}/f{index}")).collect();
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let script = Arc::new(FaultScript::new([
        (
            OpenFaultPoint::BeforeOpenChunk { chunk: 1 },
            VfError::transport(None, "injected second-compound failure"),
        ),
        (
            OpenFaultPoint::BeforePathCloseBatch,
            VfError::transport(None, "injected cleanup failure"),
        ),
    ]));
    client.set_fault_injector(script.clone());
    assert!(
        Backend::vopen_raw_impl(
            &mut client,
            &refs,
            &vec![libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; paths.len()],
            &vec![0o600; paths.len()],
        )
        .unwrap_err()
        .is_transport()
    );
    assert!(script.is_consumed());
    assert!(client.test_deferred_path_close_count() > 0);

    let next_path = format!("{dir}/next");
    let next = Backend::vopen_raw_impl(
        &mut client,
        &[Path::new(&next_path)],
        &[libc::O_CREAT | libc::O_RDWR],
        &[0o600],
    )
    .unwrap();
    assert_eq!(client.test_deferred_path_close_count(), 0);
    assert!(client.test_confirmed_path_closes() > 0);
    client.vclose_impl(&next).unwrap();
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_does_not_replay_exclusive_create_after_real_reply_loss() {
    let dir = setup_dir("openv_proxy_reply_loss");
    let proxy = DropReplyProxy::start(reply_loss_target());
    let endpoint = proxy.endpoint();
    let mut proxied = NfsVecFs::connect_with_options(&endpoint, {
        let mut options = NfsConnectOptions::default().auto_reconnect(false);
        options.minorversion = match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => Some(1),
            Ok("2") => Some(2),
            _ => None,
        };
        options.request_timeout = Duration::from_millis(500);
        options
    })
    .expect("connect through NFS reply-loss proxy");
    let paths = [format!("{dir}/f0"), format!("{dir}/f1")];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();

    proxy.arm();
    let error = Backend::vopen_raw_impl(
        &mut proxied,
        &refs,
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 2],
        &[0o644; 2],
    )
    .unwrap_err();
    proxy.wait_for_drop();

    assert!(error.is_transport(), "unexpected replay result: {error}");
    assert_eq!(error.index(), None);
    assert_eq!(proxied.test_open_handle_count(), 0);
    let mut admin = client();
    assert!(admin.exists_impl(Path::new(&paths[0])).unwrap());
    assert!(admin.exists_impl(Path::new(&paths[1])).unwrap());
    admin.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[test]
fn directory_visit_continuation_reuses_resolved_nfs_handle() {
    let dir = setup_dir("visit_dir_compound_count");
    let mut admin = client();
    for index in 0..80 {
        let path = format!("{dir}/entry-{index:03}-{}", "x".repeat(96));
        write_file(&mut admin, Path::new(&path), b"x");
    }
    let host = std::env::var("VNFS_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let mut raw = NfsVecFs::builder(&host)
        .max_compound_bytes(4 * 1024)
        .connect()
        .unwrap();
    let mask = AttrMask::MODE
        | AttrMask::SIZE
        | AttrMask::NLINK
        | AttrMask::FILEID
        | AttrMask::UID
        | AttrMask::GID
        | AttrMask::ATIME
        | AttrMask::MTIME
        | AttrMask::CTIME
        | AttrMask::CHANGE;
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let mut raw_count = 0;
    raw.visit_dir_impl(Path::new(&dir), mask, 0, &mut |_| {
        raw_count += 1;
        true
    })
    .unwrap();
    let raw_compounds = vfsi_nfs::compound::thread_compound_stats().0;

    let visitor = Nfs::builder(&host)
        .max_compound_bytes(4 * 1024)
        .connect()
        .unwrap();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let mut native_count = 0;
    visitor
        .visit_dir(&dir, |_| {
            native_count += 1;
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    let native_compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(raw_count, 80);
    assert_eq!(native_count, 80);
    assert!(
        raw_compounds > 3,
        "fixture must require multiple READDIR pages"
    );
    assert!(
        native_compounds <= raw_compounds,
        "native pagination must not re-resolve directories: native={native_compounds}, raw={raw_compounds}; EOF may avoid a final probe"
    );
    admin.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn directory_visit_recovers_if_reply_is_lost_before_first_entry() {
    let dir = setup_dir("visit_dir_proxy_reply_loss");
    let path = format!("{dir}/entry");
    let mut admin = client();
    write_file(&mut admin, Path::new(&path), b"data");

    let proxy = DropReplyProxy::start(reply_loss_target());
    let visitor = Nfs::builder(proxy.endpoint())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .request_timeout(Duration::from_millis(500))
        .connect()
        .unwrap();
    proxy.arm();
    let mut seen = Vec::new();
    visitor
        .visit_dir(&dir, |entry| {
            seen.push(entry.path().to_path_buf());
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .expect("read-only directory visit should reconnect before delivering entries");
    proxy.wait_for_drop();
    assert_eq!(seen, [PathBuf::from(path)]);
    admin.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn directory_visit_does_not_replay_after_delivering_an_entry() {
    let dir = setup_dir("visit_dir_no_partial_replay");
    let mut admin = client();
    let names: Vec<_> = (0..300)
        .map(|index| format!("{dir}/entry-{index:03}-{}", "x".repeat(96)))
        .collect();
    // Exceed the bounded 32 KiB wire page so the proxy drops an actual
    // continuation, rather than an unnecessary EOF probe.
    let writes: Vec<_> = names
        .iter()
        .map(|name| WriteOp::at(VfFile::from_path(name), 0, b"x".to_vec()).with_creation())
        .collect();
    admin.vwrite_owned_impl(&writes).unwrap();

    let proxy = DropReplyProxy::start(reply_loss_target());
    let visitor = Nfs::builder(proxy.endpoint())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .request_timeout(Duration::from_millis(500))
        .max_compound_bytes(8 * 1024)
        .connect()
        .unwrap();
    let mut delivered = 0;
    let mut unique = std::collections::HashSet::new();
    let error = visitor
        .visit_dir(&dir, |entry| {
            delivered += 1;
            assert!(unique.insert(entry.path().to_path_buf()), "entry replayed");
            if delivered == 1 {
                proxy.arm();
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap_err();
    proxy.wait_for_drop();
    assert!(error.is_transport());
    assert!(delivered > 0 && delivered <= names.len());
    admin.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn pipelined_read_recovers_after_a_lost_read_reply() {
    use vnfs::{Nfs, NfsReadPoolOptions};

    let dir = setup_dir("read_pool_proxy_reply_loss");
    let path = format!("{dir}/large.bin");
    let expected: Vec<u8> = (0usize..(2 * 1024 * 1024 + 19))
        .map(|index| (index.wrapping_mul(17) % 251) as u8)
        .collect();
    let mut admin = client();
    write_file(&mut admin, Path::new(&path), &expected);

    let proxy = DropReplyProxy::start(reply_loss_target());
    let mut pool = Nfs::builder(proxy.endpoint())
        .connect_read_pool(
            NfsReadPoolOptions::new()
                .worker_count(3)
                .chunk_size(32 * 1024)
                .max_in_flight(6)
                .max_buffered_bytes(6 * 32 * 1024),
        )
        .expect("connect read pool through NFS reply-loss proxy");

    let mut actual = Vec::with_capacity(expected.len());
    let mut first_chunk = true;
    pool.read_stream(&path, |_, data| {
        if first_chunk {
            first_chunk = false;
            proxy.arm();
        }
        actual.extend_from_slice(data);
        Ok(true)
    })
    .expect("read should recover after the lost read response");
    proxy.wait_for_drop();
    assert_eq!(actual, expected);
    admin.vremove_impl(&[VfFile::from_path(&path)]).unwrap();
}

#[test]
fn removev_partial_failure_resumes_from_failing_index() {
    // A whole-batch fallback would re-remove f0 (already removed by the
    // merged prefix) and fail with NOENT at index 0. Resume fails at the
    // missing path, index 1.
    let dir = setup_dir("removev_resume");
    let mut c = client();
    let f0 = format!("{}/f0", dir);
    let bad = format!("{}/missing", dir);
    let f2 = format!("{}/f2", dir);
    for p in [&f0, &f2] {
        write_file(&mut c, Path::new(p), b"x");
    }
    let files: Vec<VfFile> = [f0.as_str(), bad.as_str(), f2.as_str()]
        .iter()
        .map(|p| VfFile::from_path(p))
        .collect();
    let e = c.vremove_impl(&files).unwrap_err();
    assert_eq!(e.index(), Some(1), "resume must fail at the missing path");
    assert!(
        !c.exists_impl(Path::new(&f0)).unwrap(),
        "prefix was removed"
    );
    assert!(
        c.exists_impl(Path::new(&f2)).unwrap(),
        "suffix was not attempted"
    );
}

#[test]
fn listdirv_batches_many_directories() {
    // 10 sibling directories: resolve + READDIR in a few compounds instead of
    // one round trip per directory.
    let dir = setup_dir("listdirv_batch");
    let mut c = client();
    for i in 0..10 {
        c.ensure_dir_impl(Path::new(&format!("{}/d{}", dir, i)), 0o755)
            .unwrap();
        write_file(&mut c, Path::new(&format!("{}/d{}/f", dir, i)), b"x");
    }
    let dirs: Vec<String> = (0..10).map(|i| format!("{}/d{}", dir, i)).collect();
    let refs: Vec<&Path> = dirs.iter().map(Path::new).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let mut seen = 0usize;
    let mut cb = |_: &VfAttrs, _: &Path| {
        seen += 1;
        true
    };
    c.vlistdirs_impl(&refs, AttrMask::stat(), 0, false, &mut cb)
        .unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(seen, 10);
    assert!(
        compounds <= 6,
        "10 directories should list in a few compounds, got {}",
        compounds
    );
}

#[test]
fn native_read_dirs_batches_and_reports_bounded_errors() {
    use vnfs::{MetadataFields, ReadDirOptions};

    let dir = setup_dir("native_read_dirs_batch");
    let mut backend = client();
    for i in 0..10 {
        backend
            .ensure_dir_impl(Path::new(&format!("{dir}/d{i}")), 0o755)
            .unwrap();
        write_file(&mut backend, Path::new(&format!("{dir}/d{i}/f")), b"x");
    }
    let client = vfsi_sync::FsClient::new(backend);
    let directories: Vec<String> = (0..10).map(|i| format!("{dir}/d{i}")).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let listings = client
        .read_dirs_with_options(
            &directories,
            MetadataFields::MODE | MetadataFields::SIZE | MetadataFields::BLOCKS,
            ReadDirOptions::new(),
        )
        .unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert_eq!(listings.len(), 10);
    assert!(listings.iter().all(|listing| listing.entries.len() == 1));
    assert!(
        listings
            .iter()
            .all(|listing| listing.entries[0].metadata().blocks().is_some())
    );
    assert!(
        compounds <= 6,
        "expected batched directory listing, got {compounds} compounds"
    );

    let error = client
        .read_dirs_with_options(
            &directories,
            MetadataFields::MODE,
            ReadDirOptions::new().max_entries(5),
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let missing = [directories[0].clone(), format!("{dir}/missing")];
    let error = client
        .read_dirs_with_options(&missing, MetadataFields::MODE, ReadDirOptions::new())
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
}

#[test]
fn large_writev_readv_roundtrip() {
    // A single op larger than the server's per-op READ/WRITE limit must be
    // chunked across multiple READ/WRITE ops and round-trip correctly.
    let dir = setup_dir("large_rw");
    let mut c = client();
    let p = format!("{}/big.bin", dir);
    let data = vec![b'x'; 2 * 1024 * 1024 + 123];
    // The 2 MiB payload must travel as a single WRITE op (the XDR I/O cap is
    // 64 MiB, so the per-op limit is the compound budget, not 1 MiB).
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let w = c
        .vwrite_owned_impl(&[WriteOp::at(VfFile::from_path(&p), 0, data.clone()).with_creation()])
        .unwrap();
    assert_eq!(w[0].written, data.len());
    assert_eq!(
        vfsi_nfs::compound::thread_compound_stats().0,
        1,
        "writev must be 1 compound"
    );
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let r = c
        .vread_impl(&[ReadOp::at(VfFile::from_path(&p), 0, data.len())])
        .unwrap();
    assert_eq!(r[0].data, data);
    assert_eq!(
        vfsi_nfs::compound::thread_compound_stats().0,
        1,
        "readv must be 1 compound"
    );
    assert_eq!(c.stat_impl(Path::new(&p)).unwrap().size, data.len() as u64);
}

#[test]
fn read_allv_is_no_stat_whole_file_read() {
    // read_allv reads every file to EOF without a size fetch; a 2 MiB file
    // round-trips in one compound (chunked READ ops), and the compound
    // counter proves no stat compound was issued.
    let dir = setup_dir("read_all");
    let mut c = client();
    let mut files = Vec::new();
    for i in 0..4 {
        let p = format!("{}/f{}.bin", dir, i);
        let data = vec![b'a' + i as u8; 2 * 1024 * 1024 + 123];
        c.vwrite_owned_impl(&[WriteOp::at(VfFile::from_path(&p), 0, data.clone()).with_creation()])
            .unwrap();
        files.push((p, data));
    }
    let refs: Vec<VfFile> = files.iter().map(|(p, _)| VfFile::from_path(p)).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let out = c.vread_all_impl(&refs).unwrap();
    for (i, (_, data)) in files.iter().enumerate() {
        assert_eq!(&out[i], data);
    }
    // The whole batch fits one compound (4 x ~2 MiB requested, chunked into
    // per-op READs and byte-budgeted) — and no separate stat round trip.
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        compounds <= 4,
        "read_allv(4 x 2 MiB) must be a few compounds, got {}",
        compounds
    );
}

#[test]
fn read_allv_many_files_respects_aggregate_reply_budget() {
    let dir = setup_dir("read_all_many");
    let mut c = client();
    let data = vec![b'x'; 16 * 1024];
    let mut paths = Vec::new();
    for i in 0..8 {
        let p = format!("{}/f{}.bin", dir, i);
        write_file(&mut c, Path::new(&p), &data);
        paths.push(p);
    }
    let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_path(p)).collect();
    c.set_max_compound_bytes(256 * 1024);
    let out = c.vread_all_impl(&files).expect("read_allv many files");
    assert_eq!(out.len(), files.len());
    assert!(out.iter().all(|contents| contents == &data));
}

#[test]
fn mkdirv_batches_parent_resolution() {
    let dir = setup_dir("mkdirv_batch");
    let mut c = client();
    let paths: Vec<String> = (0..8).map(|i| format!("{}/d{}", dir, i)).collect();
    let attrs: Vec<VfAttrs> = paths
        .iter()
        .map(|p| VfAttrs {
            file: VfFile::from_path(p),
            masks: AttrMask::MODE,
            mode: 0o751,
            ..VfAttrs::default()
        })
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    c.vmkdir_impl(&attrs).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        compounds <= 8,
        "mkdirv of 8 dirs should batch parent resolution, got {}",
        compounds
    );
    for p in &paths {
        assert_eq!(c.stat_impl(Path::new(p)).unwrap().mode & 0o777, 0o751);
    }
}

#[test]
fn mkdirv_partial_failure_applies_prefix_modes() {
    // A mid-batch EEXIST must still apply the requested modes to the
    // directories that were created before the failure.
    let dir = setup_dir("mkdirv_resume");
    let mut c = client();
    let d1 = format!("{}/d1", dir);
    c.mkdir_raw_impl(Path::new(&d1), 0o755).unwrap();
    let d0 = format!("{}/d0", dir);
    let d2 = format!("{}/d2", dir);
    let attrs: Vec<VfAttrs> = [d0.as_str(), d1.as_str(), d2.as_str()]
        .iter()
        .map(|p| VfAttrs {
            file: VfFile::from_path(p),
            masks: AttrMask::MODE,
            mode: 0o711,
            ..VfAttrs::default()
        })
        .collect();
    let e = c.vmkdir_impl(&attrs).unwrap_err();
    assert_eq!(e.index(), Some(1), "EEXIST on the pre-created directory");
    assert_eq!(
        c.stat_impl(Path::new(&d0)).unwrap().mode & 0o777,
        0o711,
        "prefix mode applied despite the failure"
    );
    assert!(!c.exists_impl(Path::new(&d2)).unwrap());
}

#[test]
fn symlinkv_readlinkv_hardlinkv_batch_resolution() {
    let dir = setup_dir("link_batch");
    let mut c = client();
    // 5 symlinks in one directory: one batched parent resolve + one CREATE.
    let targets: Vec<String> = (0..5).map(|i| format!("target{}", i)).collect();
    let links: Vec<String> = (0..5).map(|i| format!("{}/l{}", dir, i)).collect();
    let t_refs: Vec<&Path> = targets.iter().map(Path::new).collect();
    let l_refs: Vec<&Path> = links.iter().map(Path::new).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    c.vsymlink_impl(&t_refs, &l_refs).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        compounds <= 4,
        "symlinkv of 5 links should batch parent resolution, got {}",
        compounds
    );
    // readlinkv of all links: one batched resolve + one READLINK.
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    let got = c.vreadlink_impl(&l_refs).unwrap();
    assert_eq!(got.len(), 5);
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        compounds <= 4,
        "readlinkv of 5 links should batch resolution, got {}",
        compounds
    );
    // hardlinkv: sources + destination parents batched, then one LINK.
    let hard: Vec<String> = (0..5).map(|i| format!("{}/h{}", dir, i)).collect();
    let h_refs: Vec<&Path> = hard.iter().map(Path::new).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats(); // reset counters
    c.vhardlink_impl(&l_refs, &h_refs).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        compounds <= 7,
        "hardlinkv of 5 links should batch resolution, got {}",
        compounds
    );
    for (i, h) in hard.iter().enumerate() {
        assert_eq!(
            c.readlink_raw_impl(Path::new(h)).unwrap(),
            targets[i].as_bytes()
        );
    }
}

#[test]
fn openv_ocreat_preserves_existing_mode() {
    let dir = setup_dir("openv_mode");
    let mut c = client();
    let f = format!("{}/existing.txt", dir);
    let fd = c
        .open_raw_impl(Path::new(&f), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    c.close_impl(&fd).unwrap();

    let g = format!("{}/new.txt", dir);
    Backend::vopen_raw_impl(
        &mut c,
        &[Path::new(&f), Path::new(&g)],
        &[libc::O_CREAT | libc::O_RDWR, libc::O_CREAT | libc::O_RDWR],
        &[0o777, 0o640],
    )
    .unwrap();
    assert_eq!(
        c.stat_impl(Path::new(&f)).unwrap().mode & 0o7777,
        0o600,
        "openv O_CREAT must not chmod an existing file"
    );
    assert_eq!(
        c.stat_impl(Path::new(&g)).unwrap().mode & 0o7777,
        0o640,
        "new file mode"
    );
}

#[test]
fn transport_error_on_unreachable_server() {
    let e = match NfsVecFs::connect("127.0.0.1:1") {
        Ok(_) => panic!("must fail to connect"),
        Err(e) => e,
    };
    assert!(e.is_transport(), "unreachable server is a transport error");
    assert_eq!(e.err_no(), VF_ERR_RPC);
}

// ---------------------------------------------------------------------------
// hardlink
// ---------------------------------------------------------------------------

#[test]
fn hardlinkv() {
    let dir = setup_dir("hardlink");
    let src = format!("{}/orig.txt", dir);
    let dst = format!("{}/hard.txt", dir);
    let mut c = make_file(&src, b"linked data");
    let olds = [Path::new(&src)];
    let news = [Path::new(&dst)];
    c.vhardlink_impl(&olds, &news).expect("hardlinkv");
    assert!(c.exists_impl(Path::new(&dst)).unwrap());
    let s1 = c.stat_impl(Path::new(&src)).unwrap();
    let s2 = c.stat_impl(Path::new(&dst)).unwrap();
    assert_eq!(s1.fileid, s2.fileid, "same inode");
    assert!(s1.nlink >= 2, "nlink bumped to >= 2");
}

// ---------------------------------------------------------------------------
// ensure_dir / rm
// ---------------------------------------------------------------------------

#[test]
fn ensure_dir() {
    let dir = setup_dir("ensure_dir");
    let nested = format!("{}/a/b/c/d", dir);
    let mut c = client();
    c.ensure_dir_impl(Path::new(&nested), 0o755)
        .expect("ensure_dir nested");
    assert!(c.exists_impl(Path::new(&format!("{}/a", dir))).unwrap());
    assert!(c.exists_impl(Path::new(&nested)).unwrap());
}

#[test]
fn rm_recursive_api() {
    let dir = setup_dir("rm_rec");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/x/y", dir)), 0o755)
        .unwrap();
    for f in [
        format!("{}/top", dir),
        format!("{}/x/deep", dir),
        format!("{}/x/y/deep2", dir),
    ] {
        c.vwrite_owned_impl(&[
            WriteOp::from_path(&f, VfOffset::At(0), b"d".to_vec()).with_creation()
        ])
        .unwrap();
    }
    assert!(
        c.exists_impl(Path::new(&format!("{}/x/deep", dir)))
            .unwrap()
    );
    vfsi_sync::backend_helpers::remove_tree(&mut c, Path::new(&dir)).expect("rm_recursive");
    assert!(
        !c.exists_impl(Path::new(&dir)).unwrap(),
        "whole tree removed"
    );
}

#[test]
fn rm_contents_keeps_the_directory() {
    let dir = setup_dir("rm_contents");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/x/y", dir)), 0o755)
        .unwrap();
    write_file(&mut c, Path::new(&format!("{}/top", dir)), b"t");
    write_file(&mut c, Path::new(&format!("{}/x/deep", dir)), b"d");
    write_file(&mut c, Path::new(&format!("{}/x/y/deep2", dir)), b"e");

    c.remove_dir_contents_path_impl(Path::new(&dir)).unwrap();

    assert!(c.exists_impl(Path::new(&dir)).unwrap(), "root is kept");
    assert!(
        c.listdir_impl(Path::new(&dir), vfsi_sync::AttrMask::default(), 0, false)
            .unwrap()
            .is_empty(),
        "contents are gone"
    );
}

#[test]
fn rm_contents_rejects_a_symlink_to_a_directory() {
    let dir = setup_dir("rm_contents_symlink");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/target", dir)), 0o755)
        .unwrap();
    write_file(&mut c, Path::new(&format!("{}/target/keep", dir)), b"k");
    c.symlink_raw_impl(Path::new("target"), Path::new(&format!("{}/link", dir)))
        .unwrap();

    assert!(
        c.remove_dir_contents_path_impl(Path::new(&format!("{}/link", dir)))
            .is_err(),
        "a symlink to a directory must not be emptied"
    );
    assert!(
        c.exists_impl(Path::new(&format!("{}/target/keep", dir)))
            .unwrap(),
        "the symlink target is preserved"
    );
}

#[test]
fn recursive_rm_removes_a_symlink_not_its_target() {
    let dir = setup_dir("rm_symlink");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/target", dir)), 0o755)
        .unwrap();
    write_file(&mut c, Path::new(&format!("{}/target/keep", dir)), b"k");
    c.symlink_raw_impl(Path::new("target"), Path::new(&format!("{}/link", dir)))
        .unwrap();

    c.remove_paths_impl(&[Path::new(&format!("{}/link", dir))], true)
        .unwrap();
    assert!(
        !c.exists_impl(Path::new(&format!("{}/link", dir))).unwrap(),
        "the symlink itself is removed"
    );
    assert!(
        c.exists_impl(Path::new(&format!("{}/target/keep", dir)))
            .unwrap(),
        "the symlink target is preserved"
    );
}

#[test]
fn ensure_empty_dir_creates_empties_and_rejects_files() {
    let dir = setup_dir("ensure_empty");
    let mut c = client();
    let missing = format!("{}/made", dir);
    c.ensure_empty_dir_impl(Path::new(&missing)).unwrap();
    assert!(c.exists_impl(Path::new(&missing)).unwrap());
    assert!(
        c.listdir_impl(
            Path::new(&missing),
            vfsi_sync::AttrMask::default(),
            0,
            false
        )
        .unwrap()
        .is_empty()
    );

    c.ensure_dir_impl(Path::new(&format!("{}/full", dir)), 0o755)
        .unwrap();
    write_file(&mut c, Path::new(&format!("{}/full/child", dir)), b"x");
    c.ensure_empty_dir_impl(Path::new(&format!("{}/full", dir)))
        .unwrap();
    assert!(
        c.listdir_impl(
            Path::new(&format!("{}/full", dir)),
            vfsi_sync::AttrMask::default(),
            0,
            false
        )
        .unwrap()
        .is_empty()
    );

    let file = format!("{}/afile", dir);
    write_file(&mut c, Path::new(&file), b"f");
    assert!(c.ensure_empty_dir_impl(Path::new(&file)).is_err());
    assert!(c.exists_impl(Path::new(&file)).unwrap());
}

#[test]
fn open_dir_handle_empties_without_following_symlinks() {
    let dir = setup_dir("open_dir");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/d/sub", dir)), 0o755)
        .unwrap();
    write_file(&mut c, Path::new(&format!("{}/d/a", dir)), b"a");
    write_file(&mut c, Path::new(&format!("{}/d/sub/b", dir)), b"b");

    let handle = c.open_dir_impl(Path::new(&format!("{}/d", dir))).unwrap();
    c.remove_dir_contents_handle_impl(&handle).unwrap();
    c.close_dir_impl(&handle).unwrap();
    assert!(
        c.exists_impl(Path::new(&format!("{}/d", dir))).unwrap(),
        "kept"
    );
    assert!(
        c.listdir_impl(
            Path::new(&format!("{}/d", dir)),
            vfsi_sync::AttrMask::default(),
            0,
            false
        )
        .unwrap()
        .is_empty()
    );

    c.symlink_raw_impl(Path::new("d"), Path::new(&format!("{}/link", dir)))
        .unwrap();
    assert!(
        c.open_dir_impl(Path::new(&format!("{}/link", dir)))
            .is_err(),
        "opening a symlink to a directory is rejected"
    );
}

#[test]
fn directory_handle_is_bound_to_its_client() {
    let dir = setup_dir("dir_handle_owner");
    let mut a = client();
    let mut b = client();
    for name in ["a", "b"] {
        a.ensure_dir_impl(Path::new(&format!("{dir}/{name}")), 0o755)
            .unwrap();
        write_file(&mut a, Path::new(&format!("{dir}/{name}/keep")), b"x");
    }
    let a_handle = a.open_dir_impl(Path::new(&format!("{dir}/a"))).unwrap();
    let b_handle = b.open_dir_impl(Path::new(&format!("{dir}/b"))).unwrap();
    assert!(b.remove_dir_contents_handle_impl(&a_handle).is_err());
    assert!(b.close_dir_impl(&a_handle).is_err());
    assert!(b.exists_impl(Path::new(&format!("{dir}/b/keep"))).unwrap());
    b.remove_dir_contents_handle_impl(&b_handle).unwrap();
    b.close_dir_impl(&b_handle).unwrap();
    assert!(!b.exists_impl(Path::new(&format!("{dir}/b/keep"))).unwrap());
    assert!(a.exists_impl(Path::new(&format!("{dir}/a/keep"))).unwrap());
    a.close_dir_impl(&a_handle).unwrap();
}

#[test]
fn directory_handle_survives_reconnect() {
    let dir = setup_dir("dir_handle_reconnect");
    let mut c = client();
    let child = format!("{dir}/keep");
    write_file(&mut c, Path::new(&child), b"x");
    let handle = c.open_dir_impl(Path::new(&dir)).unwrap();
    c.reconnect().unwrap();
    c.remove_dir_contents_handle_impl(&handle).unwrap();
    c.close_dir_impl(&handle).unwrap();
    assert!(!c.exists_impl(Path::new(&child)).unwrap());
}

#[test]
fn open_dir_accepts_namespace_root() {
    let mut c = client();
    let handle = c.open_dir_impl(Path::new("/")).unwrap();
    c.close_dir_impl(&handle).unwrap();
}

#[test]
fn owned_directory_handle_survives_rename_and_exposes_options() {
    use vfsi_sync::FsClient;
    use vnfs::RemoveOptions;

    let root = setup_dir("owned_remove_dir");
    let original = format!("{root}/original");
    let moved = format!("{root}/moved");
    let file = format!("{moved}/file");
    let c = FsClient::new(client());
    c.create_dir(&original).unwrap();
    c.write(format!("{original}/file"), b"data").unwrap();

    let handle = c.open_dir_handle(&original).unwrap();
    c.rename(&original, &moved).unwrap();
    handle
        .remove_contents_with_options(RemoveOptions::new().batch(2))
        .unwrap();
    assert_eq!(handle.path(), Path::new(&original));
    assert!(c.read_dir(&moved).unwrap().is_empty());
    assert!(c.metadata(&file).is_err());
    handle.close().unwrap();
    let extra = format!("{moved}/extra");
    c.write(&extra, b"data").unwrap();
    c.vremove_with_options_native(&[&extra], false, RemoveOptions::new().batch(2))
        .unwrap();
    assert!(c.metadata(&extra).is_err());
    c.remove_dir_all_with_options(&moved, RemoveOptions::new().batch(2))
        .unwrap();
    assert!(c.metadata(&moved).is_err());
}

#[test]
fn recursive_removal_drains_large_directory_and_nested_children() {
    use vfsi_sync::FsClient;
    use vnfs::RemoveOptions;

    let root = setup_dir("paged_remove_dir");
    let c = FsClient::new(client());
    for index in 0..1200 {
        c.write(format!("{root}/file-{index:04}"), b"x").unwrap();
    }
    for index in 0..24 {
        let child = format!("{root}/dir-{index:03}");
        c.create_dir(&child).unwrap();
        c.write(format!("{child}/file"), b"x").unwrap();
    }
    c.remove_dir_contents_with_options(&root, RemoveOptions::new().batch(32))
        .unwrap();
    assert!(c.read_dir(&root).unwrap().is_empty());
    c.remove_dir_all(&root).unwrap();
}

#[test]
fn rm_is_fail_fast_but_explicit_best_effort_continues() {
    let dir = setup_dir("rm_fail_fast");
    let mut c = client();
    let missing = format!("{dir}/missing/leaf");
    let victim = format!("{dir}/victim");
    write_file(&mut c, Path::new(&victim), b"x");
    assert!(
        c.remove_paths_impl(&[Path::new(&missing), Path::new(&victim)], true)
            .is_err()
    );
    assert!(c.exists_impl(Path::new(&victim)).unwrap());
    assert!(
        c.remove_paths_with_options_impl(
            &[Path::new(&missing), Path::new(&victim)],
            true,
            vfsi_sync::RemoveOptions::new().continue_on_error(true),
        )
        .is_err()
    );
    assert!(!c.exists_impl(Path::new(&victim)).unwrap());
}

#[test]
fn rm_with_options_honors_batch_size() {
    let dir = setup_dir("rm_options");
    let mut c = client();
    for batch in [1, 2, 8] {
        let root = format!("{dir}/batch-{batch}");
        c.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
        for index in 0..17 {
            write_file(&mut c, Path::new(&format!("{root}/f{index}")), b"x");
        }
        let _ = vfsi_nfs::compound::thread_compound_stats();
        c.remove_paths_with_options_impl(
            &[Path::new(&root)],
            true,
            vfsi_sync::RemoveOptions::new().batch(batch),
        )
        .unwrap();
        let (compounds, _, _, max_ops) = vfsi_nfs::compound::thread_compound_stats();
        // A flat directory costs SEQUENCE + PUTFH + up to `batch`
        // REMOVEs. Lookup/READDIR compounds have at most six operations.
        assert!(
            max_ops <= (batch + 2).max(6) as u64,
            "batch={batch}, max_ops={max_ops}"
        );
        assert!(compounds >= 17usize.div_ceil(batch) as u64);
        assert!(!c.exists_impl(Path::new(&root)).unwrap());
    }
    c.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[test]
fn recursive_rm_learns_larger_batches_from_useful_compounds() {
    let dir = setup_dir("rm_adaptive_batches");
    let mut c = client();
    let adaptive = format!("{dir}/adaptive");
    let fixed = format!("{dir}/fixed");
    for subdir in [&adaptive, &fixed] {
        c.ensure_dir_impl(Path::new(subdir), 0o755).unwrap();
        for index in 0..160 {
            write_file(&mut c, Path::new(&format!("{subdir}/f{index:03}")), b"x");
        }
    }

    let _ = vfsi_nfs::compound::thread_compound_stats();
    c.remove_paths_impl(&[Path::new(&adaptive)], true).unwrap();
    let learned_compounds = vfsi_nfs::compound::thread_compound_stats().0;

    let _ = vfsi_nfs::compound::thread_compound_stats();
    c.remove_paths_with_options_impl(
        &[Path::new(&fixed)],
        true,
        vfsi_sync::RemoveOptions::new().batch(24),
    )
    .unwrap();
    let fixed_compounds = vfsi_nfs::compound::thread_compound_stats().0;

    assert!(
        learned_compounds < fixed_compounds,
        "adaptive batching should use fewer compounds than fixed-size batching: {learned_compounds} vs {fixed_compounds}"
    );
}

#[test]
fn rm_nonrecursive_keeps_subdirs() {
    let dir = setup_dir("rm_norec");
    let mut c = client();
    let file = format!("{}/keepdir/target", dir);
    c.ensure_dir_impl(Path::new(&format!("{}/keepdir", dir)), 0o755)
        .unwrap();
    c.vwrite_owned_impl(&[
        WriteOp::from_path(&file, VfOffset::At(0), b"k".to_vec()).with_creation()
    ])
    .unwrap();
    // Non-recursive removal of the directory fails because it is not empty.
    let r = c.remove_paths_impl(&[Path::new(&dir)], false);
    assert!(
        r.is_err(),
        "non-empty dir cannot be removed non-recursively"
    );
    assert!(c.exists_impl(Path::new(&file)).unwrap());
}

// ---------------------------------------------------------------------------
// Helpers used by the new-call tests
// ---------------------------------------------------------------------------

fn write_file(c: &mut NfsVecFs, path: &Path, data: &[u8]) {
    c.vwrite_owned_impl(&[
        WriteOp::from_os_path(path, VfOffset::At(0), data.to_vec()).with_creation()
    ])
    .unwrap();
}

fn read_all(c: &mut NfsVecFs, path: &Path) -> Vec<u8> {
    let size = c.stat_impl(path).expect("stat").size as usize;
    let r = &c
        .vread_impl(&[ReadOp::from_os_path(path, VfOffset::At(0), size)])
        .expect("readv")[0];
    assert_eq!(r.data.len(), size, "read full file");
    r.data.clone()
}

// ---------------------------------------------------------------------------
// tc_openv (per-file flags and modes)
// ---------------------------------------------------------------------------

#[test]
fn openv_per_file_flags_and_modes() {
    let dir = setup_dir("openv_flags");
    let mut c = client();
    let paths = [format!("{}/a.txt", dir), format!("{}/b.txt", dir)];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let flags = [libc::O_CREAT | libc::O_RDWR, libc::O_CREAT | libc::O_RDONLY];
    let modes = [0o600, 0o640];
    let files = Backend::vopen_raw_impl(&mut c, &refs, &flags, &modes).expect("openv");
    assert_eq!(files.len(), 2);
    assert_eq!(
        c.stat_impl(Path::new(&paths[0])).unwrap().mode & 0o777,
        0o600
    );
    assert_eq!(
        c.stat_impl(Path::new(&paths[1])).unwrap().mode & 0o777,
        0o640
    );
    c.vclose_impl(&files).unwrap();
}

// ---------------------------------------------------------------------------
// tc_fseek
// ---------------------------------------------------------------------------

#[test]
fn fseek_set_cur_end() {
    let dir = setup_dir("fseek");
    let f = format!("{}/seek.bin", dir);
    let mut c = client();
    let tf = c
        .open_raw_impl(Path::new(&f), libc::O_CREAT | libc::O_RDWR, 0o644)
        .expect("open");

    let payload = b"0123456789".to_vec();
    c.vwrite_owned_impl(&[WriteOp::from_fd(
        tf.fd().unwrap(),
        VfOffset::At(0),
        payload.clone(),
    )])
    .unwrap();
    assert_eq!(c.seek_raw_impl(&tf, 0, SeekFrom::End).unwrap(), 10);

    // SeekFrom::Set then read via VfOffset::Cur.
    assert_eq!(c.seek_raw_impl(&tf, 4, SeekFrom::Set).unwrap(), 4);
    let r = &c
        .vread_impl(&[ReadOp::from_fd(tf.fd().unwrap(), VfOffset::Cur, 6)])
        .unwrap()[0];
    assert_eq!(r.data, b"456789", "read at current offset after fseek");

    // SeekFrom::Cur advances from the tracked offset (4 + 6 = 10).
    assert_eq!(c.seek_raw_impl(&tf, -4, SeekFrom::Cur).unwrap(), 6);
    let r = &c
        .vread_impl(&[ReadOp::from_fd(tf.fd().unwrap(), VfOffset::Cur, 4)])
        .unwrap()[0];
    assert_eq!(r.data, b"6789");

    // Seeking before the start is refused.
    assert!(c.seek_raw_impl(&tf, -100, SeekFrom::Set).is_err());
    c.close_impl(&tf).unwrap();
}

// ---------------------------------------------------------------------------
// tc_dupv / tc_copyv (extent copy)
// ---------------------------------------------------------------------------

#[test]
fn dupv_copies_extent() {
    let dir = setup_dir("dupv");
    let src = format!("{}/src.bin", dir);
    let dst = format!("{}/dst.bin", dir);
    let mut c = client();
    write_file(&mut c, Path::new(&src), b"abcdefghij");

    let pairs = [ExtentPair::new(&src, 4, &dst, 0, Some(4))];
    c.vcopy_data_impl(&pairs).expect("dupv");
    assert_eq!(read_all(&mut c, Path::new(&dst)), b"efgh");

    // ExtentPair length None copies to end-of-file.
    let whole = format!("{}/whole.bin", dir);
    let mut c42 = NfsVecFs::connect_minor(&test_host(), 2).expect("connect with NFSv4.2");
    c42.vcopy_impl(
        &[ExtentPair::new(&src, 2, &whole, 0, None)],
        vfsi_core::CopyOption::new(),
    )
    .expect("copyv whole file");
    assert_eq!(read_all(&mut c42, Path::new(&whole)), b"cdefghij");
}

#[test]
fn copyv_batches_nfs42_server_copies_or_falls_back() {
    let dir = setup_dir("copyv42");
    let mut c = NfsVecFs::connect_minor(&test_host(), 2).expect("connect with NFSv4.2");
    let mut pairs = Vec::new();
    // More than two internal eight-file batches proves that COPY batching
    // continues correctly across compound boundaries.
    for i in 0..17 {
        let src = format!("{}/src{}", dir, i);
        let dst = format!("{}/dst{}", dir, i);
        write_file(&mut c, Path::new(&src), format!("payload-{i}").as_bytes());
        pairs.push(ExtentPair::new(&src, 0, &dst, 0, None));
    }
    let before = c.server_copy_stats();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    c.vcopy_impl(&pairs, vfsi_core::CopyOption::new())
        .expect("batched NFSv4.2 COPY");
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    let server_copy_enabled = c.server_copy_enabled();
    let copy_stats = c.server_copy_stats();
    if std::env::var_os("VNFS_TEST_REQUIRE_SERVER_COPY").is_some() {
        assert!(
            server_copy_enabled,
            "server rejected NFSv4.2 COPY and forced the client-side fallback"
        );
        assert!(
            copy_stats.requests > before.requests,
            "no COPY request sent"
        );
        assert!(
            copy_stats.operations - before.operations >= pairs.len() as u64,
            "server acknowledged too few COPY operations: {copy_stats:?}"
        );
        assert_eq!(
            copy_stats.fallbacks, before.fallbacks,
            "client-side COPY fallback was used: {copy_stats:?}"
        );
    }
    // Some NFSv4.2 servers negotiate the protocol but reject COPY at runtime.
    // In that case copyv disables server-side copying and transparently
    // retries through dupv; only enforce batching when COPY stayed enabled.
    if server_copy_enabled {
        assert!(
            compounds < (pairs.len() * 4) as u64,
            "copyv did not amortize RPCs: {compounds} compounds"
        );
    }
    for i in 0..17 {
        let dst = format!("{}/dst{}", dir, i);
        assert_eq!(
            read_all(&mut c, Path::new(&dst)),
            format!("payload-{i}").into_bytes()
        );
    }
}

#[test]
fn copyv_extent_semantics_and_server_telemetry() {
    let dir = setup_dir("copyv_extents");
    let mut c = NfsVecFs::connect_minor(&test_host(), 2).expect("connect with NFSv4.2");

    let digits = format!("{dir}/digits");
    write_file(&mut c, Path::new(&digits), b"0123456789");

    let partial = format!("{dir}/partial");
    let offset = format!("{dir}/offset");
    let past_eof = format!("{dir}/past-eof");
    let through_eof = format!("{dir}/through-eof");
    let zero = format!("{dir}/zero");
    for path in [&partial, &offset, &past_eof, &through_eof, &zero] {
        write_file(&mut c, Path::new(path), b"stale-trailing-data");
    }

    let sparse = format!("{dir}/sparse");
    c.vwrite_owned_impl(&[WriteOp::from_path(
        &sparse,
        VfOffset::At(1024 * 1024),
        b"tail".to_vec(),
    )
    .with_creation()
    .with_truncate()])
        .expect("create sparse source");
    let sparse_copy = format!("{dir}/sparse-copy");

    let pairs = [
        ExtentPair::new(&digits, 2, &partial, 0, Some(4)),
        ExtentPair::new(&digits, 3, &offset, 4, Some(3)),
        ExtentPair::new(&digits, 8, &past_eof, 0, Some(20)),
        ExtentPair::new(&digits, 5, &through_eof, 0, None),
        ExtentPair::new(&digits, 0, &zero, 0, Some(0)),
        ExtentPair::new(&sparse, 0, &sparse_copy, 0, None),
    ];
    let before = c.server_copy_stats();
    for (i, pair) in pairs.iter().enumerate() {
        c.vcopy_impl(std::slice::from_ref(pair), vfsi_core::CopyOption::new())
            .unwrap_or_else(|error| panic!("copy extent variant {i}: {error:?}"));
        if std::env::var_os("VNFS_TEST_REQUIRE_SERVER_COPY").is_some() {
            assert!(
                c.server_copy_enabled(),
                "copy extent variant {i} used client fallback"
            );
        }
    }
    let after = c.server_copy_stats();

    assert_eq!(read_all(&mut c, Path::new(&partial)), b"2345");
    assert_eq!(read_all(&mut c, Path::new(&offset)), b"stal345");
    assert_eq!(read_all(&mut c, Path::new(&past_eof)), b"89");
    assert_eq!(read_all(&mut c, Path::new(&through_eof)), b"56789");
    assert_eq!(read_all(&mut c, Path::new(&zero)), b"");
    let sparse_data = read_all(&mut c, Path::new(&sparse_copy));
    assert_eq!(sparse_data.len(), 1024 * 1024 + 4);
    assert!(sparse_data[..1024 * 1024].iter().all(|byte| *byte == 0));
    assert_eq!(&sparse_data[1024 * 1024..], b"tail");

    if std::env::var_os("VNFS_TEST_REQUIRE_SERVER_COPY").is_some() {
        assert!(c.server_copy_enabled());
        assert!(after.requests > before.requests);
        assert!(after.operations - before.operations >= 5);
        assert_eq!(after.fallbacks, before.fallbacks);

        // Ganesha rejects both same-file and overlapping COPY requests. Make
        // sure those errors are surfaced without corrupting or truncating the
        // source file.
        let same = format!("{dir}/same");
        for (src_offset, dst_offset, length) in [(0, 10, 5), (0, 2, 6)] {
            write_file(&mut c, Path::new(&same), b"abcdefghij");
            let error = c
                .vcopy_impl(
                    &[ExtentPair::new(
                        &same,
                        src_offset,
                        &same,
                        dst_offset,
                        Some(length),
                    )],
                    vfsi_core::CopyOption::new(),
                )
                .expect_err("same-file server COPY must be rejected");
            assert!(matches!(error.err_no(), ERR_EXIST | ERR_INVAL));
            assert_eq!(read_all(&mut c, Path::new(&same)), b"abcdefghij");
        }
    }
}

#[test]
fn copyv_reports_mid_batch_failure_index() {
    let dir = setup_dir("copyv_failure");
    let mut c = client();
    let src0 = format!("{dir}/src0");
    let missing = format!("{dir}/missing");
    let src2 = format!("{dir}/src2");
    let dst0 = format!("{dir}/dst0");
    let dst1 = format!("{dir}/dst1");
    let dst2 = format!("{dir}/dst2");
    write_file(&mut c, Path::new(&src0), b"zero");
    write_file(&mut c, Path::new(&src2), b"two");
    let error = c
        .vcopy_impl(
            &[
                ExtentPair::new(&src0, 0, &dst0, 0, None),
                ExtentPair::new(&missing, 0, &dst1, 0, None),
                ExtentPair::new(&src2, 0, &dst2, 0, None),
            ],
            vfsi_core::CopyOption::new(),
        )
        .expect_err("missing middle source must fail");
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::ENOENT as u32);
    assert!(!c.exists_impl(Path::new(&dst2)).unwrap());
}

#[test]
fn copyv_falls_back_on_nfs41() {
    let dir = setup_dir("copyv41");
    let src = format!("{}/src", dir);
    let dst = format!("{}/dst", dir);
    let mut c = NfsVecFs::connect_minor(&test_host(), 1).expect("connect with NFSv4.1");
    assert_eq!(c.minorversion(), 1);
    assert!(!c.server_copy_enabled());
    write_file(&mut c, Path::new(&src), b"client-side-fallback");
    c.vcopy_impl(
        &[ExtentPair::new(&src, 0, &dst, 0, None)],
        vfsi_core::CopyOption::new(),
    )
    .expect("v4.1 copyv fallback");
    assert_eq!(read_all(&mut c, Path::new(&dst)), b"client-side-fallback");
}

#[cfg(feature = "test-faults")]
#[test]
fn client_side_copy_closes_source_when_destination_open_fails() {
    let dir = setup_dir("copy_destination_open_failure");
    let src = format!("{dir}/src");
    // The destination parent resolves, but opening the directory itself for
    // write fails after the source OPEN has succeeded.
    let dst = dir.clone();
    let mut client = NfsVecFs::connect_minor(&test_host(), 1).unwrap();
    write_file(&mut client, Path::new(&src), b"source");
    let before = client.test_confirmed_path_closes();
    let error = client
        .vcopy_impl(
            &[ExtentPair::new(&src, 0, &dst, 0, None)],
            vfsi_core::CopyOption::new(),
        )
        .unwrap_err();
    assert!(!error.is_transport());
    assert_eq!(client.test_confirmed_path_closes(), before + 1);
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn client_side_copy_surfaces_close_failure_and_continues_cleanup() {
    let dir = setup_dir("copy_close_failure");
    let src = format!("{dir}/src");
    let dst = format!("{dir}/dst");
    let mut client = NfsVecFs::connect_minor(&test_host(), 1).unwrap();
    write_file(&mut client, Path::new(&src), b"source");
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforePathClose,
        VfError::transport(None, "injected source close failure"),
    ));
    client.set_fault_injector(script.clone());
    let before = client.test_confirmed_path_closes();
    let error = client
        .vcopy_impl(
            &[ExtentPair::new(&src, 0, &dst, 0, None)],
            vfsi_core::CopyOption::new(),
        )
        .unwrap_err();
    assert!(error.is_transport());
    assert!(script.is_consumed());
    assert_eq!(
        client.test_confirmed_path_closes(),
        before + 1,
        "destination cleanup must still run after source CLOSE fails"
    );
    client.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[test]
fn extent_copy_options_regular_files() {
    let dir = setup_dir("copy-options");
    let src = format!("{}/s.txt", dir);
    let d1 = format!("{}/d1.txt", dir);
    let d2 = format!("{}/d2.txt", dir);
    let mut c = client();
    write_file(&mut c, Path::new(&src), b"0123456789");
    c.vcopy_data_impl(&[ExtentPair::new(&src, 0, &d1, 0, Some(5))])
        .unwrap();
    c.vcopy_impl(
        &[ExtentPair::new(&src, 5, &d2, 0, Some(5))],
        vfsi_core::CopyOption::new().follow_source_symlinks(false),
    )
    .unwrap();
    assert_eq!(read_all(&mut c, Path::new(&d1)), b"01234");
    assert_eq!(read_all(&mut c, Path::new(&d2)), b"56789");
}

// ---------------------------------------------------------------------------
// tc_write_adb
// ---------------------------------------------------------------------------

#[test]
fn write_adb_blocknums_and_pattern() {
    let dir = setup_dir("adb");
    let f = format!("{}/adb.bin", dir);
    let mut c = client();

    // Three ADB blocks of 1024 bytes; write the ADBN (8 bytes, BE) at the
    // start of each block, and the pattern "PAT" 8 bytes into each block.
    let a = Adb {
        path: PathBuf::from(f.clone()),
        adb_offset: 0,
        adb_block_size: 1024,
        adb_block_count: 3,
        adb_reloff_blocknum: Some(0),
        adb_block_num: 100,
        adb_reloff_pattern: Some(8),
        adb_pattern_data: b"PAT".to_vec(),
    };
    let counts = c.vwrite_adb_impl(&[a]).expect("write_adb");
    assert_eq!(counts[0], 3, "all blocks written");

    for (i, expected_adbn) in [100u64, 101, 102].iter().enumerate() {
        let base = i as u64 * 1024;
        let bn = &c
            .vread_impl(&[ReadOp::from_path(&f, VfOffset::At(base), 8)])
            .unwrap()[0];
        assert_eq!(
            u64::from_be_bytes(bn.data[0..8].try_into().unwrap()),
            *expected_adbn,
            "ADBN of block {}",
            i
        );
        let pt = &c
            .vread_impl(&[ReadOp::from_path(&f, VfOffset::At(base + 8), 3)])
            .unwrap()[0];
        assert_eq!(pt.data, b"PAT", "pattern of block {}", i);
    }
}

// ---------------------------------------------------------------------------
// tc_listdirv (callback)
// ---------------------------------------------------------------------------

#[test]
fn listdirv_callback() {
    let dir = setup_dir("listdirv");
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/sub", dir)), 0o755)
        .unwrap();
    for name in ["a.txt", "b.txt"] {
        write_file(&mut c, Path::new(&format!("{}/{}", dir, name)), b"x");
    }

    let mut seen: Vec<String> = Vec::new();
    let mut cb = |e: &VfAttrs, d: &Path| {
        assert_eq!(d, Path::new(&dir));
        seen.push(e.file.path().unwrap().to_string_lossy().into_owned());
        true
    };
    c.vlistdirs_impl(&[Path::new(&dir)], AttrMask::default(), 0, false, &mut cb)
        .expect("listdirv");
    assert!(seen.iter().any(|p| p.ends_with("a.txt")));
    assert!(seen.iter().any(|p| p.ends_with("sub")));

    // A callback returning false stops early.
    let mut count = 0usize;
    let mut stop = |_: &VfAttrs, _: &Path| {
        count += 1;
        false
    };
    c.vlistdirs_impl(&[Path::new(&dir)], AttrMask::default(), 0, false, &mut stop)
        .unwrap();
    assert_eq!(count, 1, "early-stop after first entry");
}

// ---------------------------------------------------------------------------
// tc_cp_recursive
// ---------------------------------------------------------------------------

#[test]
fn cp_recursive_copies_tree() {
    let dir = setup_dir("cp_rec");
    let src = format!("{}/src", dir);
    let dst = format!("{}/dst", dir);
    let mut c = client();
    c.ensure_dir_impl(Path::new(&format!("{}/sub", src)), 0o755)
        .unwrap();
    write_file(&mut c, Path::new(&format!("{}/a.txt", src)), b"aaa");
    write_file(&mut c, Path::new(&format!("{}/sub/b.txt", src)), b"bbbb");

    c.copy_tree_impl(Path::new(&src), Path::new(&dst), true, false)
        .expect("cp_recursive");
    assert_eq!(
        read_all(&mut c, Path::new(&format!("{}/a.txt", dst))),
        b"aaa"
    );
    assert_eq!(
        read_all(&mut c, Path::new(&format!("{}/sub/b.txt", dst))),
        b"bbbb"
    );
}

// ---------------------------------------------------------------------------
// Compound batching
// ---------------------------------------------------------------------------

#[test]
fn resolve_deep_path_single_compound() {
    // A 6-component path must resolve (and read/write) correctly; resolve now
    // walks the whole path in one [PUTFH root, LOOKUP x6, GETFH] compound.
    let dir = setup_dir("resolve_deep");
    let deep = format!("{}/a/b/c/d/e", dir);
    let mut c = client();
    c.ensure_dir_impl(Path::new(&deep), 0o755).unwrap();
    write_file(&mut c, Path::new(&format!("{}/file.txt", deep)), b"deep");
    assert_eq!(
        read_all(&mut c, Path::new(&format!("{}/file.txt", deep))),
        b"deep"
    );
}

#[test]
fn batched_readv_writev_many_files() {
    let dir = setup_dir("batch_rw");
    let mut c = client();

    // Open five files; batched WRITEs and REAADs go through the descriptor
    // fast path (one compound per chunk of `[PUTFH, WRITE/READ]` pairs).
    let mut tfs = Vec::new();
    for i in 0..5u8 {
        let f = format!("{}/f{}.txt", dir, i);
        tfs.push(
            c.open_raw_impl(Path::new(&f), libc::O_CREAT | libc::O_RDWR, 0o644)
                .unwrap(),
        );
    }

    let writes: Vec<WriteOp> = tfs
        .iter()
        .map(|tf| {
            WriteOp::from_fd(
                tf.fd().unwrap(),
                VfOffset::At(0),
                vec![b'a' + (tf.fd().unwrap() - 1) as u8; 3],
            )
        })
        .collect();
    let wres = c.vwrite_owned_impl(&writes).expect("batched writev");
    for (i, w) in wres.iter().enumerate() {
        assert_eq!(w.written, 3, "write {} wrote 3 bytes", i);
    }

    let reads: Vec<ReadOp> = tfs
        .iter()
        .map(|tf| ReadOp::from_fd(tf.fd().unwrap(), VfOffset::At(0), 3))
        .collect();
    let rres = c.vread_impl(&reads).expect("batched readv");
    for (i, r) in rres.iter().enumerate() {
        let expect = vec![b'a' + i as u8; 3];
        assert_eq!(r.data, expect, "read {} content", i);
    }

    c.vclose_impl(&tfs).unwrap();
}

#[test]
fn batched_unlinkv() {
    // unlinkv groups same-parent paths into one compound of REMOVEs.
    let dir = setup_dir("batch_unlink");
    let mut c = client();
    let files = [
        format!("{}/u1", dir),
        format!("{}/u2", dir),
        format!("{}/u3", dir),
    ];
    for f in &files {
        write_file(&mut c, Path::new(f), b"x");
    }
    let refs: Vec<&Path> = files.iter().map(Path::new).collect();
    c.vunlink_impl(&refs).expect("batched unlinkv");
    for f in &files {
        assert!(!c.exists_impl(Path::new(f)).unwrap());
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn batch_exceeds_compound_op_limit() {
    // Force a deterministic ceiling below the negotiated server maximum;
    // do not assume the fixture server grants exactly sixteen operations.
    let dir = setup_dir("batch_many");
    let mut c = client();
    let limit = c.test_limit_compound_operations(8);
    let n = 10u8;
    let paths: Vec<_> = (0..n).map(|index| format!("{dir}/f{index}.txt")).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let files = c
        .vopen_raw_simple_impl(
            &paths.iter().map(Path::new).collect::<Vec<_>>(),
            libc::O_CREAT | libc::O_RDWR,
            0o600,
        )
        .unwrap();
    let (count, _, _, max_ops) = vfsi_nfs::compound::thread_compound_stats();
    assert!(count > 1, "OPEN must split across compounds");
    assert!(max_ops <= limit as u64);

    // Batched writev across all 10 open files.
    let writes: Vec<WriteOp> = files
        .iter()
        .enumerate()
        .map(|(index, tf)| {
            WriteOp::from_fd(
                tf.fd().unwrap(),
                VfOffset::At(0),
                vec![b'a' + index as u8; 2],
            )
        })
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    c.vwrite_owned_impl(&writes)
        .expect("batched writev (10 files)");
    let (count, _, _, max_ops) = vfsi_nfs::compound::thread_compound_stats();
    assert!(count > 1, "WRITE must split across compounds");
    assert!(max_ops <= limit as u64);

    // Batched getattrsv / setattrsv on all 10 paths.
    let mut attrs: Vec<VfAttrs> = paths
        .iter()
        .map(|p| VfAttrs {
            file: VfFile::from_path(p),
            masks: AttrMask::MODE | AttrMask::SIZE,
            ..VfAttrs::default()
        })
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    c.vgetattrs_impl(&mut attrs).expect("getattrsv (10 files)");
    for a in &attrs {
        assert_eq!(a.size, 2);
        assert_eq!(a.mode & 0o777, 0o600);
    }
    c.vsetattrs_raw_impl(&attrs).expect("setattrsv (10 files)");

    // Batched readv back.
    let reads: Vec<ReadOp> = files
        .iter()
        .map(|tf| ReadOp::from_fd(tf.fd().unwrap(), VfOffset::At(0), 2))
        .collect();
    let rres = c.vread_impl(&reads).expect("batched readv (10 files)");
    for (i, r) in rres.iter().enumerate() {
        assert_eq!(r.data, vec![b'a' + i as u8; 2], "read {} content", i);
    }

    // Batched closev.
    c.vclose_impl(&files).expect("closev (10 files)");
    let (_, _, _, max_ops) = vfsi_nfs::compound::thread_compound_stats();
    assert!(
        max_ops <= limit as u64,
        "metadata/READ/CLOSE exceeded ceiling"
    );
    c.vunlink_impl(&paths.iter().map(Path::new).collect::<Vec<_>>())
        .expect("unlinkv (10 files)");
}

#[test]
fn recovery_notifications_are_delivered_after_unlock_and_allow_reentry() {
    use std::sync::{Arc, Mutex, mpsc};
    type Callback = Arc<dyn Fn() + Send + Sync>;
    struct Observer {
        events: Mutex<Vec<&'static str>>,
        callback: Mutex<Option<Callback>>,
    }
    impl NfsObserver for Observer {
        fn on_event(&self, event: &NfsEvent) {
            let name = match event {
                NfsEvent::Connected { .. } => "connected",
                NfsEvent::ReconnectStarted => "started",
                NfsEvent::ReconnectSucceeded => "succeeded",
                _ => "other",
            };
            self.events.lock().unwrap().push(name);
            if matches!(event, NfsEvent::ReconnectStarted) {
                let callback = self.callback.lock().unwrap().clone();
                if let Some(callback) = callback {
                    callback();
                }
            }
        }
    }
    let observer = Arc::new(Observer {
        events: Mutex::new(Vec::new()),
        callback: Mutex::new(None),
    });
    let mut backend = NfsVecFs::builder(test_host())
        .minor_version(
            std::env::var("VNFS_TEST_MINOR")
                .ok()
                .map(|v| v.parse().unwrap()),
        )
        .observer(observer.clone())
        .connect()
        .unwrap();
    backend.reconnect().unwrap();
    assert_eq!(
        *observer.events.lock().unwrap(),
        ["connected"],
        "backend must defer recovery notifications"
    );
    let client = vfsi_sync::FsClient::new(backend);
    let reentrant = client.clone();
    let (tx, rx) = mpsc::channel();
    *observer.callback.lock().unwrap() = Some(Arc::new(move || {
        reentrant.capabilities().unwrap();
        tx.send(()).unwrap();
    }));
    let worker = std::thread::spawn(move || {
        client.capabilities().unwrap();
        client
    });
    rx.recv_timeout(std::time::Duration::from_secs(3))
        .expect("observer must be able to reenter");
    let client = worker.join().unwrap();
    *observer.callback.lock().unwrap() = None;
    assert_eq!(
        *observer.events.lock().unwrap(),
        ["connected", "started", "succeeded"]
    );
    client.into_inner().unwrap().shutdown().unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn deferred_cleanup_reconciles_backend_owned_failed_close() {
    let dir = setup_dir("deferred_cleanup");
    let path = format!("{dir}/file");
    let mut backend = client();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseDispatch { index: 0 },
        VfError::transport(None, "injected deferred close failure"),
    ));
    backend.set_fault_injector(script.clone());
    let owned = vfsi_sync::FsClient::new(backend);
    drop(owned.create(&path).unwrap());
    assert!(owned.drain_cleanup().unwrap_err().is_transport());
    assert!(script.is_consumed());
    owned.drain_cleanup().unwrap();
    let mut backend = owned.into_inner().unwrap();
    assert_eq!(backend.test_open_handle_count(), 0);
    assert_eq!(backend.test_deferred_descriptor_close_count(), 0);
    backend.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
}

#[test]
fn directory_page_collection_preserves_batching_empty_roots_and_ordered_cancellation() {
    use vnfs::{ControlFlow, Vfsi, VisitOptions};
    let dir = setup_dir("directory_pages");
    let fs = Nfs::builder(test_host())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::V4_1,
        })
        .connect()
        .unwrap();
    let roots: Vec<_> = (0..40).map(|i| format!("{dir}/d{i}")).collect();
    fs.vmkdir(&roots.iter().map(|path| (path, 0o777)).collect::<Vec<_>>())
        .unwrap();
    let files: Vec<_> = roots
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 2 == 0)
        .map(|(_, path)| (format!("{path}/file"), b"x"))
        .collect();
    fs.write_files(&files).unwrap();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let listings = fs
        .read_dirs_with_options(&roots, VisitOptions::new())
        .unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        compounds < 20,
        "forty directories must use bounded cohorts, got {compounds} compounds"
    );
    assert_eq!(listings.len(), roots.len());
    for (index, tree) in listings.iter().enumerate() {
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].path, Path::new(&roots[index]));
        assert_eq!(tree[0].entries.len(), usize::from(index % 2 == 0));
    }
    let missing = format!("{dir}/missing");
    assert_eq!(
        fs.vlistdirs(
            &[roots[1].as_str(), missing.as_str()],
            VisitOptions::new(),
            |index, page| {
                assert_eq!(index, 0);
                assert!(page.entries.is_empty());
                assert!(fs.metadata(&page.path).unwrap().is_dir());
                Ok(ControlFlow::Break(()))
            }
        )
        .unwrap(),
        [vnfs::TraversalCompletion::Stopped]
    );
    assert_eq!(
        fs.read_dirs_with_options(&[roots[1].as_str(), missing.as_str()], VisitOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    let large = &roots[0];
    let names: Vec<_> = (0..300).map(|i| (format!("{large}/f{i}"), b"x")).collect();
    fs.write_files(&names).unwrap();
    let mut counts = Vec::new();
    fs.vlistdirs(&[large], VisitOptions::new(), |_, page| {
        counts.push(page.entries.len());
        Ok(ControlFlow::Continue(()))
    })
    .unwrap();
    assert_eq!(counts[0], 1);
    assert!(counts.iter().all(|&n| n <= 128));
    assert_eq!(counts.iter().sum::<usize>(), 301);
    fs.remove_dir_all(&dir).unwrap();
}

#[test]
fn recursive_directory_pages_reject_a_child_replaced_by_a_symlink() {
    use vnfs::{ControlFlow, Vfsi, VisitOptions};
    let dir = setup_dir("page_child_symlink");
    let fs = Nfs::builder(test_host())
        .version(if std::env::var("VNFS_TEST_MINOR").as_deref() == Ok("2") {
            vnfs::NfsVersion::V4_2
        } else {
            vnfs::NfsVersion::V4_1
        })
        .connect()
        .unwrap();
    let tree = format!("{dir}/tree");
    let child = format!("{tree}/child");
    let saved = format!("{dir}/saved");
    let target = format!("{dir}/outside");
    fs.vmkdir(&[(&tree, 0o777), (&target, 0o777)]).unwrap();
    fs.vmkdir(&[(&child, 0o777)]).unwrap();
    fs.write(format!("{target}/secret"), b"must not be traversed")
        .unwrap();
    let mut changed = false;
    let mut escaped = false;
    let result = fs.vlistdirs(&[&tree], VisitOptions::new().recursive(true), |_, page| {
        if page.path == Path::new(&tree) && !changed {
            assert!(
                page.entries
                    .iter()
                    .any(|entry| entry.path() == Path::new(&child))
            );
            fs.vrename(&[(&child, &saved)])?;
            fs.symlink("../outside", &child)?;
            changed = true;
        }
        escaped |= page
            .entries
            .iter()
            .any(|entry| entry.path().ends_with("secret"));
        Ok(ControlFlow::Continue(()))
    });
    fs.remove_dir_all(&dir).unwrap();
    assert!(changed);
    assert!(!escaped, "recursive paging followed a replacement symlink");
    let error = result.unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert!(
        matches!(
            error.err_no(),
            nfsv41_sys::nfsstat4_NFS4ERR_SYMLINK | nfsv41_sys::nfsstat4_NFS4ERR_NOTDIR
        ),
        "{error}"
    );
}

#[test]
fn recursive_directory_pages_keep_linear_deep_tree_compound_counts() {
    use vnfs::VisitOptions;
    let dir = setup_dir("page_deep_tree");
    let fs = Nfs::builder(test_host())
        .version(if std::env::var("VNFS_TEST_MINOR").as_deref() == Ok("2") {
            vnfs::NfsVersion::V4_2
        } else {
            vnfs::NfsVersion::V4_1
        })
        .connect()
        .unwrap();
    let depth = 32;
    let mut path = dir.clone();
    let mut paths = Vec::new();
    for _ in 0..depth {
        path.push_str("/d");
        paths.push(path.clone());
    }
    // Parents exist before children are created.
    for path in &paths {
        fs.vmkdir(&[(path, 0o777)]).unwrap();
    }
    fs.write(format!("{path}/leaf"), b"x").unwrap();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let trees = fs
        .read_dirs_with_options(&[&dir], VisitOptions::new().recursive(true))
        .unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    fs.remove_dir_all(&dir).unwrap();
    eprintln!("depth {depth}: {compounds} compounds");
    assert_eq!(trees[0].len(), depth + 1);
    assert!(
        compounds <= (depth + 8) as u64,
        "descendants must use anchored LOOKUP + READDIR, got {compounds} compounds for depth {depth}"
    );
}

#[test]
fn recursive_directory_pages_retain_the_parent_handle_after_rename() {
    use vnfs::{ControlFlow, Vfsi, VisitOptions};
    let dir = setup_dir("page_parent_rename");
    let fs = Nfs::builder(test_host())
        .version(if std::env::var("VNFS_TEST_MINOR").as_deref() == Ok("2") {
            vnfs::NfsVersion::V4_2
        } else {
            vnfs::NfsVersion::V4_1
        })
        .connect()
        .unwrap();
    let tree = format!("{dir}/tree");
    let moved = format!("{dir}/moved");
    let children: Vec<_> = (0..4).map(|i| format!("{tree}/child{i}")).collect();
    fs.vmkdir(&[(&tree, 0o777)]).unwrap();
    fs.vmkdir(
        &children
            .iter()
            .map(|path| (path, 0o777))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    fs.write_files(
        &children
            .iter()
            .map(|child| (format!("{child}/data"), b"x"))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let mut renamed = false;
    let mut data = 0;
    let result = fs.vlistdirs(&[&tree], VisitOptions::new().recursive(true), |_, page| {
        if page.path == Path::new(&tree) && !renamed {
            fs.vrename(&[(&tree, &moved)])?;
            renamed = true;
        }
        data += page
            .entries
            .iter()
            .filter(|entry| entry.path().ends_with("data"))
            .count();
        Ok(ControlFlow::Continue(()))
    });
    fs.remove_dir_all(&dir).unwrap();
    assert_eq!(result.unwrap(), [vnfs::TraversalCompletion::Complete]);
    assert!(renamed);
    assert_eq!(
        data, 4,
        "batched children must use the retained parent handle"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn recursive_directory_pages_do_not_retry_an_ambiguous_child_reply() {
    use vnfs::{ControlFlow, Vfsi, VisitOptions};
    let dir = setup_dir("page_child_reply_loss");
    let mut admin = client();
    let child = format!("{dir}/child");
    admin.mkdir_raw_impl(Path::new(&child), 0o755).unwrap();
    write_file(&mut admin, Path::new(&format!("{child}/data")), b"x");
    let proxy = DropReplyProxy::start(reply_loss_target());
    let visitor = Nfs::builder(proxy.endpoint())
        .version(if std::env::var("VNFS_TEST_MINOR").as_deref() == Ok("2") {
            vnfs::NfsVersion::V4_2
        } else {
            vnfs::NfsVersion::V4_1
        })
        .request_timeout(Duration::from_millis(500))
        .connect()
        .unwrap();
    let mut delivered = 0;
    let result = visitor.vlistdirs(&[&dir], VisitOptions::new().recursive(true), |_, page| {
        assert_eq!(
            page.path,
            Path::new(&dir),
            "a child page must not be replayed after the lost reply"
        );
        delivered += 1;
        proxy.arm();
        Ok(ControlFlow::Continue(()))
    });
    proxy.wait_for_drop();
    admin.remove_paths_impl(&[Path::new(&dir)], true).unwrap();
    assert!(result.unwrap_err().is_transport());
    assert_eq!(delivered, 1);
}

#[path = "support/vsetattrs.rs"]
mod vsetattrs_support;

#[test]
fn vsetattrs_many_nfs_files() {
    let directory = setup_dir("vsetattrs-many");
    let fs = Nfs::builder(test_host())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .connect()
        .unwrap();
    vsetattrs_support::check_many(&fs, &directory);
    vsetattrs_support::check_handles(&fs, &directory);
    vsetattrs_support::check_ownership(&fs, &directory);
    let other = Nfs::builder(test_host()).connect().unwrap();
    vsetattrs_support::check_foreign(&fs, &other, &format!("{directory}/foreign"));
    // A handle vector must not degrade into one RPC per scalar update.
    use vnfs::{MetadataTarget, MetadataUpdate, OpenFlags, OpenRequest};
    let paths: Vec<_> = (0..64).map(|i| format!("{directory}/file-{i}")).collect();
    let requests: Vec<_> = paths
        .iter()
        .map(|path| OpenRequest::new(path, OpenFlags::WRITE))
        .collect();
    let mut files = fs.vopen(&requests).unwrap();
    let updates: Vec<_> = files
        .iter()
        .map(|file| {
            (
                MetadataTarget::File(file),
                MetadataUpdate::new()
                    .len(4)
                    .uid(unsafe { libc::geteuid() })
                    .gid(unsafe { libc::getegid() }),
            )
        })
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    fs.vsetattrs(&updates, true).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        compounds < 16,
        "64 handle updates should be batched; got {compounds} compounds"
    );
    fs.vclose(&mut files).unwrap();
    fs.remove_dir_all(&directory).unwrap();
}

#[path = "support/links.rs"]
mod links_support;

#[test]
fn vector_links_modes_and_capabilities_work_generically_on_nfs() {
    let directory = setup_dir("vector-links-modes");
    let fs = Nfs::builder(test_host())
        .version(match std::env::var("VNFS_TEST_MINOR").as_deref() {
            Ok("1") => vnfs::NfsVersion::V4_1,
            Ok("2") => vnfs::NfsVersion::V4_2,
            _ => vnfs::NfsVersion::Auto,
        })
        .connect()
        .unwrap();
    links_support::check_links_and_modes(&fs, &directory);
    fs.remove_dir_all(&directory).unwrap();
}

#[path = "support/statfs.rs"]
mod statfs_support;

#[test]
fn filesystem_stats_nfs_vectors() {
    let directory = setup_dir("filesystem-stats");
    let version = match std::env::var("VNFS_TEST_MINOR").as_deref() {
        Ok("1") => vnfs::NfsVersion::V4_1,
        Ok("2") => vnfs::NfsVersion::V4_2,
        _ => vnfs::NfsVersion::Auto,
    };
    let fs = Nfs::builder(test_host())
        .version(version)
        .connect()
        .unwrap();
    let other = Nfs::builder(test_host())
        .version(version)
        .connect()
        .unwrap();
    statfs_support::check(&fs, &other, &directory);
    let paths: Vec<_> = (1..64).map(|i| format!("{directory}/stats-{i}")).collect();
    let mut files = fs
        .vopen(
            &paths
                .iter()
                .map(|p| vnfs::OpenRequest::new(p, vnfs::OpenFlags::READ))
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let targets: Vec<_> = files.iter().map(vnfs::MetadataTarget::File).collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let stats = fs.vstatfs(&targets).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(
        compounds < 16,
        "statistics must batch 63 handles, got {compounds}"
    );
    assert!(stats[0].max_file_size.unwrap() > 0);
    assert!(stats[0].block_size.is_none());
    assert!(stats[0].read_only.is_none());
    fs.vclose(&mut files).unwrap();
    fs.remove_dir_all(directory).unwrap();
}

#[test]
fn copy_options_nfs_mixed_source_symlinks_and_extents() {
    let directory = setup_dir("copy-options-mixed");
    let mut fs = client();
    let source = format!("{directory}/source");
    let link = format!("{directory}/link");
    let dangling = format!("{directory}/dangling");
    write_file(&mut fs, Path::new(&source), b"0123456789");
    fs.symlink_raw_impl(Path::new("source"), Path::new(&link))
        .unwrap();
    fs.symlink_raw_impl(Path::new("missing-target"), Path::new(&dangling))
        .unwrap();
    let options = CopyOption::new().follow_source_symlinks(false);
    let mut pairs: Vec<_> = (0..17)
        .map(|i| ExtentPair::new(&source, 2, &format!("{directory}/copy-{i}"), 0, Some(4)))
        .collect();
    pairs.push(ExtentPair::new(
        &link,
        0,
        &format!("{directory}/link-copy"),
        0,
        None,
    ));
    pairs.push(ExtentPair::new(
        &dangling,
        99,
        &format!("{directory}/dangling-copy"),
        100,
        Some(0),
    ));
    pairs.push(ExtentPair::new(
        &source,
        5,
        &format!("{directory}/last-copy"),
        0,
        Some(3),
    ));
    fs.vcopy_impl(&pairs, options).unwrap();
    for pair in &pairs[..17] {
        assert_eq!(read_all(&mut fs, &pair.dst_path), b"2345");
    }
    assert_eq!(
        fs.readlink_raw_impl(&pairs[17].dst_path).unwrap(),
        b"source"
    );
    assert_eq!(
        fs.readlink_raw_impl(&pairs[18].dst_path).unwrap(),
        b"missing-target"
    );
    assert_eq!(read_all(&mut fs, &pairs[19].dst_path), b"567");
    let followed = format!("{directory}/followed");
    fs.vcopy_impl(
        &[ExtentPair::new(&link, 0, &followed, 0, None)],
        CopyOption::new(),
    )
    .unwrap();
    assert_eq!(read_all(&mut fs, Path::new(&followed)), b"0123456789");
    // A failure in a symlink run must retain its original input index.
    let error = fs
        .vcopy_impl(
            &[
                ExtentPair::new(&source, 0, &format!("{directory}/prefix"), 0, None),
                pairs[18].clone(),
            ],
            options,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EEXIST as u32);
    // A failure in a data run after a link must also retain its input index.
    let error = fs
        .vcopy_impl(
            &[
                ExtentPair::new(&link, 0, &format!("{directory}/second-link"), 0, None),
                ExtentPair::new(
                    &source,
                    0,
                    &format!("{directory}/missing-parent/output"),
                    0,
                    None,
                ),
            ],
            options,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    fs.vcopy_impl(&[], options).unwrap();
    fs.remove_paths_impl(&[Path::new(&directory)], true)
        .unwrap();
}
