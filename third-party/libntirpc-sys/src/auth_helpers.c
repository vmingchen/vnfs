#include <stddef.h>

#include <rpc/auth.h>
#include <rpc/svc.h>

#ifdef VFSI_RPCSEC_GSS
#include <rpc/auth_gss.h>
#include <rpc/pool_queue.h>
#include <rpc/rpc.h>
#include <rpc/rpc_msg.h>
#include <rpc/xdr_ioq.h>
#include <misc/rbtree.h>
#include <misc/wait_queue.h>

#include <pthread.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/*
 * libntirpc decodes a reply verifier into svc_req.rq_msg, but its client
 * reply path validates clnt_req.cc_verf without copying the decoded value.
 * This affects both context establishment and every established RPCSEC_GSS
 * call.  Preserve the verifier on the decode worker and substitute those
 * exact wire bytes when libntirpc asks GSS to validate its empty copy.
 *
 * Install a per-transport decoder shim that first decodes just the RPC header,
 * copies the exact wire verifier to the matching request, rewinds the XDR
 * stream, and delegates to libntirpc's normal decoder.  GSS therefore still
 * authenticates the bytes supplied by the server; no verifier is accepted or
 * synthesized by this workaround.
 *
 * rpc_dplx_rec is private to libntirpc, but its prefix has been ABI-stable
 * throughout the supported releases.  Keep the definition limited to the
 * fields needed to reach call_replies and its lock.  The optional
 * rdma_call_expires member is selected by build.rs from the linked library's
 * exported symbols (not from the version string, which does not distinguish
 * the two layouts).  See build.rs for the exact detection.
 */
struct vfsi_rpc_dplx_lock {
    struct waitq_entry wait;
    struct {
        const char *function;
        int line;
    } trace;
};

struct vfsi_rpc_dplx_prefix {
    SVCXPRT xprt;
    struct xdr_ioq ioq;
    struct poolq_head writeq;
    struct opr_rbtree call_replies;
#ifdef VFSI_LIBNTIRPC_HAS_RDMA_EXPIRES
    struct opr_rbtree rdma_call_expires;
#endif
    struct opr_rbtree_node fd_node;
    struct {
        struct vfsi_rpc_dplx_lock lock;
        struct timespec timestamp;
    } recv;
};

struct vfsi_patched_ops {
    struct xp_ops ops;
    struct xp_ops *original_ops;
    svc_req_fun_t original_decode;
};

/*
 * Serializes transport operation patching against in-flight decodes: decodes
 * hold the read lock and install/uninstall hold the write lock, so a patch is
 * only freed once no worker can still reach it.
 */
static pthread_rwlock_t vfsi_ops_lock = PTHREAD_RWLOCK_INITIALIZER;

static SVCXPRT *vfsi_client_xprt(CLIENT *client)
{
    return clnt_vc_get_client_xprt(client);
}

static enum xprt_stat vfsi_decode_with_reply_verifier(struct svc_req *request)
{
    SVCXPRT *xprt = request->rq_xprt;
    struct vfsi_patched_ops *patched;
    struct vfsi_rpc_dplx_prefix *record =
        (struct vfsi_rpc_dplx_prefix *)xprt;
    struct rpc_msg decoded_message;
    u_int position;
    enum xprt_stat status;

    pthread_rwlock_rdlock(&vfsi_ops_lock);
    patched = (struct vfsi_patched_ops *)xprt->xp_ops;
    position = XDR_GETPOS(request->rq_xdrs);

    memset(&decoded_message, 0, sizeof(decoded_message));
    rpc_msg_init(&decoded_message);
    request->rq_xdrs->x_op = XDR_DECODE;
    if (xdr_dplx_decode(request->rq_xdrs, &decoded_message) &&
        decoded_message.rm_direction == REPLY &&
        decoded_message.rm_reply.rp_stat == MSG_ACCEPTED) {
        struct clnt_req key;
        struct opr_rbtree_node *node;

        memset(&key, 0, sizeof(key));
        key.cc_xid = decoded_message.rm_xid;
        /* The same lock guards removal from call_replies, so the matching
         * clnt_req cannot be released while we copy the verifier into it. */
        mutex_lock(&record->recv.lock.wait.mtx);
        node = opr_rbtree_lookup(&record->call_replies, &key.cc_dplx);
        if (node != NULL) {
            struct clnt_req *client_request =
                opr_containerof(node, struct clnt_req, cc_dplx);
            client_request->cc_verf = decoded_message.RPCM_ack.ar_verf;
        }
        mutex_unlock(&record->recv.lock.wait.mtx);
    }
    (void)XDR_SETPOS(request->rq_xdrs, position);
    status = patched->original_decode(request);
    pthread_rwlock_unlock(&vfsi_ops_lock);
    return status;
}

bool vfsi_libntirpc_install_reply_verifier_fix(CLIENT *client)
{
    SVCXPRT *xprt = vfsi_client_xprt(client);
    struct vfsi_patched_ops *patched;

    if (xprt == NULL)
        return false;
    pthread_rwlock_wrlock(&vfsi_ops_lock);
    if (xprt->xp_ops == NULL || xprt->xp_ops->xp_decode == NULL) {
        pthread_rwlock_unlock(&vfsi_ops_lock);
        return false;
    }
    if (xprt->xp_ops->xp_decode == vfsi_decode_with_reply_verifier) {
        pthread_rwlock_unlock(&vfsi_ops_lock);
        return true;
    }

    patched = malloc(sizeof(*patched));
    if (patched == NULL) {
        pthread_rwlock_unlock(&vfsi_ops_lock);
        return false;
    }
    patched->ops = *xprt->xp_ops;
    patched->original_ops = xprt->xp_ops;
    patched->original_decode = xprt->xp_ops->xp_decode;
    patched->ops.xp_decode = vfsi_decode_with_reply_verifier;
    xprt->xp_ops = &patched->ops;
    pthread_rwlock_unlock(&vfsi_ops_lock);
    return true;
}

void vfsi_libntirpc_uninstall_reply_verifier_fix(CLIENT *client)
{
    SVCXPRT *xprt = vfsi_client_xprt(client);
    struct vfsi_patched_ops *patched;

    if (xprt == NULL)
        return;
    pthread_rwlock_wrlock(&vfsi_ops_lock);
    if (xprt->xp_ops == NULL ||
        xprt->xp_ops->xp_decode != vfsi_decode_with_reply_verifier) {
        pthread_rwlock_unlock(&vfsi_ops_lock);
        return;
    }
    patched = (struct vfsi_patched_ops *)xprt->xp_ops;
    xprt->xp_ops = patched->original_ops;
    /* Safe to free after releasing the write lock: the transport no longer
     * points at the patch, and no decode could have entered while the write
     * lock was held. */
    pthread_rwlock_unlock(&vfsi_ops_lock);
    free(patched);
}

/*
 * libntirpc 6.3's authgss_ncreate() allocates rpc_gss_data with mem_alloc()
 * but does not initialize gc_ctx/gc_seq before encoding the INIT credential.
 * Make allocations performed by authgss_ncreate_default() zeroed while
 * preserving libntirpc's configured allocator and its matching free hook.
 *
 * The hook remains installed because libntirpc's allocator table is global.
 * Its thread-local guard means unrelated RPC allocations still use the
 * original allocator without an extra memset, including concurrent calls.
 */
static pthread_once_t zeroing_allocator_once = PTHREAD_ONCE_INIT;
static _Thread_local bool zero_ntirpc_allocations;
static bool zeroing_allocator_installed;
static void *(*original_malloc)(size_t, const char *, int, const char *);

static void *vfsi_zeroing_malloc(size_t size, const char *file, int line,
                                 const char *function)
{
    void *allocation = original_malloc(size, file, line, function);

    if (zero_ntirpc_allocations && allocation != NULL)
        memset(allocation, 0, size);
    return allocation;
}

static void install_zeroing_allocator(void)
{
    tirpc_pkg_params parameters;

    if (!tirpc_control(TIRPC_GET_PARAMETERS, &parameters))
        return;
    if (parameters.malloc_ == NULL)
        return;
    original_malloc = parameters.malloc_;
    parameters.malloc_ = vfsi_zeroing_malloc;
    if (!tirpc_control(TIRPC_PUT_PARAMETERS, &parameters))
        return;
    zeroing_allocator_installed = true;
}

#endif /* VFSI_RPCSEC_GSS */

void vfsi_libntirpc_auth_destroy(AUTH *auth)
{
    auth_destroy(auth);
}

void vfsi_libntirpc_set_process_cb(SVCXPRT *xprt, svc_req_fun_t callback)
{
    xprt->xp_dispatch.process_cb = callback;
}

/*
 * Returns `sizeof(SVCXPRT)` as the C shims (and therefore the linked library)
 * see it. The Rust bindings assert this equals `size_of::<SVCXPRT>()` so a
 * build-flag divergence (notably `INET6`) fails loudly instead of corrupting
 * the private rpc_dplx_rec offsets.
 */
size_t vfsi_libntirpc_sizeof_svcxprt(void)
{
    return sizeof(SVCXPRT);
}

#ifdef VFSI_RPCSEC_GSS
AUTH *vfsi_libntirpc_authgss_ncreate_default(CLIENT *client, char *service,
                                              struct rpc_gss_sec *security)
{
    AUTH *auth;
    bool previous;

    (void)pthread_once(&zeroing_allocator_once, install_zeroing_allocator);
    if (!zeroing_allocator_installed) {
        /* Fail closed: without the zeroing allocator, authgss_ncreate_default
         * can encode an uninitialized gc_ctx/gc_seq. The caller reports the
         * NULL return as a credential-creation failure. */
        fprintf(stderr,
                "libntirpc-sys: refusing to create RPCSEC_GSS credentials "
                "because the libntirpc zeroing allocator could not be "
                "installed\n");
        return NULL;
    }
    previous = zero_ntirpc_allocations;
    zero_ntirpc_allocations = true;
    auth = authgss_ncreate_default(client, service, security);
    zero_ntirpc_allocations = previous;
    return auth;
}
#endif /* VFSI_RPCSEC_GSS */
