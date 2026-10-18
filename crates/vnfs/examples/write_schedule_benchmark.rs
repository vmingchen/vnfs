//! Complete-write scheduler timing with an acknowledging sink and real POSIX I/O.
//! Run via scripts/benchmark-write-scheduler.py for paired release comparisons.

use std::{hint::black_box, path::Path, time::Instant};
use vfsi_sync::VfsiExt;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::{
    FileOperand, FsClient, OpenFlags, OpenOp, ReadOp, ReadResult, SetAttrsOp, Target, VfAttrs,
    VfError, VfOffset, VfPathBase, VfResult, WriteOp, WriteResult,
};

struct Sink {
    ends: Vec<u64>,
    max_write: usize,
    dispatches: usize,
}

impl Sink {
    fn open(&mut self) -> FileOperand {
        let fd = i32::try_from(self.ends.len()).expect("too many benchmark files");
        self.ends.push(0);
        FileOperand::from_fd(fd)
    }
}

impl HandleBackend for Sink {
    // BEGIN OBJECT REFERENCE HOOK
    fn open_object_ref(
        &self,
        file: &FileOperand,
    ) -> VfResult<Option<vfsi_sync::backend::OpenObjectRef<'_>>> {
        Ok(Some(vfsi_sync::backend::OpenObjectRef::Id(
            vfsi_sync::FsObjId::new(0, file.fd().expect("sink descriptor") as u64),
        )))
    }
    // END OBJECT REFERENCE HOOK

    fn open_impl(&mut self, _: &OpenOp) -> VfResult<FileOperand> {
        Ok(self.open())
    }
    fn open_path_impl(&mut self, _: VfPathBase, _: &Path, _: i32, _: u32) -> VfResult<FileOperand> {
        Ok(self.open())
    }
    fn close_impl(&mut self, _: &FileOperand) -> VfResult<()> {
        Ok(())
    }
    fn sync_data(&mut self, _: &FileOperand) -> VfResult<()> {
        Ok(())
    }
    fn read_impl(&mut self, _: &ReadOp) -> VfResult<ReadResult> {
        Err(VfError::unsupported(0))
    }
    fn write_impl(&mut self, request: WriteOp<&FileOperand, &[u8]>) -> VfResult<WriteResult> {
        Ok(self.vwrite_impl(&[request])?.remove(0))
    }
    fn seek_impl(&mut self, _: &FileOperand, _: std::io::SeekFrom) -> VfResult<u64> {
        Err(VfError::unsupported(0))
    }
    fn metadata_impl(
        &mut self,
        _: Target<'_, FileOperand>,
        _: vfsi_core::api::AttrsOptions,
    ) -> VfResult<VfAttrs> {
        Err(VfError::unsupported(0))
    }
    fn set_attributes_impl(&mut self, _: &SetAttrsOp<Target<'_, FileOperand>>) -> VfResult<()> {
        Err(VfError::unsupported(0))
    }
}

impl VectorBackend for Sink {
    fn vread_impl(&mut self, _: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        Err(VfError::unsupported(0))
    }
    fn vwrite_impl(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> VfResult<Vec<WriteResult>> {
        self.dispatches += 1;
        Ok(writes
            .iter()
            .map(|request| {
                let end = &mut self.ends[request.file().fd().expect("sink descriptor") as usize];
                let offset = match request.offset() {
                    VfOffset::At(offset) => offset,
                    VfOffset::End => *end,
                    _ => panic!("unexpected sink offset"),
                };
                let written = black_box(request.data()).len().min(self.max_write);
                *end = (*end).max(offset + written as u64);
                WriteResult {
                    file: request.file().clone(),
                    offset,
                    bytes_written: written,
                    stable: true,
                }
            })
            .collect())
    }
}

struct Item {
    file: usize,
    offset: u64,
    length: usize,
}

struct Plan {
    items: Vec<Item>,
    files: usize,
    max_write: usize,
    append: bool,
}

fn plan(scenario: &str, count: usize) -> Plan {
    assert!(count > 0);
    let mut max_write = usize::MAX;
    let mut append = false;
    let items: Vec<_> = (0..count)
        .map(|index| {
            let (file, offset, length) = match scenario {
                "overlap" => (0, 0, 32),
                "chain" => (0, index as u64 * 32, 64),
                "scatter" => (0, index as u64 * 64, 32),
                "independent" => (index, 0, 32),
                "overlap-short" => {
                    max_write = 32;
                    (0, 0, 128)
                }
                "independent-short" => {
                    max_write = 32;
                    (index, 0, 128)
                }
                "mixed" => {
                    max_write = 32;
                    let base = (index / 4) as u64 * 256;
                    match index % 4 {
                        0 => (0, base, 32),
                        1 => (0, base + 64, 64),
                        2 => (1, base + 128, 96),
                        _ => (0, base + 16, 96),
                    }
                }
                "append" => {
                    append = true;
                    (0, index as u64 * 64, 32)
                }
                _ => panic!("unknown scenario {scenario}"),
            };
            Item {
                file,
                offset,
                length,
            }
        })
        .collect();
    let files = items.iter().map(|item| item.file).max().unwrap() + 1;
    Plan {
        items,
        files,
        max_write,
        append,
    }
}

fn measure<F: VectorBackend + 'static>(
    client: FsClient<F>,
    plan: &Plan,
    iterations: usize,
    warmups: usize,
    real_root: Option<&Path>,
) -> (u128, F) {
    let paths: Vec<_> = (0..plan.files)
        .map(|index| format!("/file-{index}"))
        .collect();
    let mut flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    if plan.append {
        flags |= OpenFlags::APPEND;
    }
    let files: Vec<_> = paths
        .iter()
        .map(|path| {
            client
                .open_with(OpenOp::new(path, flags))
                .expect("open benchmark file")
        })
        .collect();
    let payloads: Vec<_> = plan
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| vec![(index % 251 + 1) as u8; item.length])
        .collect();
    let requests: Vec<_> = plan
        .items
        .iter()
        .zip(&payloads)
        .map(|(item, payload)| files[item.file].write_request_at(item.offset, payload))
        .collect();
    // Setup, correctness checks, file open/close, and fixture removal are
    // outside the timed region. Both binaries run the same monomorphized API.
    let before = client
        .vwrite_all_native(&requests)
        .expect("verify write results");
    assert_eq!(before.len(), plan.items.len());
    for (result, item) in before.iter().zip(&plan.items) {
        assert_eq!(result.bytes_written, item.length);
        if !plan.append {
            assert_eq!(result.offset, item.offset);
        }
    }
    for _ in 0..warmups {
        black_box(
            client
                .vwrite_all_native(black_box(&requests))
                .expect("warmup writes"),
        );
    }
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(
            client
                .vwrite_all_native(black_box(&requests))
                .expect("measured writes"),
        );
    }
    let elapsed = start.elapsed().as_nanos();
    let after = client
        .vwrite_all_native(&requests)
        .expect("verify final write results");
    assert_eq!(after.len(), plan.items.len());
    assert!(
        after
            .iter()
            .zip(&plan.items)
            .all(|(result, item)| result.bytes_written == item.length)
    );
    if let Some(root) = real_root {
        assert!(!plan.append, "real append benchmark would grow the fixture");
        let mut expected = vec![Vec::new(); plan.files];
        for (item, payload) in plan.items.iter().zip(&payloads) {
            let target = &mut expected[item.file];
            let offset = item.offset as usize;
            target.resize(target.len().max(offset + payload.len()), 0);
            target[offset..offset + payload.len()].copy_from_slice(payload);
        }
        for (path, contents) in paths.iter().zip(expected) {
            assert_eq!(
                std::fs::read(root.join(&path[1..])).expect("read fixture"),
                contents
            );
        }
    }
    drop(requests);
    for file in files {
        file.close().expect("close benchmark file");
    }
    let backend = match client.into_inner() {
        Ok(backend) => backend,
        Err(_) => panic!("benchmark retained an open handle"),
    };
    (elapsed, backend)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 7 {
        return Err("usage: write_schedule_benchmark sink|posix SCENARIO COUNT ITERATIONS WARMUPS FIXTURE_ROOT".into());
    }
    let backend = args[1].as_str();
    let scenario = args[2].as_str();
    let count: usize = args[3].parse()?;
    let iterations: usize = args[4].parse()?;
    let warmups: usize = args[5].parse()?;
    assert!(iterations > 0);
    let plan = plan(scenario, count);
    let (elapsed, dispatches) = match backend {
        "sink" => {
            let (elapsed, sink) = measure(
                FsClient::new(Sink {
                    ends: Vec::new(),
                    max_write: plan.max_write,
                    dispatches: 0,
                }),
                &plan,
                iterations,
                warmups,
                None,
            );
            (elapsed, Some(sink.dispatches))
        }
        "posix" => {
            assert_eq!(plan.max_write, usize::MAX, "short writes need the sink");
            assert!(!plan.append, "append timing uses the sink");
            let root = tempfile::Builder::new()
                .prefix("write-schedule-")
                .tempdir_in(&args[6])?;
            let (elapsed, _) = measure(
                vfsi_posix::connect(root.path())?,
                &plan,
                iterations,
                warmups,
                Some(root.path()),
            );
            (elapsed, None)
        }
        _ => return Err("unknown backend".into()),
    };
    let dispatches = dispatches.map_or_else(|| "null".to_owned(), |count| count.to_string());
    println!(
        "{{\"backend\":\"{backend}\",\"scenario\":\"{scenario}\",\"count\":{count},\"iterations\":{iterations},\"warmups\":{warmups},\"elapsed_ns\":{elapsed},\"ns_per_batch\":{},\"dispatches\":{dispatches}}}",
        elapsed as f64 / iterations as f64,
    );
    Ok(())
}
