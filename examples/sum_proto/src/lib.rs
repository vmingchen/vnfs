#[allow(non_camel_case_types)]
use xdr_codec;

include!(concat!(env!("OUT_DIR"), "/sum_xdr.rs"));

pub const SUM_PROGRAM : std::os::raw::c_uint = 22888;
pub const SUM_VERSION : std::os::raw::c_uint = 1;

#[cfg(test)]
mod tests {
    use xdr_codec::{Pack, Unpack};

    use super::sum_args;

    #[test]
    fn test_xdr() {
        let args = sum_args {
            a: 1,
            b: 2,
        };
        let mut bytes = Vec::<u8>::new();
        let wsize = args.pack(&mut bytes).expect("pack");
        let mut input = bytes.as_slice();
        let (args_recovered, rsize) = sum_args::unpack(&mut input).expect("unpack");
        assert_eq!(rsize, wsize);
        assert_eq!(args, args_recovered);
    }
}