/*
 * Extern "C" wrappers around the static inline XDR codecs defined in
 * nfsv41.h. Rust cannot call static inline functions directly, so we
 * compile this translation unit (with libntirpc's headers) into a small
 * static archive that exposes a handful of entry points.
 */
#include "ganesha_rpc.h"
#include "nfsv41.h"

bool
xdr_wrap_COMPOUND4args(XDR *xdrs, COMPOUND4args *objp)
{
	return (xdr_COMPOUND4args(xdrs, objp));
}

bool
xdr_wrap_COMPOUND4res(XDR *xdrs, COMPOUND4res *objp)
{
	return (xdr_COMPOUND4res(xdrs, objp));
}

bool
xdr_wrap_nfs_argop4(XDR *xdrs, nfs_argop4 *objp)
{
	return (xdr_nfs_argop4(xdrs, objp));
}

bool
xdr_wrap_nfs_resop4(XDR *xdrs, nfs_resop4 *objp)
{
	return (xdr_nfs_resop4(xdrs, objp));
}

bool
xdr_wrap_nfs_fh4(XDR *xdrs, nfs_fh4 *objp)
{
	return (xdr_nfs_fh4(xdrs, objp));
}
