#![cfg(all(feature = "auto", target_os = "linux"))]

use vnfs::FsExt;
use vnfs::{ErrorKind, Mounted, helpers::TreeBuilder};

#[test]
fn duplicate_directories_do_not_consume_planned_entry_slots() {
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    TreeBuilder::new()
        .max_entries(1)
        .add_directory("a")
        .add_directory("./a")
        .add_directory("a/")
        .create(&client, "/one")
        .unwrap();
    assert!(client.metadata_one("/one/a").unwrap().is_dir());
    // Deduplication must not change the stored file position used for writes.
    TreeBuilder::new()
        .max_entries(2)
        .add_directory("a")
        .add_directory("./a")
        .add_file("a/file", "payload")
        .create(&client, "/with-file")
        .unwrap();
    assert_eq!(
        client
            .readv_with_options(
                [vnfs::ReadOp::whole("/with-file/a/file")],
                vnfs::ReadOptions::default()
            )
            .unwrap()
            .remove(0)
            .data
            .as_deref()
            .unwrap(),
        b"payload"
    );
}

#[test]
fn duplicate_directories_do_not_consume_path_storage_budget() {
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    let mut builder = TreeBuilder::new().max_total_bytes("/fixture/a".len());
    for _ in 0..32 {
        builder = builder.add_directory("./a");
    }
    builder.create(&client, "/fixture").unwrap();
    assert!(client.metadata_one("/fixture/a").unwrap().is_dir());
}

#[test]
fn original_error_indices_include_suppressed_directory_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    let builders = [
        (
            TreeBuilder::new()
                .add_directory("a")
                .add_directory("./a")
                .add_file("../invalid", ""),
            2,
        ),
        (
            TreeBuilder::new()
                .add_directory("a")
                .add_directory("./a")
                .add_file("a/file", "first")
                .add_file("a/file", "second"),
            3,
        ),
        (
            TreeBuilder::new()
                .max_entries(1)
                .add_directory("a")
                .add_directory("./a")
                .add_directory("b"),
            2,
        ),
    ];
    for (builder, expected_index) in builders {
        let error = builder.create(&client, "/fixture").unwrap_err();
        assert_eq!(error.index(), Some(expected_index));
        assert!(!temp.path().join("fixture").exists());
    }
}

#[test]
fn builds_nested_binary_empty_and_directory_entries_without_implicit_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    let tree = TreeBuilder::new()
        .batch_size(2)
        .add_file("config/./app.conf", "host = localhost")
        .add_file("data/blob", [0, 255, 10])
        .add_empty_file("logs/app.log")
        .add_directory("data/raw")
        .add_directory("data")
        .create(&client, "/fixture")
        .unwrap();
    assert_eq!(tree.root(), std::path::Path::new("/fixture"));
    assert_eq!(
        client
            .readv_with_options(
                [vnfs::ReadOp::whole("/fixture/config/app.conf")],
                vnfs::ReadOptions::default()
            )
            .unwrap()
            .remove(0)
            .data
            .as_deref()
            .unwrap(),
        b"host = localhost"
    );
    assert_eq!(
        client
            .readv_with_options(
                [vnfs::ReadOp::whole("/fixture/data/blob")],
                vnfs::ReadOptions::default()
            )
            .unwrap()
            .remove(0)
            .data
            .as_deref()
            .unwrap(),
        [0, 255, 10]
    );
    assert!(
        client
            .readv_with_options(
                [vnfs::ReadOp::whole("/fixture/logs/app.log")],
                vnfs::ReadOptions::default()
            )
            .unwrap()
            .remove(0)
            .data
            .unwrap()
            .is_empty()
    );
    assert!(client.metadata_one("/fixture/data/raw").unwrap().is_dir());
    drop(tree);
    assert!(temp.path().join("fixture/config/app.conf").exists());
    client.remove_dir_all_one("/fixture").unwrap();
    assert!(!temp.path().join("fixture").exists());
}

#[test]
fn rejects_invalid_paths_and_collisions_before_creating_root() {
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    for path in ["../outside", "a/../../outside", "/outside", "", ".", "a\0b"] {
        let error = TreeBuilder::new()
            .add_file("valid", "ok")
            .add_file(path, "bad")
            .create(&client, "/fixture")
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{path:?}");
        assert_eq!(error.index(), Some(1));
        assert!(!temp.path().join("fixture").exists());
    }
    for builder in [
        TreeBuilder::new()
            .add_file("a", "one")
            .add_file("./a", "two"),
        TreeBuilder::new().add_file("a", "one").add_directory("a"),
        TreeBuilder::new().add_directory("a").add_file("a", "one"),
        TreeBuilder::new()
            .add_file("a", "one")
            .add_file("a/b", "two"),
        TreeBuilder::new()
            .add_file("a/b", "one")
            .add_file("a", "two"),
    ] {
        let error = builder.create(&client, "/fixture").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.index(), Some(1));
        assert!(!temp.path().join("fixture").exists());
    }
}

#[test]
fn budgets_include_implicit_directories_payloads_and_planned_path_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    let builders = [
        TreeBuilder::new().max_entries(2).add_file("a/b/c", ""),
        TreeBuilder::new().max_total_bytes(5).add_file("f", "12345"),
        TreeBuilder::new().max_total_bytes(10).add_file("a/b", ""),
        TreeBuilder::new().batch_size(0).add_file("f", ""),
        TreeBuilder::new()
            .add_directory(std::iter::repeat_n("x", 129).collect::<Vec<_>>().join("/")),
    ];
    for builder in builders {
        assert!(builder.create(&client, "/fixture").is_err());
        assert!(!temp.path().join("fixture").exists());
    }
    // Exact final path+payload budget succeeds.
    TreeBuilder::new()
        .max_total_bytes("/fixture/f".len() + 3)
        .add_file("f", "123")
        .create(&client, "/fixture")
        .unwrap();
    assert_eq!(
        client
            .readv_with_options(
                [vnfs::ReadOp::whole("/fixture/f")],
                vnfs::ReadOptions::default()
            )
            .unwrap()
            .remove(0)
            .data
            .as_deref()
            .unwrap(),
        b"123"
    );
}

#[test]
fn existing_root_is_not_overwritten_or_cleaned_up_even_if_it_is_a_symlink() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    client.create_dir_one("/existing").unwrap();
    client.write_one("/existing/keep", b"original").unwrap();
    symlink("existing", temp.path().join("link")).unwrap();
    for root in ["/existing", "/link"] {
        assert!(
            TreeBuilder::new()
                .add_file("keep", "replacement")
                .create(&client, root)
                .is_err()
        );
        assert_eq!(
            client
                .readv_with_options(
                    [vnfs::ReadOp::whole("/existing/keep")],
                    vnfs::ReadOptions::default()
                )
                .unwrap()
                .remove(0)
                .data
                .as_deref()
                .unwrap(),
            b"original"
        );
    }
}

#[test]
fn empty_tree_and_missing_root_parent_have_explicit_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    TreeBuilder::new().create(&client, "/empty").unwrap();
    assert!(client.read_dir_one("/empty").unwrap().is_empty());
    let error = TreeBuilder::new()
        .add_file("f", "payload")
        .create(&client, "/missing/root")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(!temp.path().join("missing").exists());
}

#[test]
fn bulk_directory_creation_preserves_error_index_path_and_completed_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let client = Mounted::new(temp.path()).unwrap();
    client.create_dir_one("/taken").unwrap();
    let error = client
        .create_dirs(&["/first", "/taken", "/last"])
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.path(), Some(std::path::Path::new("/taken")));
    assert!(temp.path().join("first").is_dir());
    assert!(!temp.path().join("last").exists());
    assert!(client.create_dirs(&["/duplicate", "/duplicate"]).is_err());
    assert!(!temp.path().join("duplicate").exists());
    client.create_dirs::<&str>(&[]).unwrap();
}

#[test]
fn routed_client_supports_the_same_builder_and_directory_preflight() {
    let temp = tempfile::tempdir().unwrap();
    let client = vnfs::Auto::new(temp.path()).unwrap();
    let tree = TreeBuilder::new()
        .add_file("a/file", "a")
        .add_file("b/file", "b")
        .create(&client, "/fixture")
        .unwrap();
    assert_eq!(
        client
            .readv_with_options(
                [
                    vnfs::ReadOp::whole("/fixture/a/file"),
                    vnfs::ReadOp::whole("/fixture/b/file")
                ],
                vnfs::ReadOptions::default()
            )
            .unwrap()
            .into_iter()
            .map(|r| r.data.unwrap())
            .collect::<Vec<_>>(),
        [b"a".to_vec(), b"b".to_vec()]
    );
    assert!(client.create_dirs(&["/duplicate", "/duplicate"]).is_err());
    assert!(!temp.path().join("duplicate").exists());
    client.create_dirs::<&str>(&[]).unwrap();
    client.remove_dir_all_one(tree.root()).unwrap();
}
