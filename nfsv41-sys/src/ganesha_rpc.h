/*
 * Minimal stand-in for the NFS-Ganesha <ganesha_rpc.h> header that
 * nfsv41.h includes. nfsv41.h only needs the libntirpc RPC/XDR types
 * plus a couple of GSS-related type names; it does not need any of the
 * real Ganesha infrastructure, so this shim deliberately pulls in only
 * the individual libntirpc headers (avoiding <rpc/rpc.h>, which would
 * drag in <gssapi/gssapi.h> via <rpc/auth_gss.h>).
 */
#ifndef GANESHA_RPC_H
#define GANESHA_RPC_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "config.h"

#include <rpc/types.h>
#include <rpc/xdr.h>
#include <rpc/xdr_inline.h>
#include <rpc/rpc_msg.h>
#include <rpc/auth_unix.h>

/*
 * Bounded-array limits for the nfsv41.h codecs. The rpcgen-era header
 * that txn-compound vendors passes maxsize = ~0 for every unbounded
 * array; ntirpc's inline xdr_array_decode rejects maxsize with
 * maxsize > UINT_MAX/selem (an overflow guard), so unbounded arrays
 * must be given a finite cap. Values match NFS-Ganesha
 * src/include/gsh_rpc.h.
 */
#define XDR_ARRAY_MAXLEN 1024
/* General-attribute opaque cap (matches NFS-Ganesha gsh_rpc.h). */
#define XDR_BYTES_MAXLEN (1024 * 1024)
/* READ/WRITE payloads use the I/O cap (XDR_BYTES_MAXLEN_IO upstream) instead
 * of the 1 MiB general cap, so a single data op can carry a whole compound's
 * budget. */
#define XDR_BYTES_MAXLEN_IO (64 * 1024 * 1024)
#define XDR_STRING_MAXLEN (8 * 1024)

/* RPCSEC_GSS service type, normally from <rpc/auth_gss.h>. */
typedef enum rpc_gss_svc {
	RPC_GSS_SVC_NONE = 1,
	RPC_GSS_SVC_INTEGRITY = 2,
	RPC_GSS_SVC_PRIVACY = 3
} rpc_gss_svc_t;

/*
 * Only used by the rpcgen-generated freeresult prototypes in nfsv41.h;
 * the struct layout is never referenced, so an opaque forward-declared
 * typedef is sufficient.
 */
typedef struct __rpc_svcxprt SVCXPRT;

/*
 * Only used in the rpcgen-generated cb_* prototypes in nfsv41.h; the
 * struct layout is never referenced, so an opaque forward-declared
 * typedef is sufficient.
 */
typedef struct rpc_client CLIENT;

/*
 * Ganesha's request-lookahead tracking. nfsv41.h's xdr_nfs_argop4 /
 * xdr_nfs_resop4 codecs maintain a nfs_request_lookahead pointed to by
 * XDR.x_public while they walk the arg/res arrays; the fields are never
 * read back in this crate, but they must be defined for the codecs to
 * compile. Values match NFS-Ganesha src/Protocols/nfs4/support/nfsxdr.h.
 */
#define NFS_LOOKAHEAD_NONE 0x0000
#define NFS_LOOKAHEAD_MOUNT 0x0001
#define NFS_LOOKAHEAD_OPEN 0x0002
#define NFS_LOOKAHEAD_CLOSE 0x0004
#define NFS_LOOKAHEAD_READ 0x0008
#define NFS_LOOKAHEAD_WRITE 0x0010
#define NFS_LOOKAHEAD_COMMIT 0x0020
#define NFS_LOOKAHEAD_CREATE 0x0040
#define NFS_LOOKAHEAD_REMOVE 0x0080
#define NFS_LOOKAHEAD_RENAME 0x0100
#define NFS_LOOKAHEAD_LOCK 0x0200
#define NFS_LOOKAHEAD_READDIR 0x0400
#define NFS_LOOKAHEAD_LAYOUTCOMMIT 0x0040
#define NFS_LOOKAHEAD_SETATTR 0x0080
#define NFS_LOOKAHEAD_SETCLIENTID 0x0100
#define NFS_LOOKAHEAD_SETCLIENTID_CONFIRM 0x0200
#define NFS_LOOKAHEAD_LOOKUP 0x0400
#define NFS_LOOKAHEAD_READLINK 0x0800

struct nfs_request_lookahead {
	uint32_t flags;
	uint16_t read;
	uint16_t write;
};

#define NFS_LOOKAHEAD_HIGH_LATENCY(lkhd) \
	(((lkhd).flags & (NFS_LOOKAHEAD_READ | NFS_LOOKAHEAD_WRITE | \
			  NFS_LOOKAHEAD_COMMIT | NFS_LOOKAHEAD_LAYOUTCOMMIT | \
			  NFS_LOOKAHEAD_READDIR)))

#endif /* !GANESHA_RPC_H */
