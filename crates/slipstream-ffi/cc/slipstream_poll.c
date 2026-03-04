#include "picoquic_internal.h"

typedef struct {
    int reason_code;
    int existing_path_id;
    int partial_match_path_id;
    int has_remote_cnxid_stash;
    int is_multipath_enabled;
    int migration_disabled_remote;
    int migration_disabled_local;
    unsigned int nb_paths;
    unsigned int path_target_limit;
    uint64_t max_path_id_local;
    uint64_t max_path_id_remote;
    uint64_t local_initial_max_path_id;
    uint64_t remote_initial_max_path_id;
    unsigned int remote_active_connection_id_limit;
} slipstream_path_probe_debug_t;

void slipstream_request_poll(picoquic_cnx_t *cnx) {
    if (cnx == NULL) {
        return;
    }
    cnx->is_poll_requested = 1;
}

int slipstream_is_flow_blocked(picoquic_cnx_t *cnx) {
    if (cnx == NULL) {
        return 0;
    }
    return (cnx->flow_blocked || cnx->stream_blocked) ? 1 : 0;
}

int slipstream_has_ready_stream(picoquic_cnx_t *cnx) {
    if (cnx == NULL) {
        return 0;
    }
    return picoquic_find_ready_stream(cnx) != NULL ? 1 : 0;
}

void slipstream_disable_ack_delay(picoquic_cnx_t *cnx) {
    if (cnx == NULL) {
        return;
    }
    cnx->no_ack_delay = 1;
}

int slipstream_find_path_id_by_addr(picoquic_cnx_t *cnx, const struct sockaddr* addr_peer) {
    if (cnx == NULL || addr_peer == NULL || addr_peer->sa_family == 0) {
        return -1;
    }

    for (int path_id = 0; path_id < cnx->nb_paths; path_id++) {
        picoquic_path_t* path_x = cnx->path[path_id];
        if (path_x == NULL) {
            continue;
        }
        if (path_x->path_is_demoted || path_x->path_abandon_received || path_x->path_abandon_sent) {
            continue;
        }
        if (picoquic_compare_addr((struct sockaddr*) &path_x->peer_addr, addr_peer) != 0) {
            continue;
        }
        return path_id;
    }

    return -1;
}

int slipstream_get_path_id_from_unique(picoquic_cnx_t *cnx, uint64_t unique_path_id) {
    if (cnx == NULL) {
        return -1;
    }
    int path_id = picoquic_get_path_id_from_unique(cnx, unique_path_id);
    if (path_id < 0 || path_id >= cnx->nb_paths) {
        return -1;
    }
    picoquic_path_t* path_x = cnx->path[path_id];
    if (path_x == NULL) {
        return -1;
    }
    if (path_x->path_is_demoted || path_x->path_abandon_received || path_x->path_abandon_sent) {
        return -1;
    }
    return path_id;
}

uint64_t slipstream_get_max_streams_bidir_remote(picoquic_cnx_t *cnx) {
    if (cnx == NULL || cnx->remote_parameters_received == 0) {
        return 0;
    }
    /* STREAM_RANK_FROM_ID is 1-based and returns stream count, not a zero-based index. */
    return STREAM_RANK_FROM_ID(cnx->max_stream_id_bidir_remote);
}

unsigned int slipstream_get_path_target_limit(void) {
    return PICOQUIC_NB_PATH_TARGET;
}

void slipstream_set_initial_max_path_id(picoquic_quic_t *quic, uint64_t initial_max_path_id) {
    if (quic == NULL) {
        return;
    }

    uint64_t max_initial_max_path_id = (PICOQUIC_NB_PATH_TARGET > 0) ? (uint64_t)(PICOQUIC_NB_PATH_TARGET - 1) : 0;
    if (initial_max_path_id > max_initial_max_path_id) {
        initial_max_path_id = max_initial_max_path_id;
    }

    quic->default_tp.is_multipath_enabled = 1;
    quic->default_tp.initial_max_path_id = initial_max_path_id;
}

int slipstream_get_path_probe_debug(
    picoquic_cnx_t *cnx,
    const struct sockaddr* addr_peer,
    const struct sockaddr* addr_local,
    slipstream_path_probe_debug_t* debug
) {
    if (cnx == NULL || debug == NULL) {
        return -1;
    }

    debug->reason_code = 0;
    debug->existing_path_id = -1;
    debug->partial_match_path_id = -1;
    debug->has_remote_cnxid_stash = 0;
    debug->is_multipath_enabled = cnx->is_multipath_enabled;
    debug->migration_disabled_remote = cnx->remote_parameters.migration_disabled;
    debug->migration_disabled_local = cnx->local_parameters.migration_disabled;
    debug->nb_paths = (cnx->nb_paths > 0) ? (unsigned int)cnx->nb_paths : 0;
    debug->path_target_limit = PICOQUIC_NB_PATH_TARGET;
    debug->max_path_id_local = cnx->max_path_id_local;
    debug->max_path_id_remote = cnx->max_path_id_remote;
    debug->local_initial_max_path_id = cnx->local_parameters.initial_max_path_id;
    debug->remote_initial_max_path_id = cnx->remote_parameters.initial_max_path_id;
    debug->remote_active_connection_id_limit = cnx->remote_parameters.active_connection_id_limit;

    int partial_match_path = -1;
    if (addr_peer != NULL && addr_peer->sa_family != 0) {
        debug->existing_path_id = picoquic_find_path_by_address(cnx, addr_local, addr_peer, &partial_match_path);
        debug->partial_match_path_id = partial_match_path;
    }

    if (cnx->first_remote_cnxid_stash != NULL &&
        cnx->first_remote_cnxid_stash->cnxid_stash_first != NULL) {
        debug->has_remote_cnxid_stash = 1;
    }

    if ((cnx->remote_parameters.migration_disabled && (addr_peer == NULL || addr_peer->sa_family != 0)) ||
        cnx->local_parameters.migration_disabled) {
        debug->reason_code = 1; /* migration disabled */
    } else if (debug->existing_path_id >= 0) {
        debug->reason_code = 2; /* path already exists */
    } else if (debug->partial_match_path_id >= 0 && (addr_peer == NULL || addr_peer->sa_family == 0)) {
        debug->reason_code = 2; /* path already exists (partial match) */
    } else if (!debug->has_remote_cnxid_stash) {
        debug->reason_code = 3; /* no remote CID stash */
    } else if (cnx->nb_paths >= PICOQUIC_NB_PATH_TARGET) {
        debug->reason_code = 4; /* hard path target limit reached */
    } else if ((uint64_t)cnx->nb_paths > cnx->max_path_id_remote ||
        (uint64_t)cnx->nb_paths > cnx->max_path_id_local) {
        debug->reason_code = 5; /* max_path_id negotiation limit reached */
    } else if (cnx->is_multipath_enabled == 0) {
        debug->reason_code = 6; /* multipath not negotiated */
    }

    return 0;
}
