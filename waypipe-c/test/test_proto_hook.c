/*
 * Copyright © 2024 Manuel Stoeckl
 *
 * Permission is hereby granted, free of charge, to any person obtaining
 * a copy of this software and associated documentation files (the
 * "Software"), to deal in the Software without restriction, including
 * without limitation the rights to use, copy, modify, merge, publish,
 * distribute, sublicense, and/or sell copies of the Software, and to
 * permit persons to whom the Software is furnished to do so, subject to
 * the following conditions:
 *
 * The above copyright notice and this permission notice (including the
 * next paragraph) shall be included in all copies or substantial
 * portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
 * EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
 * MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
 * NONINFRINGEMENT.  IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS
 * BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN
 * ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
 * CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

#include "common.h"
#include "main.h"

#include <fcntl.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int parse_video_string(const char *str, struct main_config *config,
		bool on_display_side)
{
	char tmp[128];
	size_t l = strlen(str);
	if (l >= 127) {
		return -1;
	}
	memcpy(tmp, str, l + 1);

	config->video_if_possible = true;
	char *part = strtok(tmp, ",");
	while (part) {
		if (!strcmp(part, "none")) {
			config->video_if_possible = false;
		} else if (!strcmp(part, "h264")) {
			config->video_fmt = VIDEO_H264;
		} else if (!strcmp(part, "vp9")) {
			config->video_fmt = VIDEO_VP9;
		} else if (!strcmp(part, "av1")) {
			config->video_fmt = VIDEO_AV1;
		} else if (!strcmp(part, "hw")) {
			config->prefer_hwvideo = true;
		} else if (!strcmp(part, "hwenc")) {
			if (!on_display_side) {
				config->prefer_hwvideo = true;
			}
		} else if (!strcmp(part, "hwdec")) {
			if (on_display_side) {
				config->prefer_hwvideo = true;
			}
		} else if (!strcmp(part, "sw")) {
			config->prefer_hwvideo = false;
		} else if (!strcmp(part, "swenc")) {
			if (!on_display_side) {
				config->prefer_hwvideo = false;
			}
		} else if (!strcmp(part, "swdec")) {
			if (on_display_side) {
				config->prefer_hwvideo = false;
			}
		} else if (!strncmp(part, "bpf=", 4)) {
			char *ep;
			double bpf = strtod(part + 4, &ep);
			if (*ep == 0 && bpf <= 1e9 && bpf >= 1.0) {
				config->video_bpf = (int)bpf;
			} else {
				return -1;
			}
		} else {
			return -1;
		}
		part = strtok(NULL, ",");
	}
	return 0;
}

static uint32_t conntoken_header(const struct main_config *config,
		bool reconnectable, bool update)
{
	uint32_t header = (WAYPIPE_PROTOCOL_VERSION << 16) | CONN_FIXED_BIT;
	header |= (update ? CONN_UPDATE_BIT : 0);
	header |= (reconnectable ? CONN_RECONNECTABLE_BIT : 0);
#ifdef HAS_LZ4
	header |= (config->compression == COMP_LZ4 ? CONN_LZ4_COMPRESSION : 0);
#endif
#ifdef HAS_ZSTD
	header |= (config->compression == COMP_ZSTD ? CONN_ZSTD_COMPRESSION
						    : 0);
#endif
	if (config->compression == COMP_NONE) {
		header |= CONN_NO_COMPRESSION;
	}
	if (config->video_if_possible) {
		header |= (config->video_fmt == VIDEO_H264 ? CONN_H264_VIDEO
							   : 0);
		header |= (config->video_fmt == VIDEO_VP9 ? CONN_VP9_VIDEO : 0);
		header |= (config->video_fmt == VIDEO_AV1 ? CONN_AV1_VIDEO : 0);
	} else {
		header |= CONN_NO_VIDEO;
	}
#ifdef HAS_DMABUF
	header |= (config->no_gpu ? CONN_NO_DMABUF_SUPPORT : 0);
#else
	header |= CONN_NO_DMABUF_SUPPORT;
#endif
	return header;
}

static int check_conn_header(uint32_t header, const struct main_config *config,
		char *err, size_t err_size)
{
	if ((header & CONN_FIXED_BIT) == 0 && (header & CONN_UNSET_BIT) != 0) {
		snprintf(err, err_size,
				"Waypipe client is rejecting connection header %08" PRIx32
				"; it is either garbage or there was a wire protocol endianness mismatch.",
				header);
		return -1;
	}

	/* Earlier versions strictly required a protocol version match; now
	 * there is a protocol version negotiation where waypipe-server sends
	 * its desired version, and if this is not the minimum, the
	 * waypipe-client's first message in reply will acknowledge that
	 * version. To ensure newer clients still work with older Waypipe (that
	 * checked bits 16-31), the version field is now extracted from bits 3-6
	 * and 16-23.
	 */
	uint32_t version =
			(((header >> 16) & 0xff) << 4) | ((header >> 3) & 0xf);
	wp_debug("Waypipe server is requesting protocol version %u; using default version 16",
			version);

	/* For now, reject mismatches in compression format and video coding
	 * setting, and print an error. Adopting whatever the server asks for
	 * is a minor security issue -- e.g., video handling is a good target
	 * for exploits, and compression can cost CPU time, especially if the
	 * initial connection mechanism were to be expanded to allow setting
	 * compression level. */
	if ((header & CONN_COMPRESSION_MASK) == CONN_ZSTD_COMPRESSION) {
		if (config->compression != COMP_ZSTD) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client is configured for compression=%s, not the compression=ZSTD the Waypipe server expected",
					compression_mode_to_str(
							config->compression));
			return -1;
		}
	} else if ((header & CONN_COMPRESSION_MASK) == CONN_LZ4_COMPRESSION) {
		if (config->compression != COMP_LZ4) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client is configured for compression=%s, not the compression=LZ4 the Waypipe server expected",
					compression_mode_to_str(
							config->compression));
			return -1;
		}
	} else if ((header & CONN_COMPRESSION_MASK) == CONN_NO_COMPRESSION) {
		if (config->compression != COMP_NONE) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client is configured for compression=%s, not the compression=NONE the Waypipe server expected",
					compression_mode_to_str(
							config->compression));
			return -1;
		}
	} else if ((header & CONN_COMPRESSION_MASK) != 0) {
		snprintf(err, err_size,
				"Waypipe client is rejecting connection, Waypipe client is configured for compression=%s, not the unidentified compression type the Waypipe server expected",
				compression_mode_to_str(config->compression));
		return -1;
	}

	if ((header & CONN_VIDEO_MASK) == CONN_VP9_VIDEO) {
		if (!config->video_if_possible) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client was not run with video encoding enabled, unlike Waypipe server");
			return -1;
		}
		if (config->video_fmt != VIDEO_VP9) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client was not configured for the VP9 video coding format requested by the Waypipe server");
			return -1;
		}
	} else if ((header & CONN_VIDEO_MASK) == CONN_H264_VIDEO) {
		if (!config->video_if_possible) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client was not run with video encoding enabled, unlike Waypipe server");
			return -1;
		}
		if (config->video_fmt != VIDEO_H264) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client was not configured for the VP9 video coding format requested by the Waypipe server");
			return -1;
		}
	} else if ((header & CONN_VIDEO_MASK) == CONN_AV1_VIDEO) {
		if (!config->video_if_possible) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client was not run with video encoding enabled, unlike Waypipe server");
			return -1;
		}
		if (config->video_fmt != VIDEO_AV1) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client was not configured for the AV1 video coding format requested by the Waypipe server");
			return -1;
		}
	} else if ((header & CONN_VIDEO_MASK) == CONN_NO_VIDEO) {
		if (config->video_if_possible) {
			snprintf(err, err_size,
					"Waypipe client is rejecting connection, Waypipe client has video encoding enabled, but Waypipe server does not");
			return -1;
		}
	} else if ((header & CONN_VIDEO_MASK) != 0) {
		snprintf(err, err_size,
				"Waypipe client is rejecting connection, Waypipe client was not configured for the unidentified video coding format requested by the Waypipe server");
		return -1;
	}

	return 0;
}

const char *strip_prefix(const char *input, const char *prefix)
{
	if (!strncmp(input, prefix, strlen(prefix))) {
		return input + strlen(prefix);
	} else {
		return NULL;
	}
}

log_handler_func_t log_funcs[2] = {NULL, NULL};
int main(int argc, char **argv)
{
	if (argc == 1 || !strcmp(argv[1], "--help")) {
		printf("Usage: ./test_proto_hook [options] [client-conn|server-conn]\n");
		printf("A program which runs the main proxy loop of Waypipe.\n");
		return EXIT_FAILURE;
	}
	bool debug = false;
	struct main_config config = (struct main_config){
			.drm_node = NULL,
			.n_worker_threads = 1,
			.compression = COMP_NONE,
			.compression_level = 1,
			.no_gpu = false,
			.only_linear_dmabuf = true,
			.video_if_possible = false,
			.video_bpf = 3e5,
			.old_video_mode = false,
			.vsock = false,
			.vsock_cid = 0,
			.vsock_port = 0,
			.vsock_to_host = false,
			.title_prefix = NULL,
			.secctx_app_id = NULL,
	};
	if (strcmp(argv[argc - 1], "client-conn") &&
			strcmp(argv[argc - 1], "server-conn")) {
		fprintf(stderr, "Last argument should be client-conn/server-conn\n");
		return EXIT_FAILURE;
	}
	bool display_side = !strcmp(argv[argc - 1], "client-conn");

	const char *value;
	for (int i = 1; i < argc - 1; i++) {
		if (!strcmp(argv[i], "--debug")) {
			debug = true;
		} else if (!strcmp(argv[i], "--no-gpu")) {
			config.no_gpu = true;
		} else if ((value = strip_prefix(argv[i], "--threads="))) {
			uint32_t v;
			if (parse_uint32(value, &v) == -1 || v > (1 << 20)) {
				fprintf(stderr, "Invalid thread count\n");
				return EXIT_FAILURE;
			}
			config.n_worker_threads = (int)v;
		} else if ((value = strip_prefix(argv[i], "--compress="))) {
			if (!strcmp(value, "none")) {
				config.compression = COMP_NONE;
#ifdef HAS_LZ4
			} else if (!strcmp(value, "lz4")) {
				config.compression = COMP_LZ4;
#endif
#ifdef HAS_ZSTD
			} else if (!strcmp(value, "zstd")) {
				config.compression = COMP_ZSTD;
#endif
			}
		} else if ((value = strip_prefix(argv[i], "--drm-node="))) {
			config.drm_node = value;
		} else if ((value = strip_prefix(argv[i], "--title-prefix="))) {
			if (!is_utf8(value) || strlen(value) > 128) {
				fprintf(stderr, "Invalid title prefix of length %d\n",
						(int)strlen(value));
				return EXIT_FAILURE;
			}
			config.title_prefix = value;
		} else if ((value = strip_prefix(argv[i], "--video="))) {
			if (parse_video_string(value, &config, display_side) ==
					-1) {
				fprintf(stderr, "Failed to parse video config string '%s'\n",
						value);
				return EXIT_FAILURE;
			}
		} else if ((value = strip_prefix(
					    argv[i], "--test-wire-version="))) {
			if (strcmp(value, "16")) {
				fprintf(stderr, "Version '%s' not implemented\n",
						value);
				return EXIT_FAILURE;
			}
		} else {
			fprintf(stderr, "Unexpected argument %s. See source code of this program for details.\n",
					argv[i]);
			return EXIT_FAILURE;
		}
	}

	if (debug) {
		log_funcs[0] = test_atomic_log_handler;
	}
	log_funcs[1] = test_atomic_log_handler;

	const char *upstream = getenv("WAYLAND_SOCKET");
	const char *downstream = getenv("WAYPIPE_CONNECTION_FD");
	if (!upstream || !downstream) {
		fprintf(stderr, "Missing environment variable. See source code of this program for details.\n");
		return EXIT_FAILURE;
	}

	uint32_t upstream_u, downstream_u;
	if (parse_uint32(upstream, &upstream_u) == -1 ||
			parse_uint32(downstream, &downstream_u) == -1) {
		fprintf(stderr, "Failed to parse sockets.\n");
		return EXIT_FAILURE;
	}
	int upstream_fd = (int)upstream_u;
	int downstream_fd = (int)downstream_u;

	if (display_side) {
		uint8_t header[16];
		if (read(downstream_fd, header, sizeof(header)) !=
				sizeof(header)) {
			fprintf(stderr, "Failed to read connection header.\n");
			return EXIT_FAILURE;
		}
		uint32_t h = *(uint32_t *)header;
		char err[512];
		if (check_conn_header(h, &config, err, sizeof(err)) == -1) {
			fprintf(stderr, "Bad connection header: %s\n", err);
			return EXIT_FAILURE;
		}
	} else {
		uint32_t header = conntoken_header(&config, false, false);
		uint8_t padding[12] = {0};
		write(upstream_fd, &header, sizeof(header));
		write(upstream_fd, padding, sizeof(padding));
	}

	return main_interface_loop(display_side ? downstream_fd : upstream_fd,
			display_side ? upstream_fd : downstream_fd, -1, &config,
			display_side);
}
