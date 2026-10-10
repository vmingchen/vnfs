use vnfs::directory::{Attributes, ControlFlow, ListDirOptions};
use vnfs::files::Vfsi;
use vnfs::nfs::Nfs;

pub fn run(fs: &impl Vfsi, paths: &[String], tree: &str) -> vnfs::Result<usize> {
    // Attributes arrive with the listings, avoiding a scalar stat per entry.
    // Bound each cohort; walk the tree once, not once per listing batch.
    for cohort in paths.chunks(64) {
        fs.vlistdirs(
            cohort,
            ListDirOptions::new().fields(Attributes::MODE | Attributes::SIZE),
            |_, listing| {
                for entry in listing.entries {
                    println!("{:?}: {:?} bytes", entry.path(), entry.attrs().len());
                }
                Ok(ControlFlow::Continue(()))
            },
        )?;
    }
    // Visit bounded pages instead of collecting a large tree.
    // Symlinks are not followed; iteration order is backend-defined.
    let mut entries = 0;
    fs.vlistdirs(&[tree], ListDirOptions::new().recursive(true), |_, page| {
        entries += page.entries.len();
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(entries)
}

fn main() -> vnfs::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: directories HOST EXPORT_ROOT TREE [DIRECTORY ...]");
        std::process::exit(2);
    }
    let fs = Nfs::builder(&args[1]).root(&args[2]).connect()?;
    println!("visited {} entries", run(&fs, &args[3..], &args[3])?);
    Ok(())
}
