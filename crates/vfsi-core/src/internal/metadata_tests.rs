use super::*;
use crate::SetAttrsOp;
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn metadata_validation_rejects_reserved_ids_and_preserves_absent_times() {
    assert_eq!(
        validate_setattrs(&SetAttrsOp::new("/file")).unwrap(),
        [None; 2]
    );
    for op in [
        SetAttrsOp::new("/file").uid(u32::MAX),
        SetAttrsOp::new("/file").gid(u32::MAX),
    ] {
        assert_eq!(
            validate_setattrs(&op).unwrap_err().raw_code(),
            crate::ERR_INVAL
        );
    }
    assert!(validate_setattrs(&SetAttrsOp::new("/file").uid(0).gid(u32::MAX - 1)).is_ok());
}

#[test]
fn timestamp_normalization_handles_both_sides_of_epoch() {
    for (time, expected) in [
        (UNIX_EPOCH, (0, 0)),
        (UNIX_EPOCH + Duration::new(7, 123), (7, 123)),
        (UNIX_EPOCH - Duration::new(7, 0), (-7, 0)),
        (UNIX_EPOCH - Duration::new(7, 123), (-8, 999_999_877)),
    ] {
        let op = SetAttrsOp::new("/file").accessed(time).modified(time);
        assert_eq!(validate_setattrs(&op).unwrap(), [Some(expected); 2]);
    }
    if let Some(time) = UNIX_EPOCH.checked_sub(Duration::new(i64::MAX as u64, 1)) {
        assert_eq!(
            validate_setattrs(&SetAttrsOp::new("/file").modified(time)).unwrap(),
            [None, Some((i64::MIN, 999_999_999))]
        );
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[test]
fn timestamp_normalization_accepts_the_minimum_signed_second() {
    for (duration, expected) in [
        (Duration::from_secs(i64::MAX as u64 + 1), (i64::MIN, 0)),
        (Duration::new(i64::MAX as u64, 1), (i64::MIN, 999_999_999)),
        (Duration::new(i64::MAX as u64, 999_999_999), (i64::MIN, 1)),
    ] {
        let time = UNIX_EPOCH
            .checked_sub(duration)
            .expect("Unix signed-second time");
        let op = SetAttrsOp::new("/file").accessed(time).modified(time);
        assert_eq!(validate_setattrs(&op).unwrap(), [Some(expected); 2]);
    }
}
