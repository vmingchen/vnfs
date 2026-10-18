//! Server COPY preserves dependencies between requests without disabling batching.
#![cfg(feature = "server-copy")]

use std::path::Path;
use vfsi_nfs::NfsVecFs;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::{CopyOptions, ExtentPair, FileOperand};

fn connect(name: &str) -> Option<(NfsVecFs, String)> {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return None;
    };
    let mut fs = NfsVecFs::connect_minor(&server, 2).expect("connect NFSv4.2 fixture");
    let root = format!("/prr-copy-dependency-{}-{name}", std::process::id());
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    Some((fs, root))
}
fn put(fs: &mut NfsVecFs, path: &str, data: &[u8]) {
    let file = fs
        .open_raw_impl(
            Path::new(path),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
        .unwrap();
    assert_eq!(fs.write_raw_impl(&file, 0, data).unwrap(), data.len());
    fs.close_impl(&file).unwrap();
}
fn get(fs: &mut NfsVecFs, path: &str) -> Vec<u8> {
    fs.read_raw_impl(&FileOperand::from_path(path), 0, 100)
        .unwrap()
}
fn cleanup(fs: &mut NfsVecFs, root: &str) {
    fs.remove_paths_impl(&[Path::new(root)], true).unwrap();
}
fn chained(hardlink: bool) {
    let Some((mut fs, root)) = connect(if hardlink { "hardlink" } else { "chain" }) else {
        return;
    };
    let (a, b, c, alias) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
        format!("{root}/alias"),
    );
    put(&mut fs, &a, b"fresh");
    put(&mut fs, &b, b"stale-tail");
    put(&mut fs, &c, b"old");
    if hardlink {
        fs.vhardlink_impl(&[Path::new(&b)], &[Path::new(&alias)])
            .unwrap();
    }
    let source = if hardlink { &alias } else { &b };
    let result = fs.vcopy_impl(
        &[
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(source, 0, &c, 0, None),
        ],
        CopyOptions::default(),
    );
    let actual_b = get(&mut fs, &b);
    let actual_c = get(&mut fs, &c);
    let enabled = fs.server_copy_enabled();
    cleanup(&mut fs, &root);
    result.expect("chained COPY requests must execute in caller order");
    assert_eq!(actual_b, b"fresh");
    assert_eq!(actual_c, b"fresh");
    assert!(
        enabled,
        "a dependency must not disable server COPY for future calls"
    );
}
#[test]
fn chained_copy_reads_the_previous_destination() {
    chained(false)
}
#[test]
fn chained_copy_detects_hardlink_alias_dependencies() {
    chained(true)
}
#[test]
fn later_destination_does_not_invalidate_an_earlier_source() {
    let Some((mut fs, root)) = connect("anti") else {
        return;
    };
    let (a, b, c) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
    );
    put(&mut fs, &a, b"first");
    put(&mut fs, &b, b"stale");
    put(&mut fs, &c, b"second");
    let result = fs.vcopy_impl(
        &[
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(&c, 0, &a, 0, None),
        ],
        CopyOptions::default(),
    );
    let actual_b = get(&mut fs, &b);
    let actual_a = get(&mut fs, &a);
    let enabled = fs.server_copy_enabled();
    cleanup(&mut fs, &root);
    result.unwrap();
    assert_eq!(actual_b, b"first");
    assert_eq!(actual_a, b"second");
    assert!(enabled);
}
#[test]
fn repeated_destination_preserves_final_size_without_disabling_server_copy() {
    let Some((mut fs, root)) = connect("repeat") else {
        return;
    };
    let (a, b, c) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
    );
    put(&mut fs, &a, b"long-first");
    put(&mut fs, &b, b"x");
    put(&mut fs, &c, b"stale-tail");
    let result = fs.vcopy_impl(
        &[
            ExtentPair::new(&a, 0, &c, 0, None),
            ExtentPair::new(&b, 0, &c, 0, None),
        ],
        CopyOptions::default(),
    );
    let actual = get(&mut fs, &c);
    let enabled = fs.server_copy_enabled();
    cleanup(&mut fs, &root);
    result.unwrap();
    assert_eq!(actual, b"x");
    assert!(enabled);
}
#[test]
fn independent_pairs_keep_one_server_copy_cohort() {
    let Some((mut fs, root)) = connect("independent") else {
        return;
    };
    let (a, b, c, d) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
        format!("{root}/d"),
    );
    put(&mut fs, &a, b"first");
    put(&mut fs, &b, b"second");
    let before = fs.server_copy_stats();
    let result = fs.vcopy_impl(
        &[
            ExtentPair::new(&a, 0, &c, 0, None),
            ExtentPair::new(&b, 0, &d, 0, None),
        ],
        CopyOptions::default(),
    );
    let after = fs.server_copy_stats();
    let actual_c = get(&mut fs, &c);
    let actual_d = get(&mut fs, &d);
    cleanup(&mut fs, &root);
    result.unwrap();
    assert_eq!(actual_c, b"first");
    assert_eq!(actual_d, b"second");
    assert_eq!(after.fallbacks, before.fallbacks);
    assert_eq!(after.requests - before.requests, 2);
    assert_eq!(after.operations - before.operations, 4);
}

fn fanout(hardlink: bool) {
    let Some((mut fs, root)) = connect(if hardlink { "fanout-alias" } else { "fanout" }) else {
        return;
    };
    let (a, b, c, alias) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
        format!("{root}/alias"),
    );
    put(&mut fs, &a, b"fresh");
    if hardlink {
        fs.vhardlink_impl(&[Path::new(&a)], &[Path::new(&alias)])
            .unwrap();
    }
    let source = if hardlink { &alias } else { &a };
    let result = fs.vcopy_impl(
        &[
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(source, 0, &c, 0, None),
        ],
        CopyOptions::default(),
    );
    let actual_b = get(&mut fs, &b);
    let actual_c = get(&mut fs, &c);
    let enabled = fs.server_copy_enabled();
    cleanup(&mut fs, &root);
    result.unwrap();
    assert_eq!(actual_b, b"fresh");
    assert_eq!(actual_c, b"fresh");
    assert!(enabled);
}
#[test]
fn repeated_source_fanout_preserves_server_copy_capability() {
    fanout(false)
}
#[test]
fn repeated_source_hardlink_fanout_preserves_server_copy_capability() {
    fanout(true)
}

#[test]
fn failed_dependent_suffix_keeps_original_index_and_does_not_replay_prefix() {
    let Some((mut fs, root)) = connect("error") else {
        return;
    };
    let (a, b, c) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
    );
    put(&mut fs, &a, b"fresh");
    put(&mut fs, &b, b"stale");
    put(&mut fs, &c, b"old");
    let result = fs.vcopy_impl(
        &[
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(&b, 0, &c, 0, None),
            ExtentPair::new(&c, 0, &c, 0, None),
        ],
        CopyOptions::default(),
    );
    let actual_b = get(&mut fs, &b);
    let actual_c = get(&mut fs, &c);
    let enabled = fs.server_copy_enabled();
    #[cfg(feature = "test-faults")]
    assert_eq!(fs.test_open_handle_count(), 0);
    cleanup(&mut fs, &root);
    let error = result.expect_err("fixture rejects same-file server COPY");
    assert_eq!(error.index(), Some(2));
    assert_eq!(actual_b, b"fresh");
    assert_eq!(actual_c, b"fresh");
    assert!(enabled);
}
