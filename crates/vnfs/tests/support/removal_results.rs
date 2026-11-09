use vnfs::directory::{RemoveMode, RemoveOptions};
use vnfs::files::{Vfsi, VfsiExt};

pub fn check(fs: &impl Vfsi, base: &str) {
    assert!(
        fs.vremove::<&str>(&[], RemoveMode::Tree, RemoveOptions::default())
            .unwrap()
            .is_empty()
    );
    for mode in [RemoveMode::Entry, RemoveMode::Tree, RemoveMode::Contents] {
        let dir = format!("{base}/{mode:?}");
        fs.create_dir_all(&dir).unwrap();
        let roots = [
            format!("{dir}/first"),
            format!("{dir}/missing"),
            format!("{dir}/last"),
        ];
        for root in [&roots[0], &roots[2]] {
            if mode == RemoveMode::Entry {
                fs.write(root, b"data").unwrap();
            } else {
                fs.create_dir_all(format!("{root}/nested")).unwrap();
                fs.write(format!("{root}/nested/file"), b"data").unwrap();
            }
        }
        let results = fs.vremove(&roots, mode, RemoveOptions::default()).unwrap();
        assert_eq!(results.len(), roots.len());
        assert!(results[0].is_ok(), "{mode:?}: {:?}", results[0]);
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
        assert!(results[2].is_ok(), "{mode:?}: {:?}", results[2]);
        for root in [&roots[0], &roots[2]] {
            if mode == RemoveMode::Contents {
                assert!(fs.attrs(root).unwrap().is_dir());
                assert!(fs.read_dir(root).unwrap().is_empty());
            } else {
                assert_eq!(
                    fs.attrs(root).unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
            }
        }
        fs.remove_dir_all(&dir).unwrap();
    }

    let outside = format!("{base}/outside");
    let keep = format!("{outside}/keep");
    let link = format!("{base}/link");
    let healthy = format!("{base}/healthy");
    fs.create_dir_all(&outside).unwrap();
    fs.write(&keep, b"must remain").unwrap();
    fs.symlink("outside", &link).unwrap();
    fs.create_dir_all(&healthy).unwrap();
    fs.write(format!("{healthy}/file"), b"remove").unwrap();
    let outcomes = fs
        .vremove(
            &[&link, &healthy],
            RemoveMode::Contents,
            RemoveOptions::default(),
        )
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(
        outcomes[0].is_err(),
        "contents must not follow a final symlink"
    );
    assert!(
        outcomes[1].is_ok(),
        "wrong-type root must not suppress a sibling"
    );
    assert_eq!(fs.read(&keep).unwrap(), b"must remain");
    assert!(fs.read_dir(&healthy).unwrap().is_empty());
    fs.remove_file(&link).unwrap();
    fs.remove_dir_all(&outside).unwrap();
    fs.remove_dir(&healthy).unwrap();
}
