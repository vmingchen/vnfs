/*
 * Please do not edit this file.
 * It was generated using rpcgen.
 */

#ifndef _SUM_H_RPCGEN
#define _SUM_H_RPCGEN

#include <rpc/rpc.h>


#ifdef __cplusplus
extern "C" {
#endif


struct sum_args {
	int a;
	int b;
};
typedef struct sum_args sum_args;

#define SUMPROG 22888
#define SUMVERS 1

#if defined(__STDC__) || defined(__cplusplus)
#define sum 1
extern  int * sum_1(sum_args *, CLIENT *);
extern  int sum_1_svc(sum_args *, struct svc_req *);
extern int sumprog_1_freeresult (SVCXPRT *, xdrproc_t, caddr_t);

#else /* K&R C */
#define sum 1
extern  int * sum_1();
extern  int * sum_1_svc();
extern int sumprog_1_freeresult ();
#endif /* K&R C */

/* the xdr functions */

#if defined(__STDC__) || defined(__cplusplus)
extern  bool_t xdr_sum_args (XDR *, sum_args*);

#else /* K&R C */
extern bool_t xdr_sum_args ();

#endif /* K&R C */

#ifdef __cplusplus
}
#endif

#endif /* !_SUM_H_RPCGEN */
