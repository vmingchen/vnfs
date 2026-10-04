use vnfs::{Fs, FsExt, Nfs, OpenFlags, OpenRequest};

// A range read may be short: exact reads must advance by actual progress
// until their target length or EOF is reached.
pub fn run(fs: &impl Fs, paths: &[String]) -> vnfs::Result<Vec<Vec<u8>>> {
    let requests: Vec<_> = paths
        .iter()
        .map(|path| OpenRequest::new(path, OpenFlags::READ))
        .collect();
    let files = fs.vopen(&requests)?;
    let mut buffers = vec![[0_u8; 4096]; files.len()];
    let result = {
        let reads: Vec<_> = files
            .iter()
            .zip(&mut buffers)
            .map(|(file, buffer)| vnfs::ReadOp::into(file, 0, buffer))
            .collect();
        fs.readv(reads)
    };
    // Explicit close surfaces errors; dropping handles is best-effort only.
    let close = fs.closev(files);
    let results = result?;
    close?;
    Ok(buffers
        .iter()
        .zip(results)
        .map(|(buffer, result)| buffer[..result.read()].to_vec())
        .collect())
}

fn main() -> vnfs::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: open_handles HOST EXPORT_ROOT FILE [FILE ...]");
        std::process::exit(2);
    }
    let fs = Nfs::builder(&args[1]).root(&args[2]).connect()?;
    // Bound application-owned buffers as well as backend allocations.
    for paths in args[3..].chunks(64) {
        for data in run(&fs, paths)? {
            println!("read {} bytes", data.len());
        }
    }
    Ok(())
}
