//! Rust-native scalar and vector NFS workflow.

use std::error::Error;

use vnfs::prelude::*;

fn main() -> Result<(), Box<dyn Error>> {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    let client = Nfs::connect(host)?;
    let root = format!("/vnfs-demo-{}", std::process::id());
    let _ = client.remove_dir_all(&root);
    client.create_dir_all(&root)?;

    let paths = [format!("{root}/file-1"), format!("{root}/file-2")];
    let files = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open_many(&paths)?;

    client
        .write_many_outcomes(&[
            files[0].write_request_at(0, b"hello"),
            files[1].write_request_at(0, b"world"),
        ])?
        .into_values()?;
    let contents = client
        .read_many_outcomes(&[
            files[0].read_request_at(0, 5),
            files[1].read_request_at(0, 5),
        ])?
        .into_values()?;

    println!("{}", String::from_utf8_lossy(&contents[0].data));
    println!("{}", String::from_utf8_lossy(&contents[1].data));
    drop(files);
    client.remove_dir_all(root)?;
    Ok(())
}
