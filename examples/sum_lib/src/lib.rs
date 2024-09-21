#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
pub type rpcblist = rp__list;

#[cfg(test)]
mod tests {
    use super::xdr_void;

    #[test]
    fn test_basic() {
        assert_eq!(unsafe { xdr_void() }, 1);
    }
}
