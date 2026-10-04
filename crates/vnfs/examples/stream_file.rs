use vnfs::{Fs, FsExt, Nfs, ReadStreamOptions};

pub fn run(fs: &impl Fs, path: &str) -> vnfs::Result<u64> {
    let mut bytes = 0_u64;
    fs.read_stream_with_options_one(
        path,
        ReadStreamOptions::new().chunk_size(1024 * 1024),
        |offset, chunk| {
            // Process the borrowed chunk here; it is valid only in this callback.
            // Do not collect chunks: that would defeat the memory bound.
            assert_eq!(offset, bytes);
            bytes += chunk.len() as u64;
            Ok(true) // false stops successfully; callback errors propagate.
        },
    )?;
    Ok(bytes)
}

fn main() -> vnfs::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: stream_file HOST EXPORT_ROOT FILE");
        std::process::exit(2);
    }
    let fs = Nfs::builder(&args[1]).root(&args[2]).connect()?;
    println!("processed {} bytes", run(&fs, &args[3])?);
    Ok(())
}
