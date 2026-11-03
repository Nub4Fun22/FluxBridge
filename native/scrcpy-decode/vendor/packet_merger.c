/*
 * packet_merger.c
 *
 * Vendored from scrcpy v5.0.1 (https://github.com/Genymobile/scrcpy),
 * upstream path: app/src/packet_merger.c
 * scrcpy is Copyright (C) Genymobile and scrcpy contributors.
 * Licensed under the Apache License, Version 2.0 (see ../LICENSE and ../NOTICE).
 *
 * MODIFICATIONS (per Apache-2.0 section 4, statement of changes):
 *   - Replaced the scrcpy-internal include "util/log.h" with a minimal
 *     local LOG_OOM() macro so this file compiles standalone against
 *     libavcodec/libavutil only (scrcpy's logging layer is not vendored).
 * The merge algorithm itself is UNCHANGED from upstream.
 */

#include "packet_merger.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <libavutil/avutil.h>

/*
 * scrcpy's LOG_OOM() logs an out-of-memory condition through its own logging
 * layer (util/log.h), which is not vendored. Provide a minimal standalone
 * replacement that writes to stderr so the control flow is identical.
 */
#ifndef LOG_OOM
#define LOG_OOM() fprintf(stderr, "packet_merger: out of memory\n")
#endif

void
sc_packet_merger_init(struct sc_packet_merger *merger) {
    merger->config = NULL;
}

void
sc_packet_merger_destroy(struct sc_packet_merger *merger) {
    free(merger->config);
}

bool
sc_packet_merger_merge(struct sc_packet_merger *merger, AVPacket *packet) {
    bool is_config = packet->pts == AV_NOPTS_VALUE;

    if (is_config) {
        free(merger->config);

        merger->config = malloc(packet->size);
        if (!merger->config) {
            LOG_OOM();
            return false;
        }

        memcpy(merger->config, packet->data, packet->size);
        merger->config_size = packet->size;
    } else if (merger->config) {
        size_t config_size = merger->config_size;
        size_t media_size = packet->size;

        if (av_grow_packet(packet, config_size)) {
            LOG_OOM();
            return false;
        }

        memmove(packet->data + config_size, packet->data, media_size);
        memcpy(packet->data, merger->config, config_size);

        free(merger->config);
        merger->config = NULL;
        // merger->size is meaningless when merger->config is NULL
    }

    return true;
}
