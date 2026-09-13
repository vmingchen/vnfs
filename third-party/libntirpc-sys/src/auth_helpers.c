#include <rpc/auth.h>

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
 * fields needed to reach call_replies and its lock.
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

static enum xprt_stat vfsi_decode_with_reply_verifier(struct svc_req *request)
{
    SVCXPRT *xprt = request->rq_xprt;
    struct vfsi_patched_ops *patched =
        (struct vfsi_patched_ops *)xprt->xp_ops;
    struct vfsi_rpc_dplx_prefix *record =
        (struct vfsi_rpc_dplx_prefix *)xprt;
    struct rpc_msg decoded_message;
    u_int position = XDR_GETPOS(request->rq_xdrs);

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
    return patched->original_decode(request);
}

bool vfsi_libntirpc_install_reply_verifier_fix(CLIENT *client)
{
    SVCXPRT *xprt = clnt_vc_get_client_xprt(client);
    struct vfsi_patched_ops *patched;

    if (xprt == NULL || xprt->xp_ops == NULL || xprt->xp_ops->xp_decode == NULL)
        return false;
    if (xprt->xp_ops->xp_decode == vfsi_decode_with_reply_verifier)
        return true;

    patched = malloc(sizeof(*patched));
    if (patched == NULL)
        return false;
    patched->ops = *xprt->xp_ops;
    patched->original_ops = xprt->xp_ops;
    patched->original_decode = xprt->xp_ops->xp_decode;
    patched->ops.xp_decode = vfsi_decode_with_reply_verifier;
    xprt->xp_ops = &patched->ops;
    return true;
}

void vfsi_libntirpc_uninstall_reply_verifier_fix(CLIENT *client)
{
    SVCXPRT *xprt = clnt_vc_get_client_xprt(client);
    struct vfsi_patched_ops *patched;

    if (xprt == NULL || xprt->xp_ops == NULL ||
        xprt->xp_ops->xp_decode != vfsi_decode_with_reply_verifier)
        return;
    patched = (struct vfsi_patched_ops *)xprt->xp_ops;
    xprt->xp_ops = patched->original_ops;
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
    original_malloc = parameters.malloc_;
    parameters.malloc_ = vfsi_zeroing_malloc;
    (void)tirpc_control(TIRPC_PUT_PARAMETERS, &parameters);
}

#endif /* VFSI_RPCSEC_GSS */

void vfsi_libntirpc_auth_destroy(AUTH *auth)
{
    auth_destroy(auth);
}

#ifdef VFSI_RPCSEC_GSS
AUTH *vfsi_libntirpc_authgss_ncreate_default(CLIENT *client, char *service,
                                              struct rpc_gss_sec *security)
{
    AUTH *auth;
    bool previous;

    (void)pthread_once(&zeroing_allocator_once, install_zeroing_allocator);
    previous = zero_ntirpc_allocations;
    zero_ntirpc_allocations = true;
    auth = authgss_ncreate_default(client, service, security);
    zero_ntirpc_allocations = previous;
    return auth;
}
#endif /* VFSI_RPCSEC_GSS */
