/*
 * bindgen input header for the nfsv41-sys crate. In addition to the
 * nfsv41.h types, it declares the extern wrappers implemented in
 * wrapper.c so that bindgen emits Rust declarations for them.
 */
#ifndef NFSV41_WRAPPER_H
#define NFSV41_WRAPPER_H

#include "ganesha_rpc.h"
#include "nfsv41.h"

bool xdr_wrap_COMPOUND4args(XDR *, COMPOUND4args *);
bool xdr_wrap_COMPOUND4res(XDR *, COMPOUND4res *);
bool xdr_wrap_nfs_argop4(XDR *, nfs_argop4 *);
bool xdr_wrap_nfs_resop4(XDR *, nfs_resop4 *);
bool xdr_wrap_nfs_fh4(XDR *, nfs_fh4 *);

#endif /* !NFSV41_WRAPPER_H */
