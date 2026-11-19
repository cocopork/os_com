// SPDX-License-Identifier: MPL-2.0

#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define BLOCK_SIZE 4096ULL
#define HISTOGRAM_EXACT_BINS 1024U
#define HISTOGRAM_SUB_BINS 16U
#define HISTOGRAM_EXPONENTS 54U
#define HISTOGRAM_BINS (HISTOGRAM_EXACT_BINS + HISTOGRAM_SUB_BINS * HISTOGRAM_EXPONENTS)
#define FNV_OFFSET UINT64_C(1469598103934665603)
#define FNV_PRIME UINT64_C(1099511628211)

enum decision_kind {
	DECIDE_OPERATION = 1,
	DECIDE_PAGE = 2,
	DECIDE_SYNC = 3,
};

struct config {
	const char *directory;
	unsigned int threads;
	uint64_t file_bytes;
	unsigned int read_percent;
	unsigned int sync_percent;
	uint64_t seed;
	unsigned int warmup_seconds;
	unsigned int runtime_seconds;
	bool self_test;
};

struct stats {
	uint64_t operations;
	uint64_t reads;
	uint64_t writes;
	uint64_t fsyncs;
	uint64_t read_bytes;
	uint64_t write_bytes;
	uint64_t latency_sum_ns;
	uint64_t latency_overflow_count;
	uint64_t *latency_histogram;
};

struct shared_state {
	struct config config;
	pthread_barrier_t phase_barrier;
	struct timespec warmup_end;
	struct timespec measurement_start;
	struct timespec measurement_end;
	atomic_bool failed;
	pthread_mutex_t startup_mutex;
	pthread_cond_t startup_cond;
	unsigned int started_threads;
	enum { STARTUP_WAIT, STARTUP_RUN, STARTUP_ABORT } startup_state;
};

struct worker {
	struct shared_state *shared;
	unsigned int thread_id;
	int fd;
	struct stats stats;
};

static uint64_t splitmix64(uint64_t value)
{
	value += UINT64_C(0x9e3779b97f4a7c15);
	value = (value ^ (value >> 30)) * UINT64_C(0xbf58476d1ce4e5b9);
	value = (value ^ (value >> 27)) * UINT64_C(0x94d049bb133111eb);
	return value ^ (value >> 31);
}

static uint64_t decision(const struct config *config, unsigned int thread_id,
			 uint64_t operation_index, enum decision_kind kind)
{
	uint64_t value = config->seed;

	value ^= (uint64_t)(thread_id + 1) * UINT64_C(0x9e3779b97f4a7c15);
	value ^= (operation_index + 1) * UINT64_C(0xbf58476d1ce4e5b9);
	value ^= (uint64_t)kind * UINT64_C(0x94d049bb133111eb);
	return splitmix64(value);
}

static struct timespec monotonic_now(void)
{
	struct timespec now;

	if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
		perror("clock_gettime");
		exit(EXIT_FAILURE);
	}
	return now;
}

static struct timespec add_seconds(struct timespec value, unsigned int seconds)
{
	value.tv_sec += (time_t)seconds;
	return value;
}

static bool before(struct timespec left, struct timespec right)
{
	return left.tv_sec < right.tv_sec ||
	       (left.tv_sec == right.tv_sec && left.tv_nsec < right.tv_nsec);
}

static uint64_t elapsed_ns(struct timespec start, struct timespec end)
{
	uint64_t seconds = (uint64_t)(end.tv_sec - start.tv_sec);
	int64_t nanoseconds = end.tv_nsec - start.tv_nsec;

	if (nanoseconds < 0) {
		seconds--;
		nanoseconds += 1000000000L;
	}
	return seconds * UINT64_C(1000000000) + (uint64_t)nanoseconds;
}

static void report_worker_error(struct shared_state *shared, unsigned int thread_id,
				const char *operation)
{
	if (!atomic_exchange_explicit(&shared->failed, true, memory_order_relaxed))
		fprintf(stderr, "thread %u: %s failed: %s\n", thread_id, operation,
			strerror(errno));
}

static bool perform_operation(struct worker *worker, uint64_t operation_index,
			      void *buffer, bool measured)
{
	struct shared_state *shared = worker->shared;
	const struct config *config = &shared->config;
	uint64_t page_count = config->file_bytes / BLOCK_SIZE;
	bool is_read = decision(config, worker->thread_id, operation_index,
				DECIDE_OPERATION) % 100 < config->read_percent;
	off_t offset = (off_t)((decision(config, worker->thread_id, operation_index,
					  DECIDE_PAGE) % page_count) * BLOCK_SIZE);
	struct timespec started = { 0 };
	ssize_t length;
	bool did_fsync = false;

	if (measured)
		started = monotonic_now();
#ifdef MIXED_SYNC_TEST_IO_DELAY_US
	usleep(MIXED_SYNC_TEST_IO_DELAY_US);
#endif
	if (is_read) {
		length = pread(worker->fd, buffer, BLOCK_SIZE, offset);
		if (length != (ssize_t)BLOCK_SIZE) {
			if (length >= 0)
				errno = EIO;
			report_worker_error(shared, worker->thread_id, "pread");
			return false;
		}
	} else {
		length = pwrite(worker->fd, buffer, BLOCK_SIZE, offset);
		if (length != (ssize_t)BLOCK_SIZE) {
			if (length >= 0)
				errno = EIO;
			report_worker_error(shared, worker->thread_id, "pwrite");
			return false;
		}
		if (decision(config, worker->thread_id, operation_index, DECIDE_SYNC) % 100 <
		    config->sync_percent) {
			if (fsync(worker->fd) != 0) {
				report_worker_error(shared, worker->thread_id, "fsync");
				return false;
			}
			did_fsync = true;
		}
	}

	if (measured) {
		uint64_t latency = elapsed_ns(started, monotonic_now());
		uint64_t latency_us = latency / 1000;
		unsigned int bin;
		struct stats *stats = &worker->stats;

		if (latency_us < HISTOGRAM_EXACT_BINS) {
			bin = (unsigned int)latency_us;
		} else {
			unsigned int exponent = 63U - (unsigned int)__builtin_clzll(latency_us);
			uint64_t base = UINT64_C(1) << exponent;
			unsigned int sub = (unsigned int)((latency_us - base) /
				(base / HISTOGRAM_SUB_BINS));

			bin = HISTOGRAM_EXACT_BINS +
				(exponent - 10U) * HISTOGRAM_SUB_BINS + sub;
			if (bin >= HISTOGRAM_BINS) {
				bin = HISTOGRAM_BINS - 1;
				stats->latency_overflow_count++;
			}
		}

		stats->operations++;
		stats->latency_sum_ns += latency;
		stats->latency_histogram[bin]++;
		if (is_read) {
			stats->reads++;
			stats->read_bytes += BLOCK_SIZE;
		} else {
			stats->writes++;
			stats->write_bytes += BLOCK_SIZE;
			stats->fsyncs += did_fsync;
		}
	}
	return true;
}

static void barrier_wait(pthread_barrier_t *barrier)
{
	int error = pthread_barrier_wait(barrier);

	if (error != 0 && error != PTHREAD_BARRIER_SERIAL_THREAD) {
		errno = error;
		perror("pthread_barrier_wait");
		exit(EXIT_FAILURE);
	}
}

static void *worker_main(void *argument)
{
	struct worker *worker = argument;
	struct shared_state *shared = worker->shared;
	void *buffer = NULL;
	int barrier_result;
	uint64_t operation_index = 0;

	if (posix_memalign(&buffer, BLOCK_SIZE, BLOCK_SIZE) != 0) {
		errno = ENOMEM;
		report_worker_error(shared, worker->thread_id, "posix_memalign");
	}
	if (buffer != NULL)
		memset(buffer, (int)(worker->thread_id + 1), BLOCK_SIZE);

	pthread_mutex_lock(&shared->startup_mutex);
	shared->started_threads++;
	pthread_cond_broadcast(&shared->startup_cond);
	while (shared->startup_state == STARTUP_WAIT)
		pthread_cond_wait(&shared->startup_cond, &shared->startup_mutex);
	if (shared->startup_state == STARTUP_ABORT) {
		pthread_mutex_unlock(&shared->startup_mutex);
		free(buffer);
		return NULL;
	}
	pthread_mutex_unlock(&shared->startup_mutex);

	barrier_result = pthread_barrier_wait(&shared->phase_barrier);
	if (barrier_result == PTHREAD_BARRIER_SERIAL_THREAD)
		shared->warmup_end = add_seconds(monotonic_now(), shared->config.warmup_seconds);
	else if (barrier_result != 0)
		exit(EXIT_FAILURE);
	barrier_wait(&shared->phase_barrier);

	while (buffer != NULL && !atomic_load_explicit(&shared->failed, memory_order_relaxed) &&
	       before(monotonic_now(), shared->warmup_end)) {
		if (!perform_operation(worker, operation_index++, buffer, false))
			break;
	}

	barrier_result = pthread_barrier_wait(&shared->phase_barrier);
	if (barrier_result == PTHREAD_BARRIER_SERIAL_THREAD) {
		shared->measurement_start = monotonic_now();
		shared->measurement_end =
			add_seconds(shared->measurement_start, shared->config.runtime_seconds);
	} else if (barrier_result != 0) {
		exit(EXIT_FAILURE);
	}
	barrier_wait(&shared->phase_barrier);

	operation_index = 0;
	while (buffer != NULL && !atomic_load_explicit(&shared->failed, memory_order_relaxed) &&
	       before(monotonic_now(), shared->measurement_end)) {
		if (!perform_operation(worker, operation_index++, buffer, true))
			break;
	}
	free(buffer);
	return NULL;
}

static uint64_t histogram_quantile(const uint64_t *histogram, uint64_t count,
				   unsigned int numerator, unsigned int denominator)
{
	uint64_t target = (count * numerator + denominator - 1) / denominator;
	uint64_t cumulative = 0;
	unsigned int bin;

	if (count == 0)
		return 0;
	for (bin = 0; bin < HISTOGRAM_BINS; bin++) {
		cumulative += histogram[bin];
		if (cumulative >= target) {
			unsigned int offset, exponent, sub;
			uint64_t base;

			if (bin < HISTOGRAM_EXACT_BINS)
				return bin;
			offset = bin - HISTOGRAM_EXACT_BINS;
			exponent = 10U + offset / HISTOGRAM_SUB_BINS;
			sub = offset % HISTOGRAM_SUB_BINS;
			base = UINT64_C(1) << exponent;
			return base + (base * sub) / HISTOGRAM_SUB_BINS;
		}
	}
	return HISTOGRAM_BINS - 1;
}

static int run_self_test(void)
{
	struct config config = {
		.threads = 1,
		.file_bytes = UINT64_C(320) * 1024 * 1024,
		.read_percent = 30,
		.seed = 20261119,
	};
	uint64_t address_digest = FNV_OFFSET;
	uint64_t operation_digest = FNV_OFFSET;
	uint64_t sync_counts[6] = { 0 };
	uint64_t sync_digests[6] = { FNV_OFFSET, FNV_OFFSET, FNV_OFFSET,
		FNV_OFFSET, FNV_OFFSET, FNV_OFFSET };
	const unsigned int percentages[6] = { 0, 20, 40, 60, 80, 100 };
	uint64_t writes = 0;
	uint64_t index;
	unsigned int threshold;

	for (index = 0; index < 10000; index++) {
		bool is_read = decision(&config, 0, index, DECIDE_OPERATION) % 100 < 30;
		uint64_t page = decision(&config, 0, index, DECIDE_PAGE) %
			(config.file_bytes / BLOCK_SIZE);

		address_digest = (address_digest ^ page) * FNV_PRIME;
		operation_digest = (operation_digest ^ is_read) * FNV_PRIME;
		if (is_read)
			continue;
		writes++;
		for (threshold = 0; threshold < 6; threshold++) {
			if (decision(&config, 0, index, DECIDE_SYNC) % 100 <
			    percentages[threshold]) {
				sync_counts[threshold]++;
				sync_digests[threshold] =
					(sync_digests[threshold] ^ index) * FNV_PRIME;
			}
		}
	}
	printf("{\"status\":\"pass\",\"decision_count\":10000,"
	       "\"write_count\":%" PRIu64 ",\"address_digest\":%" PRIu64
	       ",\"operation_digest\":%" PRIu64 ",\"sync_counts\":{"
	       "\"0\":%" PRIu64 ",\"20\":%" PRIu64 ",\"40\":%" PRIu64
	       ",\"60\":%" PRIu64 ",\"80\":%" PRIu64 ",\"100\":%" PRIu64 "},"
	       "\"sync_digests\":{\"0\":%" PRIu64 ",\"20\":%" PRIu64
	       ",\"40\":%" PRIu64 ",\"60\":%" PRIu64 ",\"80\":%" PRIu64
	       ",\"100\":%" PRIu64 "}}\n",
	       writes, address_digest, operation_digest, sync_counts[0], sync_counts[1],
	       sync_counts[2], sync_counts[3], sync_counts[4], sync_counts[5],
	       sync_digests[0], sync_digests[1], sync_digests[2], sync_digests[3],
	       sync_digests[4], sync_digests[5]);
	return 0;
}

static int create_worker(pthread_t *thread, void *(*start)(void *), void *argument)
{
#ifdef MIXED_SYNC_TEST_FAIL_CREATE_SECOND
	static unsigned int calls;
	if (++calls == 2)
		return EAGAIN;
#endif
	return pthread_create(thread, NULL, start, argument);
}

static int parse_unsigned(const char *name, const char *text, unsigned int *output)
{
	char *end = NULL;
	unsigned long value;

	errno = 0;
	value = strtoul(text, &end, 10);
	if (errno != 0 || end == text || *end != '\0' || value > UINT32_MAX) {
		fprintf(stderr, "invalid %s: %s\n", name, text);
		return -1;
	}
	*output = (unsigned int)value;
	return 0;
}

static int parse_u64(const char *name, const char *text, uint64_t *output)
{
	char *end = NULL;
	unsigned long long value;

	errno = 0;
	value = strtoull(text, &end, 10);
	if (errno != 0 || end == text || *end != '\0') {
		fprintf(stderr, "invalid %s: %s\n", name, text);
		return -1;
	}
	*output = (uint64_t)value;
	return 0;
}

static int parse_options(int argc, char **argv, struct config *config)
{
	static const struct option options[] = {
		{ "directory", required_argument, NULL, 'd' },
		{ "threads", required_argument, NULL, 't' },
		{ "file-bytes", required_argument, NULL, 'b' },
		{ "read-percent", required_argument, NULL, 'r' },
		{ "sync-percent", required_argument, NULL, 's' },
		{ "seed", required_argument, NULL, 'S' },
		{ "warmup-seconds", required_argument, NULL, 'w' },
		{ "runtime-seconds", required_argument, NULL, 'R' },
		{ "self-test", no_argument, NULL, 'T' },
		{ NULL, 0, NULL, 0 },
	};
	int option;

	while ((option = getopt_long(argc, argv, "", options, NULL)) != -1) {
		switch (option) {
		case 'd': config->directory = optarg; break;
		case 't': if (parse_unsigned("threads", optarg, &config->threads)) return -1; break;
		case 'b': if (parse_u64("file-bytes", optarg, &config->file_bytes)) return -1; break;
		case 'r': if (parse_unsigned("read-percent", optarg, &config->read_percent)) return -1; break;
		case 's': if (parse_unsigned("sync-percent", optarg, &config->sync_percent)) return -1; break;
		case 'S': if (parse_u64("seed", optarg, &config->seed)) return -1; break;
		case 'w': if (parse_unsigned("warmup-seconds", optarg, &config->warmup_seconds)) return -1; break;
		case 'R': if (parse_unsigned("runtime-seconds", optarg, &config->runtime_seconds)) return -1; break;
		case 'T': config->self_test = true; break;
		default: return -1;
		}
	}
	if (optind != argc) {
		fprintf(stderr, "unexpected positional arguments\n");
		return -1;
	}
	if (config->self_test)
		return 0;
	if (config->threads == 0 || config->threads > 256) {
		fprintf(stderr, "threads must be in 1..256\n");
		return -1;
	}
	if (config->file_bytes == 0 || config->file_bytes % BLOCK_SIZE != 0) {
		fprintf(stderr, "file-bytes must be a nonzero multiple of 4096\n");
		return -1;
	}
	if (config->read_percent > 100) {
		fprintf(stderr, "read-percent must be in 0..100\n");
		return -1;
	}
	if (config->sync_percent > 100) {
		fprintf(stderr, "sync-percent must be in 0..100\n");
		return -1;
	}
	if (config->runtime_seconds == 0) {
		fprintf(stderr, "runtime-seconds must be nonzero\n");
		return -1;
	}
	return 0;
}

int main(int argc, char **argv)
{
	struct config config = {
		.directory = ".",
		.threads = 8,
		.file_bytes = UINT64_C(320) * 1024 * 1024,
		.read_percent = 0,
		.sync_percent = 0,
		.seed = 20261119,
		.warmup_seconds = 10,
		.runtime_seconds = 30,
	};
	struct shared_state shared;
	struct worker *workers = NULL;
	pthread_t *threads = NULL;
	uint64_t *histogram = NULL;
	uint64_t operations = 0, reads = 0, writes = 0, fsyncs = 0;
	uint64_t read_bytes = 0, write_bytes = 0, latency_sum_ns = 0;
	uint64_t latency_overflow_count = 0;
	unsigned int opened = 0, created = 0, index;
	struct timespec finished;
	char path[4096];
	double elapsed;
	int result = EXIT_FAILURE;

	if (parse_options(argc, argv, &config) != 0)
		return EXIT_FAILURE;
	if (config.self_test)
		return run_self_test();

	workers = calloc(config.threads, sizeof(*workers));
	threads = calloc(config.threads, sizeof(*threads));
	histogram = calloc(HISTOGRAM_BINS, sizeof(*histogram));
	if (workers == NULL || threads == NULL || histogram == NULL) {
		perror("calloc");
		goto cleanup;
	}
	memset(&shared, 0, sizeof(shared));
	shared.config = config;
	atomic_init(&shared.failed, false);
	if (pthread_mutex_init(&shared.startup_mutex, NULL) != 0 ||
	    pthread_cond_init(&shared.startup_cond, NULL) != 0) {
		fprintf(stderr, "startup synchronization initialization failed\n");
		goto cleanup;
	}
	if (pthread_barrier_init(&shared.phase_barrier, NULL, config.threads) != 0) {
		fprintf(stderr, "pthread_barrier_init failed\n");
		goto cleanup;
	}

	for (index = 0; index < config.threads; index++) {
		struct stat status;
		int length = snprintf(path, sizeof(path), "%s/f.%u", config.directory, index);

		if (length < 0 || (size_t)length >= sizeof(path)) {
			fprintf(stderr, "file path is too long for thread %u\n", index);
			goto destroy_barrier;
		}
		workers[index].fd = open(path, O_RDWR);
		if (workers[index].fd < 0) {
			fprintf(stderr, "open %s failed: %s\n", path, strerror(errno));
			goto destroy_barrier;
		}
		opened++;
		if (fstat(workers[index].fd, &status) != 0 ||
		    !S_ISREG(status.st_mode) || (uint64_t)status.st_size != config.file_bytes) {
			fprintf(stderr, "%s must be exactly %" PRIu64 " bytes\n", path,
				config.file_bytes);
			goto destroy_barrier;
		}
		if ((uint64_t)status.st_blocks * 512 < config.file_bytes) {
			fprintf(stderr, "%s must be fully allocated before measurement\n", path);
			goto destroy_barrier;
		}
		workers[index].shared = &shared;
		workers[index].thread_id = index;
		workers[index].stats.latency_histogram =
			calloc(HISTOGRAM_BINS, sizeof(uint64_t));
		if (workers[index].stats.latency_histogram == NULL) {
			perror("calloc latency histogram");
			goto destroy_barrier;
		}
	}

	for (index = 0; index < config.threads; index++) {
		int error = create_worker(&threads[index], worker_main, &workers[index]);

		if (error != 0) {
			errno = error;
			perror("pthread_create");
			pthread_mutex_lock(&shared.startup_mutex);
			shared.startup_state = STARTUP_ABORT;
			pthread_cond_broadcast(&shared.startup_cond);
			pthread_mutex_unlock(&shared.startup_mutex);
			goto join_threads;
		}
		created++;
	}
	pthread_mutex_lock(&shared.startup_mutex);
	while (shared.started_threads != config.threads)
		pthread_cond_wait(&shared.startup_cond, &shared.startup_mutex);
	shared.startup_state = STARTUP_RUN;
	pthread_cond_broadcast(&shared.startup_cond);
	pthread_mutex_unlock(&shared.startup_mutex);

join_threads:
	for (index = 0; index < created; index++)
		pthread_join(threads[index], NULL);
	if (created != config.threads || atomic_load_explicit(&shared.failed, memory_order_relaxed))
		goto destroy_barrier;

	finished = monotonic_now();
	for (index = 0; index < config.threads; index++) {
		unsigned int bin;
		struct stats *stats = &workers[index].stats;

		operations += stats->operations;
		reads += stats->reads;
		writes += stats->writes;
		fsyncs += stats->fsyncs;
		read_bytes += stats->read_bytes;
		write_bytes += stats->write_bytes;
		latency_sum_ns += stats->latency_sum_ns;
		latency_overflow_count += stats->latency_overflow_count;
		for (bin = 0; bin < HISTOGRAM_BINS; bin++)
			histogram[bin] += stats->latency_histogram[bin];
	}
	elapsed = (double)elapsed_ns(shared.measurement_start, finished) / 1e9;
	printf("{\"threads\":%u,\"files_opened\":%u,\"file_bytes\":%" PRIu64
	       ",\"read_percent\":%u,\"sync_percent\":%u,\"seed\":%" PRIu64
	       ",\"warmup_seconds\":%u,\"runtime_seconds\":%u,"
	       "\"elapsed_seconds\":%.9f,\"operations\":%" PRIu64
	       ",\"reads\":%" PRIu64 ",\"writes\":%" PRIu64
	       ",\"fsyncs\":%" PRIu64 ",\"read_bytes\":%" PRIu64
	       ",\"write_bytes\":%" PRIu64 ",\"operations_per_second\":%.6f,"
	       "\"mib_per_second\":%.6f,\"mean_latency_us\":%.6f,"
	       "\"p50_latency_us\":%" PRIu64 ",\"p95_latency_us\":%" PRIu64
	       ",\"p99_latency_us\":%" PRIu64 ",\"p999_latency_us\":%" PRIu64
	       ",\"latency_overflow_count\":%" PRIu64 "}\n",
	       config.threads, opened, config.file_bytes, config.read_percent,
	       config.sync_percent, config.seed, config.warmup_seconds,
	       config.runtime_seconds, elapsed, operations, reads, writes, fsyncs,
	       read_bytes, write_bytes, operations / elapsed,
	       (read_bytes + write_bytes) / (1024.0 * 1024.0) / elapsed,
	       operations == 0 ? 0.0 : latency_sum_ns / 1000.0 / operations,
	       histogram_quantile(histogram, operations, 50, 100),
	       histogram_quantile(histogram, operations, 95, 100),
	       histogram_quantile(histogram, operations, 99, 100),
	       histogram_quantile(histogram, operations, 999, 1000),
	       latency_overflow_count);
	result = operations == 0 ? EXIT_FAILURE : EXIT_SUCCESS;

destroy_barrier:
	pthread_barrier_destroy(&shared.phase_barrier);
	pthread_cond_destroy(&shared.startup_cond);
	pthread_mutex_destroy(&shared.startup_mutex);
cleanup:
	if (workers != NULL) {
		for (index = 0; index < config.threads; index++) {
			if (index < opened)
				close(workers[index].fd);
			free(workers[index].stats.latency_histogram);
		}
	}
	free(histogram);
	free(threads);
	free(workers);
	return result;
}
