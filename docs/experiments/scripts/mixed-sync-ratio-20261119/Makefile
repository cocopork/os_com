# SPDX-License-Identifier: MPL-2.0

CC ?= cc
CFLAGS ?= -O2
CFLAGS += -std=c11 -D_GNU_SOURCE -Wall -Wextra -Werror -pthread

.PHONY: all clean test

all: mixed_sync_bench

mixed_sync_bench: mixed_sync_bench.c
	$(CC) $(CFLAGS) -o $@ $<

test: mixed_sync_bench
	python3 test_mixed_sync_bench.py

clean:
	rm -f mixed_sync_bench
