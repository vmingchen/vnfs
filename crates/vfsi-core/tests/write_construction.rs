use vfsi_core::{VfFile, VfOffset, WriteOp};

#[test]
fn owned_native_constructors_infer_without_expected_types_or_flag_builders() {
    let current = WriteOp::new(VfFile::from_fd(7), VfOffset::Cur, vec![1, 2]);
    assert_eq!(current.offset(), VfOffset::Cur);
    assert_eq!(current.data(), &[1, 2]);

    let positional = WriteOp::at(VfFile::from_fd(7), 11, vec![3, 4]);
    assert_eq!(positional.offset(), VfOffset::At(11));
    assert_eq!(positional.data(), &[3, 4]);
    assert_eq!(
        positional.borrowed().data().as_ptr(),
        positional.data().as_ptr()
    );
}

#[test]
fn borrowed_native_constructors_infer_without_a_backend_call() {
    let file = VfFile::from_fd(7);
    let payload = &b"payload"[..];
    let current = WriteOp::new(&file, VfOffset::End, payload);
    assert!(std::ptr::eq(current.file(), &file));
    assert_eq!(current.offset(), VfOffset::End);
    assert_eq!(current.data().as_ptr(), payload.as_ptr());

    let positional = WriteOp::at(&file, 13, payload);
    assert!(std::ptr::eq(positional.file(), &file));
    assert_eq!(positional.offset(), VfOffset::At(13));
    assert_eq!(positional.data().as_ptr(), payload.as_ptr());
}

#[test]
fn portable_constructors_keep_absolute_offsets_and_copyable_borrowed_storage() {
    struct NonClone;
    let file = NonClone;
    let payload = &b"payload"[..];
    let op = vfsi_core::api::WriteOp::at(&file, 17, payload);
    let copy = op;
    assert!(std::ptr::eq(op.file(), copy.file()));
    assert_eq!(copy.offset(), 17);
    assert_eq!(copy.data().as_ptr(), payload.as_ptr());
}
