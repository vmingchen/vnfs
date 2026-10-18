use super::*;
fn bitmap(attrs: &[u32]) -> bitmap4 {
    let mut mask = bitmap4 {
        bitmap4_len: 3,
        map: [0; 3],
    };
    for attr in attrs {
        mask.map[(attr / 32) as usize] |= 1 << (attr % 32);
    }
    mask
}
#[test]
fn omitted_attributes_remain_unknown_and_zero_is_preserved() {
    let attrs = bitmap(&[FATTR4_MAXNAME, FATTR4_SPACE_FREE, FATTR4_SPACE_TOTAL]);
    let mut bytes = 255u32.to_be_bytes().to_vec();
    bytes.extend_from_slice(&0u64.to_be_bytes());
    bytes.extend_from_slice(&12345u64.to_be_bytes());
    let stats = decode_filesystem_stats(attrs, &bytes).unwrap();
    assert_eq!(stats.max_name_len(), Some(255));
    assert_eq!(stats.free_bytes(), Some(0));
    assert_eq!(stats.total_bytes(), Some(12345));
    assert_eq!(stats.available_bytes(), None);
    assert_eq!(stats.max_links(), None);
    assert_eq!(
        decode_filesystem_stats(bitmap(&[]), &[]).unwrap(),
        FilesystemStats::default()
    );
}
#[test]
fn all_statistics_decode_without_losing_zero_maxima_or_presence() {
    let attrs = bitmap(&filesystem_attributes());
    let mut bytes = Vec::new();
    // Attribute values are encoded in bitmap order, not struct field order.
    for value in [u64::MAX, 0, 1, u64::MAX] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.extend_from_slice(&u32::MAX.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    for value in [0, u64::MAX, 12345] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    assert_eq!(
        decode_filesystem_stats(attrs, &bytes).unwrap(),
        FilesystemStats::default()
            .with_available_files(Some(u64::MAX))
            .with_free_files(Some(0))
            .with_total_files(Some(1))
            .with_max_file_size(Some(u64::MAX))
            .with_max_links(Some(u64::from(u32::MAX)))
            .with_max_name_len(Some(0))
            .with_available_bytes(Some(0))
            .with_free_bytes(Some(u64::MAX))
            .with_total_bytes(Some(12345))
    );
}

#[test]
fn malformed_statistics_are_rejected() {
    assert!(decode_filesystem_stats(bitmap(&[FATTR4_SPACE_TOTAL]), &[0; 4]).is_err());
    assert!(decode_filesystem_stats(bitmap(&[]), &[0; 4]).is_err());
    assert!(decode_filesystem_stats(bitmap(&[FATTR4_SIZE]), &[0; 8]).is_err());
    assert!(
        decode_filesystem_stats(
            bitmap4 {
                bitmap4_len: 4,
                map: [0; 3]
            },
            &[]
        )
        .is_err()
    );
}
