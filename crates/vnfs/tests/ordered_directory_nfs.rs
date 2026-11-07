#![cfg(feature = "nfs")]
use std::path::Path;
use vnfs::directory::{Attributes, ListDirOptions, WalkControl, WalkEventKind, WalkOptions};
use vnfs::files::VfsiExt;
use vnfs::nfs::{Nfs, NfsVersion, Telemetry};

fn clients() -> Vec<(NfsVersion, vnfs::nfs::NfsClient, Telemetry)> {
    let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return Vec::new();
    };
    let versions = match std::env::var("VFSI_NFS_MINOR").as_deref() {
        Ok("1") => vec![NfsVersion::V4_1],
        Ok("2") => vec![NfsVersion::V4_2],
        Err(std::env::VarError::NotPresent) => vec![NfsVersion::V4_1, NfsVersion::V4_2],
        value => panic!("invalid VFSI_NFS_MINOR: {value:?}"),
    };
    versions
        .into_iter()
        .map(|version| {
            let metrics = Telemetry::default();
            let fs = Nfs::builder(&host)
                .root(std::env::var("VFSI_NFS_EXPORT").unwrap_or_else(|_| "/".into()))
                .version(version)
                .telemetry(metrics.clone())
                .connect()
                .unwrap();
            (version, fs, metrics)
        })
        .collect()
}

fn fixture() -> String {
    format!(
        "/ordered-pages-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[test]
fn ordered_visitors_retain_the_observed_parent_after_rename_and_replacement() {
    for (_, fs, _) in clients() {
        for lifecycle in [false, true] {
            let root = fixture();
            let parent = format!("{root}/parent");
            let moved = format!("{root}-saved");
            fs.create_dir_all(format!("{parent}/child")).unwrap();
            fs.write(format!("{parent}/child/leaf"), b"original")
                .unwrap();
            let mut changed = false;
            let mut observed = Vec::new();
            let mut swap = || -> vnfs::Result<()> {
                if !changed {
                    fs.rename(&root, &moved)?;
                    changed = true;
                    fs.create_dir_all(format!("{parent}/child"))?;
                    fs.write(format!("{parent}/child/secret"), b"replacement")?;
                }
                Ok(())
            };
            let result = if lifecycle {
                fs.listdir(
                    &root,
                    WalkOptions::new(ListDirOptions::new().with_recursive(true))
                        .with_enter_leave(true)
                        .with_sort_by_name(true),
                    |event| {
                        if event.kind == WalkEventKind::Enter
                            && event.entry.path() == Path::new(&parent)
                        {
                            swap()?;
                        }
                        if event.kind == WalkEventKind::Entry {
                            observed.push(event.entry.path().to_path_buf());
                        }
                        Ok(WalkControl::Continue)
                    },
                )
            } else {
                fs.visit_dirs_ordered(
                    &root,
                    ListDirOptions::new(),
                    |a, b| a.path().cmp(b.path()),
                    |_| true,
                    |listing, depth| {
                        if depth == 0 {
                            swap()?;
                        }
                        observed.extend(
                            listing
                                .entries
                                .into_iter()
                                .map(|entry| entry.into_parts().0),
                        );
                        Ok(WalkControl::Continue)
                    },
                )
            };
            let cleanup = fs.remove_dir_all(&root);
            let moved_cleanup = if changed {
                fs.remove_dir_all(&moved)
            } else {
                Ok(())
            };
            result.unwrap();
            cleanup.unwrap();
            moved_cleanup.unwrap();
            assert!(changed);
            assert!(
                observed.iter().any(|path| path.ends_with("leaf")),
                "lost observed subtree: {observed:?}"
            );
            assert!(
                !observed.iter().any(|path| path.ends_with("secret")),
                "followed replacement: {observed:?}"
            );
        }
    }
}

#[test]
fn ordered_traversal_uses_fewer_rpcs_than_reopening_directory_paths() {
    for (version, fs, metrics) in clients() {
        let root = fixture();
        for i in 0..16 {
            fs.create_dir_all(format!("{root}/d{i:02}/deep")).unwrap();
            fs.write(format!("{root}/d{i:02}/deep/file"), b"data")
                .unwrap();
        }
        let options = ListDirOptions::new().with_fields(Attributes::MODE);
        let before = metrics.snapshot().calls;
        let mut pending = vec![std::path::PathBuf::from(&root)];
        let mut baseline = Vec::new();
        while let Some(path) = pending.pop() {
            let entries = fs.read_dir_with_options(&path, options).unwrap();
            for entry in entries {
                if entry.attrs().is_dir() {
                    pending.push(entry.path().to_path_buf());
                }
                baseline.push(entry.into_parts().0);
            }
        }
        let baseline_calls = metrics.snapshot().calls - before;
        let before = metrics.snapshot().calls;
        let mut observed = Vec::new();
        let result = fs.visit_dirs_ordered(
            &root,
            options,
            |a, b| a.path().cmp(b.path()),
            |_| true,
            |listing, _| {
                observed.extend(
                    listing
                        .entries
                        .into_iter()
                        .map(|entry| entry.into_parts().0),
                );
                Ok(WalkControl::Continue)
            },
        );
        let calls = metrics.snapshot().calls - before;
        let cleanup = fs.remove_dir_all(&root);
        result.unwrap();
        cleanup.unwrap();
        baseline.sort();
        observed.sort();
        assert_eq!(observed, baseline);
        eprintln!(
            "{version:?}: anchored ordered={calls} RPCs, reopened paths={baseline_calls} RPCs"
        );
        assert!(
            calls * 2 < baseline_calls,
            "anchored ordered paging lost its expected RPC reduction"
        );
    }
}
