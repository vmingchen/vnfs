use crate::api::{ListDirOptions, TraversalCompletion, Vfsi, VfsiExt};
use std::num::NonZeroUsize;
use std::path::Path;
use vfsi_core::{AttrMask, FailureState};

/// Shared public contract; the caller owns and removes the fixture directory.
pub fn public_directory_results<C: Vfsi>(fs: &C, other: &C, directory: &str, recursive: bool) {
    let page_size = NonZeroUsize::new(1).unwrap();
    let options = ListDirOptions::new().with_fields(AttrMask::MODE);
    assert!(
        fs.vlistdirs::<&str, std::ops::ControlFlow<()>>(&[], options, |_, _| panic!(
            "empty visitor does no work"
        ),)
            .unwrap()
            .is_empty()
    );
    assert!(
        fs.vlistdir_pages::<&str>(&[], vec![], options.with_recursive(true), page_size)
            .unwrap()
            .is_empty()
    );
    if let Err(error) = fs.create_dir(directory) {
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    }
    let first = format!("{directory}/first");
    let last = format!("{directory}/last");
    let missing = format!("{directory}/missing");
    let empty = format!("{directory}/empty");
    for root in [&first, &last, &empty] {
        fs.create_dir(root).unwrap();
    }
    for root in [&first, &last] {
        for index in 0..2 {
            fs.write(format!("{root}/file{index}"), b"payload").unwrap();
        }
    }
    let pages = fs
        .vlistdir_pages(
            &[&first, &missing, &last],
            vec![None, None, None],
            options,
            page_size,
        )
        .unwrap();
    assert_eq!(pages.len(), 3);
    assert!(pages[0].is_ok() && pages[2].is_ok());
    let error = pages[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    let _: &std::io::Error = error.cause();
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert!(error.progress().is_none());
    let mut pages = pages.into_iter();
    let first_page = pages.next().unwrap().unwrap();
    let first_name = first_page.listing.entries[0].path().to_owned();
    let first_cursor = first_page
        .continuation
        .expect("first directory has more entries");
    let _ = pages.next();
    let last_page = pages.next().unwrap().unwrap();
    let last_name = last_page.listing.entries[0].path().to_owned();
    let last_cursor = last_page
        .continuation
        .expect("last directory has more entries");
    let mut foreign = other
        .vlistdir_pages(&[&last], vec![None], options, page_size)
        .unwrap();
    let foreign = foreign
        .remove(0)
        .unwrap()
        .continuation
        .expect("foreign live cursor");

    let moved = format!("{directory}/moved");
    fs.rename(&first, &moved).unwrap();
    fs.create_dir(&first).unwrap();
    fs.write(format!("{first}/keep"), b"replacement").unwrap();
    let pages = fs
        .vlistdir_pages(
            &[&first, &last, &last],
            vec![Some(first_cursor), Some(foreign), Some(last_cursor)],
            options,
            page_size,
        )
        .unwrap();
    assert_eq!(pages.len(), 3);
    let error = pages[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::NotAttempted);
    assert_eq!(
        vfsi_core::internal::errors::to_errno(error.cause()) as u32,
        libc::EBADF as u32
    );
    let _: &std::io::Error = error.cause();
    for (index, previous) in [(0, first_name), (2, last_name)] {
        let page = pages[index].as_ref().unwrap();
        assert_eq!(page.listing.entries.len(), 1);
        assert_ne!(page.listing.entries[0].path(), previous);
        assert_ne!(page.listing.entries[0].path().file_name().unwrap(), "keep");
    }
    assert_eq!(fs.read(format!("{first}/keep")).unwrap(), b"replacement");
    drop(pages);

    let mut visits = [0usize; 3];
    let mut names = [
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
    ];
    let result = fs
        .vlistdirs(&[&moved, &missing, &last], options, |index, page| {
            visits[index] += 1;
            for entry in page.entries {
                assert!(
                    names[index].insert(entry.into_parts().0),
                    "never repeat delivered entries"
                );
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].as_ref().unwrap(), &TraversalCompletion::Complete);
    assert_eq!(result[2].as_ref().unwrap(), &TraversalCompletion::Complete);
    assert_eq!(
        result[1].as_ref().unwrap_err().cause().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(visits[0] > 0 && visits[1] == 0 && visits[2] > 0);
    assert_eq!(names[0].len(), 2);
    assert_eq!(names[2].len(), 2);

    let mut visits = [0usize; 2];
    let result = fs
        .vlistdirs(&[&moved, &last], options, |index, _| {
            visits[index] += 1;
            if index == 0 {
                Err(std::io::Error::from_raw_os_error(libc::EACCES))
            } else {
                Ok(std::ops::ControlFlow::Continue(()))
            }
        })
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(visits[0], 1, "failed callbacks are never retried");
    assert!(
        visits[1] > 0,
        "a failed callback does not suppress an independent root"
    );
    assert_eq!(
        result[0].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(
        vfsi_core::internal::errors::to_errno(result[0].as_ref().unwrap_err().cause()) as u32,
        libc::EACCES as u32
    );
    assert_eq!(result[1].as_ref().unwrap(), &TraversalCompletion::Complete);

    let mut seen = Vec::new();
    let result = fs
        .vlistdirs(&[&moved, &last], options, |index, _| {
            seen.push(index);
            Ok(std::ops::ControlFlow::Break(()))
        })
        .unwrap();
    assert_eq!(seen, [0]);
    assert_eq!(
        result
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap(),
        [TraversalCompletion::Stopped, TraversalCompletion::Stopped]
    );

    for options in [options.with_max_entries(1), options.with_max_path_bytes(0)] {
        let pages = fs
            .vlistdir_pages(
                &[&moved, &empty],
                vec![None, None],
                options,
                NonZeroUsize::new(4).unwrap(),
            )
            .unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(
            vfsi_core::internal::errors::to_errno(pages[0].as_ref().unwrap_err().cause()) as u32,
            libc::EFBIG as u32
        );
        assert!(pages[1].as_ref().unwrap().listing.entries.is_empty());
    }
    if recursive {
        fs.create_dir_all(format!("{moved}/deep/sub")).unwrap();
        fs.write(format!("{moved}/deep/sub/file"), b"recursive")
            .unwrap();
        let mut seen = Vec::new();
        let result = fs
            .vlistdirs(
                &[&moved, &missing, &last],
                options.with_recursive(true),
                |index, page| {
                    seen.push((index, page.path));
                    Ok(std::ops::ControlFlow::Continue(()))
                },
            )
            .unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].as_ref().unwrap(), &TraversalCompletion::Complete);
        assert!(result[1].is_err());
        assert_eq!(result[2].as_ref().unwrap(), &TraversalCompletion::Complete);
        assert!(seen.iter().any(|(index, path)| *index == 0 && path == Path::new(&format!("{moved}/deep/sub"))));
        assert!(seen.iter().any(|(index, _)| *index == 2));
    }
}
