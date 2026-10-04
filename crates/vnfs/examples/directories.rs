use vnfs::{ControlFlow, Fs, FsExt, MetadataFields, Nfs, ReadDirOptions, WalkOptions};

pub fn run(fs: &impl Fs, paths: &[String], tree: &str) -> vnfs::Result<usize> {
    // Attributes arrive with the listings, avoiding a scalar stat per entry.
    // Bound each cohort; walk the tree once, not once per listing batch.
    for cohort in paths.chunks(64) {
        let listings = fs.read_dirs_with_options(
            cohort,
            MetadataFields::MODE | MetadataFields::SIZE,
            ReadDirOptions::new(),
        )?;
        for listing in listings {
            for entry in listing.entries {
                println!("{:?}: {} bytes", entry.path(), entry.metadata().len());
            }
        }
    }
    // Visit bounded pages instead of collecting a large tree.
    // Symlinks are not followed; iteration order is backend-defined.
    let mut entries = 0;
    fs.visit_walk_with_options_one(tree, WalkOptions::new(), |_| {
        entries += 1;
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
