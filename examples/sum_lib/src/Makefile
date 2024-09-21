BIN = sum_server sum_client
LIB = libsumlib.a
GEN = sum_clnt.c sum_svc.c sum_xdr.c sum.h
RPCCOM = rpcgen
CFLAGS += -I/usr/include/tirpc
# Note that we use tirpc instead of ntirpc because ntirpc
# lack some functionality on the server side.
LDFLAGS += -ltirpc

all: $(BIN) $(LIB)

sum_xdr.o: sum_xdr.c sum.h
	$(CC) -c $(CFLAGS) -o $@ sum_xdr.c

sum_clnt.o: sum_clnt.c sum.h
	$(CC) -c $(CFLAGS) -o $@ sum_clnt.c

libsumlib.a: sum_xdr.o sum_clnt.o
	$(AR) $(ARFLAGS) $@ $^

sum_server: sum_svc.c sum_xdr.o
	$(CC) $(CFLAGS) -o $@ sum_svc.c sum_xdr.o $(LDFLAGS)

sum_client: sum_clnt.c sum_client.o sum_xdr.o
	$(CC) $(CFLAGS) -o $@ sum_client.c sum_clnt.c sum_xdr.o $(LDFLAGS)

sum_client.o: sum_client.c sum.h
	$(CC) -c $(CFLAGS) -g -o $@ sum_client.c

# only run if the protocol changes
# $(GEN): sum.x
#	$(RPCCOM) sum.x

.PHONY: clean
clean:
	rm -f *.o $(BIN) $(LIB)

