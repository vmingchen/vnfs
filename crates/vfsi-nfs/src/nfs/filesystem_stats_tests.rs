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
    assert_eq!(stats.max_name_len, Some(255));
    assert_eq!(stats.free_bytes, Some(0));
    assert_eq!(stats.total_bytes, Some(12345));
    assert_eq!(stats.available_bytes, None);
    assert_eq!(stats.max_links, None);
    assert_eq!(
        decode_filesystem_stats(bitmap(&[]), &[]).unwrap(),
        FilesystemStats::default()
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
