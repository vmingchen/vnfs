use vnfs::files::{StreamOptions, Vfsi};
use vnfs::nfs::Nfs;

pub fn run(fs: &impl Vfsi, path: &str) -> vnfs::Result<u64> {
    let mut bytes = 0_u64;
    fs.vstream(
        &[path],
        StreamOptions::new().chunk_size(std::num::NonZeroUsize::new(1024 * 1024).unwrap()),
        |index, offset, chunk| {
            // Process the borrowed chunk here; it is valid only in this callback.
            // Do not collect chunks: that would defeat the memory bound.
            assert_eq!(index, 0);
            assert_eq!(offset, bytes);
            bytes += chunk.len() as u64;
            Ok(std::ops::ControlFlow::Continue(())) // false stops successfully; callback errors propagate.
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
