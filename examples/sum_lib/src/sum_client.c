#include "sum.h"
#include <rpc/rpc.h>
#include <stdlib.h>

void
sumprog_1( char* host, int argc, char *argv[])
{
   CLIENT *clnt;
   int  *result_1;
   int i;
   sum_args args;

   args.a = atoi(argv[2]);
   args.b = atoi(argv[3]);

   clnt = clnt_create(host, SUMPROG, SUMVERS, "udp");
   if (clnt == NULL) {
      clnt_pcreateerror(host);
      exit(1);
   }
   result_1 = sum_1(&args, clnt);
   if (result_1 == NULL) {
      clnt_perror(clnt, "call failed:");
   }
   clnt_destroy( clnt );
   printf("sum = %d\n", *result_1);
}


int main( int argc, char* argv[] )
{
   char *host;

   if(argc != 4) {
     printf("usage: %s server_host a b\n", argv[0]);
     exit(1);
   }
   host = argv[1];
   sumprog_1(host, argc, argv);
   return 0;
}
