use crate::{Adb, VfError, VfResult};

type Block = (u64, Option<u64>, Option<u64>);

/// Plan every block before opening a file. Offsets include checked field ends;
/// payloads remain borrowed by each backend during execution.
pub fn adb_layout(pattern: &Adb, index: usize) -> VfResult<Vec<Block>> {
    let overflow = || VfError::failure(index, libc::EOVERFLOW as u32);
    let pattern_len = u64::try_from(pattern.adb_pattern_data.len()).map_err(|_| overflow())?;
    let mut layout = Vec::with_capacity(pattern.adb_block_count);
    for block in 0..pattern.adb_block_count {
        let base = (block as u64)
            .checked_mul(pattern.adb_block_size)
            .and_then(|offset| pattern.adb_offset.checked_add(offset))
            .ok_or_else(overflow)?;
        let number = pattern
            .adb_block_num
            .checked_add(block as u64)
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
        layout.push((
            number,
            field(pattern.adb_reloff_blocknum, 8)?,
            field(pattern.adb_reloff_pattern, pattern_len)?,
        ));
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn layout_keeps_optional_fields_and_zero_blocks() {
        let mut p = Adb::blocknum_only("/unused", 10, 100, 2, 4, 7);
        p.adb_reloff_pattern = Some(20);
        p.adb_pattern_data = b"payload".to_vec();
        assert_eq!(
            adb_layout(&p, 0).unwrap(),
            [(7, Some(14), Some(30)), (8, Some(114), Some(130))]
        );
        p.adb_reloff_blocknum = None;
        p.adb_reloff_pattern = None;
        assert_eq!(
            adb_layout(&p, 0).unwrap(),
            [(7, None, None), (8, None, None)]
        );
        p.adb_block_count = 0;
        p.adb_offset = u64::MAX;
        assert!(adb_layout(&p, 0).unwrap().is_empty());
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
                Ok(layout) => { prop_assert!(valid); prop_assert_eq!(layout, expected); }
                Err(error) => { prop_assert!(!valid); prop_assert_eq!(error.index(), Some(4));
                    prop_assert_eq!(error.err_no(), libc::EOVERFLOW as u32); }
            }
        }
    }
}
