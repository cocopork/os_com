# SPDX-License-Identifier: MPL-2.0

CC ?= cc
CFLAGS ?= -O2
CFLAGS += -std=c11 -D_GNU_SOURCE -Wall -Wextra -Werror -pthread
LDFLAGS ?= -static

.PHONY: all clean test

all: mixed_sync_bench

mixed_sync_bench: mixed_sync_bench.c
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<

mixed_sync_bench_fault_create: mixed_sync_bench.c
	$(CC) $(CFLAGS) $(LDFLAGS) -DMIXED_SYNC_TEST_FAIL_CREATE_SECOND -o $@ $<

mixed_sync_bench_slow_io: mixed_sync_bench.c
	$(CC) $(CFLAGS) $(LDFLAGS) -DMIXED_SYNC_TEST_IO_DELAY_US=100000 -o $@ $<

test: mixed_sync_bench
	python3 test_mixed_sync_bench.py

clean:
	rm -f mixed_sync_bench mixed_sync_bench_fault_create mixed_sync_bench_slow_io
