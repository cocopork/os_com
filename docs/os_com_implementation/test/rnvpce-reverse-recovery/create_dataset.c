// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define MIB (1024ULL * 1024ULL)
#define PAGE_SIZE_BYTES 4096ULL
#define FILES_PER_DIR 1024ULL

static unsigned char zeroes[MIB];

static void die(const char *operation, const char *path) {
    fprintf(stderr, "%s %s: %s\n", operation, path, strerror(errno));
    exit(EXIT_FAILURE);
}

static void mkdir_if_needed(const char *path) {
    if (mkdir(path, 0755) != 0 && errno != EEXIST) {
        die("mkdir", path);
    }
}

static void create_file(const char *path, uint64_t bytes) {
    int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (fd < 0) {
        die("open", path);
    }
    while (bytes != 0) {
        size_t chunk = bytes < sizeof(zeroes) ? (size_t)bytes : sizeof(zeroes);
        ssize_t written = write(fd, zeroes, chunk);
        if (written < 0) {
            die("write", path);
        }
        if (written == 0) {
            fprintf(stderr, "short write to %s\n", path);
            exit(EXIT_FAILURE);
        }
        bytes -= (uint64_t)written;
    }
    if (close(fd) != 0) {
        die("close", path);
    }
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: %s ROOT TARGET_FILES DATA_MIB\n", argv[0]);
        return EXIT_FAILURE;
    }

    const char *root = argv[1];
    uint64_t target = strtoull(argv[2], NULL, 10);
    uint64_t data_mib = strtoull(argv[3], NULL, 10);
    uint64_t bytes_per_file;
    char directory[4096];
    char path[4096];

    if (target == 0 || data_mib == 0 || (data_mib * MIB) % target != 0) {
        fprintf(stderr, "DATA_MIB must divide evenly across TARGET_FILES\n");
        return EXIT_FAILURE;
    }
    bytes_per_file = data_mib * MIB / target;
    if (bytes_per_file < PAGE_SIZE_BYTES || bytes_per_file % PAGE_SIZE_BYTES != 0) {
        fprintf(stderr, "each file must contain an integral number of 4 KiB pages\n");
        return EXIT_FAILURE;
    }

    mkdir_if_needed(root);
    for (uint64_t index = 0; index < target; ++index) {
        if (target == 1) {
            if (snprintf(path, sizeof(path), "%s/f.0", root) >= (int)sizeof(path)) {
                return EXIT_FAILURE;
            }
        } else {
            uint64_t directory_index = index / FILES_PER_DIR;
            if (snprintf(directory, sizeof(directory), "%s/d.%llu", root,
                         (unsigned long long)directory_index) >= (int)sizeof(directory)) {
                return EXIT_FAILURE;
            }
            if (index % FILES_PER_DIR == 0) {
                mkdir_if_needed(directory);
            }
            if (snprintf(path, sizeof(path), "%s/f.%llu", directory,
                         (unsigned long long)(index % FILES_PER_DIR)) >= (int)sizeof(path)) {
                return EXIT_FAILURE;
            }
        }
        create_file(path, bytes_per_file);
        if ((index + 1) % 65536 == 0) {
            fprintf(stderr, "prepared_files=%llu\n", (unsigned long long)(index + 1));
        }
    }

    fprintf(stderr, "prepared_files=%llu data_bytes=%llu\n",
            (unsigned long long)target,
            (unsigned long long)(target * bytes_per_file));
    return EXIT_SUCCESS;
}
