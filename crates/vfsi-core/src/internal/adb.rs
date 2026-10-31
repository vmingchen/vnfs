use crate::{Adb, VfError, VfResult};

type Block = (u64, Option<u64>, Option<u64>);

/// Validate every block before opening a file, using constant planning storage.
/// All offsets increase monotonically, so checking the final block also proves
/// the earlier blocks and field ends fit. Execution borrows the payload.
pub fn adb_layout(
    pattern: &Adb,
    index: usize,
) -> VfResult<impl ExactSizeIterator<Item = Block> + DoubleEndedIterator> {
    let overflow = || VfError::errno(index, libc::EOVERFLOW as u32);
    let pattern_len = u64::try_from(pattern.adb_pattern_data.len()).map_err(|_| overflow())?;
    if let Some(last) = pattern.adb_block_count.checked_sub(1) {
        let last = u64::try_from(last).map_err(|_| overflow())?;
        let base = last
            .checked_mul(pattern.adb_block_size)
            .and_then(|offset| pattern.adb_offset.checked_add(offset))
            .ok_or_else(overflow)?;
        pattern
            .adb_block_num
            .checked_add(last)
            .ok_or_else(overflow)?;
        let field = |relative: Option<u64>, length| {
            relative
                .map(|relative| {
                    let offset = base.checked_add(relative).ok_or_else(overflow)?;
                    offset.checked_add(length).ok_or_else(overflow)?;
                    Ok(offset)
                })
                .transpose()
        };
        field(pattern.adb_reloff_blocknum, 8)?;
        field(pattern.adb_reloff_pattern, pattern_len)?;
    }
    Ok((0..pattern.adb_block_count).map(move |block| {
        // The checked last block bounds every conversion, sum and product here.
        let block = block as u64;
        let base = pattern.adb_offset + block * pattern.adb_block_size;
        (
            pattern.adb_block_num + block,
            pattern.adb_reloff_blocknum.map(|relative| base + relative),
            pattern.adb_reloff_pattern.map(|relative| base + relative),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn huge_block_count_returns_overflow_without_allocating() {
        let p = Adb::blocknum_only("/unused", u64::MAX, 1, usize::MAX, 0, 0);
        match adb_layout(&p, 7) {
            Err(error) => {
                assert_eq!(error.index(), Some(7));
                assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
            }
            Ok(_) => panic!("invalid final block must fail before allocation or execution"),
        }
    }

    #[test]
    fn final_block_number_and_field_ends_are_checked_before_execution() {
        for p in [
            Adb::blocknum_only("/unused", 0, 0, 2, 0, u64::MAX),
            Adb::blocknum_only("/unused", 0, u64::MAX, 3, 0, 0),
            Adb::blocknum_only("/unused", 0, u64::MAX / 2, 3, 0, 0),
        ] {
            match adb_layout(&p, 3) {
                Err(error) => {
                    assert_eq!(error.index(), Some(3));
                    assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
                }
                Ok(_) => panic!("a later overflow must fail before any blocks are executed"),
            }
        }
    }

    #[test]
    fn layout_keeps_optional_fields_and_zero_blocks() {
        let mut p = Adb::blocknum_only("/unused", 10, 100, 2, 4, 7);
        p.adb_reloff_pattern = Some(20);
        p.adb_pattern_data = b"payload".to_vec();
        assert_eq!(
            adb_layout(&p, 0).unwrap().collect::<Vec<_>>(),
            [(7, Some(14), Some(30)), (8, Some(114), Some(130))]
        );
        p.adb_reloff_blocknum = None;
        p.adb_reloff_pattern = None;
        assert_eq!(
            adb_layout(&p, 0).unwrap().collect::<Vec<_>>(),
            [(7, None, None), (8, None, None)]
        );
        p.adb_block_count = 0;
        p.adb_offset = u64::MAX;
        assert_eq!(adb_layout(&p, 0).unwrap().len(), 0);
    }

    proptest! {
        #[test]
        fn all_layout_arithmetic_matches_wide_integer_reference(
            base in any::<u64>(), stride in any::<u64>(), first in any::<u64>(),
            number in proptest::option::of(any::<u64>()), pattern in proptest::option::of(any::<u64>()),
            count in 0usize..5, bytes in 0usize..16,
        ) {
            let mut p = Adb::blocknum_only("/unused", base, stride, count, 0, first);
            p.adb_reloff_blocknum = number;
            p.adb_reloff_pattern = pattern;
            p.adb_pattern_data = vec![0; bytes];
            let mut expected = Vec::new();
            let mut valid = true;
            for b in 0..count {
                let offset = base as u128 + b as u128 * stride as u128;
                let n = first as u128 + b as u128;
                valid &= offset <= u64::MAX as u128 && n <= u64::MAX as u128;
                for (relative, length) in [(number, 8), (pattern, bytes as u128)] {
                    if let Some(relative) = relative {
                        valid &= offset + relative as u128 + length <= u64::MAX as u128;
                    }
                }
                expected.push((n as u64, number.map(|x| (offset + x as u128) as u64),
                    pattern.map(|x| (offset + x as u128) as u64)));
            }
            match adb_layout(&p, 4) {
                Ok(layout) => { prop_assert!(valid); prop_assert_eq!(layout.collect::<Vec<_>>(), expected); }
                Err(error) => { prop_assert!(!valid); prop_assert_eq!(error.index(), Some(4));
                    prop_assert_eq!(error.raw_code(), libc::EOVERFLOW as u32); }
            }
        }
    }
}
