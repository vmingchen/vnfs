use std::process;

use nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH;
use vnfs::client::{NfsClient, OpenCreate};
use vnfs::error::RpcError;

fn main() {
    let host = "127.0.0.1";

    println!("connecting to {} ...", host);
    let mut client = match NfsClient::connect(host) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect failed: {}", e);
            process::exit(1);
        }
    };
    println!("connected; root fh {} bytes", client.root().len());

    match read_hello(&mut client) {
        Ok(()) => println!("READ hello.txt: OK"),
        Err(e) => eprintln!("READ hello.txt failed: {}", e),
    }

    match write_read_back(&mut client) {
        Ok(()) => println!("WRITE/READ roundtrip: OK"),
        Err(e) => eprintln!("WRITE/READ roundtrip failed: {}", e),
    }

    match mkdir_symlink(&mut client) {
        Ok(()) => println!("MKDIR/SYMLINK test: OK"),
        Err(e) => eprintln!("MKDIR/SYMLINK test failed: {}", e),
    }
}

/// Create a directory and a symlink inside it via NFSv4 CREATE compounds,
/// then read the symlink target back. Verify from the kernel mount point
/// (/mnt/nfs) afterwards.
fn mkdir_symlink(client: &mut NfsClient) -> Result<(), RpcError> {
    let pid = std::process::id();
    let dir_name = format!("vnfs_dir_{}", pid);
    let link_name = format!("vnfs_link_{}", pid);
    let target = format!("/export/dir1/{}.target", pid);

    let dir = client.mkdir(&client.root().clone(), &dir_name)?;
    println!("mkdir {}: OK", dir_name);

    let link = client.symlink(&dir, &link_name, &target)?;
    println!("symlink {} -> {}: OK", link_name, target);

    let got = client.readlink(&link)?;
    let got = String::from_utf8_lossy(&got);
    println!("readlink {}: {}", link_name, got);
    if got != target {
        return Err(RpcError::transport(format!(
            "readlink mismatch: got {:?}, want {:?}",
            got, target
        )));
    }
    Ok(())
}

fn read_hello(client: &mut NfsClient) -> Result<(), RpcError> {
    let dir = client.root().clone();
    let (fh, stateid) = client.open(
        &dir,
        "hello.txt",
        OPEN4_SHARE_ACCESS_BOTH,
        OpenCreate::NoCreate,
    )?;

    // Read in a few chunks until EOF.
    let mut offset = 0u64;
    let mut content = Vec::new();
    loop {
        let (chunk, eof) = client.read(&fh, &stateid, offset, 4096)?;
        if chunk.is_empty() || eof {
            break;
        }
        offset += chunk.len() as u64;
        content.extend_from_slice(&chunk);
    }
    println!(
        "hello.txt content ({} bytes): {:?}",
        content.len(),
        String::from_utf8_lossy(&content)
    );
    client.close(&fh, &stateid)?;
    Ok(())
}

fn write_read_back(client: &mut NfsClient) -> Result<(), RpcError> {
    let name = format!("vnfs_scratch_{}.txt", std::process::id());
    let dir = client.resolve("")?;

    let (fh, stateid) = client.open(&dir, &name, OPEN4_SHARE_ACCESS_BOTH, OpenCreate::Guarded)?;
    println!("created {}", name);

    let data = b"hello from the rust vnfs client!\n0123456789\n";
    let (n, committed) = client.write(&fh, &stateid, 0, data)?;
    println!("wrote {} bytes, committed {}", n, committed);
    assert_eq!(n as usize, data.len());

    let (read_back, _eof) = client.read(&fh, &stateid, 0, data.len() as u32)?;
    println!(
        "read back {} bytes: {:?}",
        read_back.len(),
        String::from_utf8_lossy(&read_back)
    );
    if read_back != data {
        return Err(RpcError::transport("data mismatch after write/read"));
    }

    client.close(&fh, &stateid)?;
    Ok(())
}
