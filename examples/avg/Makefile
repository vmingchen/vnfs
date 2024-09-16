BIN = avg_client avg_server
GEN = avg_clnt.c avg_svc.c avg_xdr.c avg.h
RPCCOM = rpcgen
CFLAGS += -I/usr/include/tirpc
LDFLAGS += -ltirpc

all: $(BIN)

avg_client: avg_clnt.c avg_client.o avg_xdr.o
	$(CC) $(CFLAGS) -o $@ avg_client.c avg_clnt.c avg_xdr.o $(LDFLAGS)

avg_client.o: avg_client.c avg.h
	$(CC) -c $(CFLAGS) -g -o $@ avg_client.c

avg_server: avg_server.o avg_svc.c avg_xdr.o
	$(CC) $(CFLAGS) -o $@ avg_server.o avg_svc.c avg_xdr.o $(LDFLAGS)

avg_server.o: avg_server.c avg.h
	$(CC) -c $(CFLAGS) -g -o $@ avg_server.c

$(GEN): avg.x
	$(RPCCOM) avg.x

.PHONY: clean
clean:
	rm -f $(GEN) *.o $(BIN)

