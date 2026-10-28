use super::*;

#[test]
fn native_unsupported_errors_keep_their_diagnostic_at_the_c_boundary() {
    for error in [VfError::nfs(2, 10019), VfError::smb(2, 0xc000_0257)] {
        let result = vfsi_result::from_error(error.with_context("vread", "/referral"));
        assert_eq!(result.category, VFSI_ERROR_UNSUPPORTED);
        let bytes: Vec<u8> = result
            .message
            .iter()
            .map(|c| c.to_ne_bytes()[0])
            .take_while(|c| *c != 0)
            .collect();
        let message = String::from_utf8(bytes).unwrap();
        assert!(message.contains("unsupported"), "{message}");
        assert!(message.contains("/referral"), "{message}");
    }
}
