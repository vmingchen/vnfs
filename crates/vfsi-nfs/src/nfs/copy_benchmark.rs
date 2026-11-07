//! Explicit copy protocol experiment; production copy dispatch is unchanged.
use super::*;
use crate::telemetry::Telemetry;
use std::time::Instant;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
#[ignore = "manual writable-NFS new-destination copy experiment"]
fn compare_bounded_new_file_copy_waves() {
    let host = std::env::var("VFSI_NFS_SERVER").expect("set VFSI_NFS_SERVER");
    let mount =
        PathBuf::from(std::env::var_os("VFSI_NFS_TEST_MOUNT").expect("set export-root mount"));
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(matches!(minor, 1 | 2));
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("vfsi-copy-waves-{}-{unique}", std::process::id());
    let local = mount.join(&name);
    std::fs::create_dir(&local).unwrap();
    let fixture = Fixture(local);
    let telemetry = Telemetry::default();
    let mut backend = NfsClientBuilder::new(host)
        .minor_version(Some(minor))
        .auto_reconnect(false)
        .telemetry(telemetry.clone())
        .connect()
        .unwrap();
    for (case, files, bytes) in [("small", 32, 4096), ("large", 8, 2 * 1024 * 1024)] {
        let source = fixture.0.join(case);
        std::fs::create_dir(&source).unwrap();
        for index in 0..files {
            let payload: Vec<_> = (0..bytes)
                .map(|offset| ((offset + index) % 251) as u8)
                .collect();
            std::fs::write(source.join(format!("source-{index:02}")), payload).unwrap();
        }
        for sample in 0..3 {
            let order = if sample % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for prototype in order {
                let destination = format!("{case}/out-{sample}-{prototype}");
                let local_destination = fixture.0.join(&destination);
                std::fs::create_dir(&local_destination).unwrap();
                let pairs: Vec<_> = (0..files)
                    .map(|index| {
                        ExtentPair::from_os_paths(
                            &PathBuf::from(format!("/{name}/{case}/source-{index:02}")),
                            0,
                            &PathBuf::from(format!("/{name}/{destination}/file-{index:02}")),
                            0,
                            Some(bytes as u64),
                        )
                    })
                    .collect();
                let before = telemetry.snapshot();
                let start = Instant::now();
                let operation = if prototype {
                    vfsi_sync::test_support::copy_new_files_batched(
                        &mut backend,
                        &pairs,
                        0o644,
                        None,
                    )
                } else {
                    backend.vcopy_data_impl(&pairs)
                };
                let elapsed = start.elapsed().as_micros();
                let after = telemetry.snapshot();
                operation.unwrap();
                assert!(
                    backend.open_files.is_empty(),
                    "copy must close every owned descriptor"
                );
                assert_eq!(after.transport_errors, before.transport_errors);
                for index in 0..files {
                    assert_eq!(
                        std::fs::read(source.join(format!("source-{index:02}"))).unwrap(),
                        std::fs::read(local_destination.join(format!("file-{index:02}"))).unwrap(),
                        "all copied bytes and EOF sizes must match",
                    );
                }
                println!(
                    "COPY_WAVES {{\"minor\":{minor},\"case\":\"{case}\",\"prototype\":{prototype},\"files\":{files},\"bytes_per_file\":{bytes},\"sample\":{sample},\"rpc_attempts\":{},\"nfs_rejections\":{},\"operations\":{},\"request_bytes\":{},\"elapsed_us\":{elapsed}}}",
                    after.calls - before.calls,
                    after.nfs_errors - before.nfs_errors,
                    after.operations - before.operations,
                    after.request_bytes - before.request_bytes,
                );
                std::fs::remove_dir_all(local_destination).unwrap();
            }
        }
    }
}
