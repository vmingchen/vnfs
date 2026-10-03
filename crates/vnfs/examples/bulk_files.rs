use vnfs::{Client, Nfs};

// The parent must exist. Never delete an existing directory to make room.
pub fn run(fs: &impl Client, fresh_root: &str) -> vnfs::Result<Vec<Vec<u8>>> {
    fs.create_dir(fresh_root)?; // Cleanup starts only after exclusive creation succeeds.
    let result = (|| {
        let paths = [
            format!("{fresh_root}/file-1"),
            format!("{fresh_root}/file-2"),
        ];
        fs.write_files(&[
            (&paths[0], b"hello".as_slice()),
            (&paths[1], b"world".as_slice()),
        ])?;
        fs.read_files(&paths) // Default aggregate returned-data budget: 16 MiB.
    })();
    let cleanup = fs.remove_dir_all(fresh_root);
    // Preserve an operation error, but report cleanup failure after successful I/O.
    let contents = result?;
    cleanup?;
    Ok(contents)
}

fn main() -> vnfs::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: bulk_files HOST EXPORT_ROOT FRESH_DIRECTORY");
        std::process::exit(2);
    }
    let fs = Nfs::builder(&args[1]).root(&args[2]).connect()?;
    let contents = run(&fs, &args[3])?;
    assert_eq!(contents, [b"hello".to_vec(), b"world".to_vec()]);
    Ok(())
}
