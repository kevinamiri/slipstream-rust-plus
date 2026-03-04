use crate::error::ClientError;
use slipstream_ffi::picoquic::{
    picoquic_cnx_t, picoquic_current_time, picoquic_get_path_addr, picoquic_probe_new_path_ex,
    slipstream_find_path_id_by_addr, slipstream_get_path_id_from_unique,
    slipstream_get_path_probe_debug, slipstream_get_path_target_limit,
    slipstream_path_probe_debug_t, slipstream_set_default_path_mode,
};
use slipstream_ffi::ResolverMode;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tracing::{debug, info, warn};

use super::resolver::{reset_resolver_path, ResolverHealthState, ResolverState};

const PATH_PROBE_INITIAL_DELAY_US: u64 = 250_000;
const PATH_PROBE_MAX_DELAY_US: u64 = 10_000_000;

fn probe_reason_label(reason_code: i32) -> &'static str {
    match reason_code {
        0 => "unknown/transient",
        1 => "migration_disabled",
        2 => "path_already_exists",
        3 => "no_remote_cid_stash",
        4 => "path_target_limit_reached",
        5 => "max_path_id_limit_reached",
        6 => "multipath_not_negotiated",
        _ => "unclassified",
    }
}

fn should_warn_probe_failure(reason_changed: bool, repeats: u32) -> bool {
    reason_changed || repeats <= 3 || repeats.is_power_of_two()
}

pub(crate) fn refresh_resolver_path(
    cnx: *mut picoquic_cnx_t,
    resolver: &mut ResolverState,
) -> bool {
    if resolver.retire_pending {
        if let Some(unique_path_id) = resolver.unique_path_id {
            let path_id = unsafe { slipstream_get_path_id_from_unique(cnx, unique_path_id) };
            if path_id >= 0 {
                resolver.added = true;
                if resolver.path_id != path_id {
                    resolver.path_id = path_id;
                }
                return true;
            }
        }
        return false;
    }

    if let Some(unique_path_id) = resolver.unique_path_id {
        let path_id = unsafe { slipstream_get_path_id_from_unique(cnx, unique_path_id) };
        if path_id >= 0 {
            resolver.added = true;
            if resolver.path_id != path_id {
                resolver.path_id = path_id;
            }
            resolver.state = ResolverHealthState::Active;
            return true;
        }
        resolver.unique_path_id = None;
    }
    let peer = &resolver.storage as *const _ as *const libc::sockaddr;
    let path_id = unsafe { slipstream_find_path_id_by_addr(cnx, peer) };
    if path_id < 0 {
        if resolver.added || resolver.path_id >= 0 {
            reset_resolver_path(resolver);
            resolver.next_probe_at = 0;
        }
        return false;
    }

    resolver.added = true;
    resolver.state = ResolverHealthState::Active;
    if resolver.activated_at == 0 {
        resolver.activated_at = unsafe { picoquic_current_time() };
    }
    if resolver.path_id != path_id {
        resolver.path_id = path_id;
    }
    true
}

pub(crate) fn add_paths(
    cnx: *mut picoquic_cnx_t,
    resolvers: &mut [ResolverState],
) -> Result<(), ClientError> {
    if resolvers.len() <= 1 {
        return Ok(());
    }

    let mut local_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let ret = unsafe { picoquic_get_path_addr(cnx, 0, 1, &mut local_storage) };
    if ret != 0 {
        return Ok(());
    }
    let now = unsafe { picoquic_current_time() };
    let primary_mode = resolvers[0].mode;
    let mut default_mode = primary_mode;
    let path_target_limit = (unsafe { slipstream_get_path_target_limit() } as usize).max(1);
    let mut active_paths = resolvers
        .iter()
        .filter(|resolver| resolver.is_path_occupied())
        .count();

    for resolver in resolvers.iter_mut().skip(1) {
        if active_paths >= path_target_limit {
            break;
        }
        if resolver.is_path_occupied() {
            continue;
        }
        if matches!(
            resolver.state,
            ResolverHealthState::Disabled | ResolverHealthState::Retiring
        ) {
            continue;
        }
        if !resolver.is_probe_due(now) {
            continue;
        }
        if resolver.mode != default_mode {
            unsafe { slipstream_set_default_path_mode(resolver_mode_to_c(resolver.mode)) };
            default_mode = resolver.mode;
        }
        let mut path_id: libc::c_int = -1;
        let ret = unsafe {
            picoquic_probe_new_path_ex(
                cnx,
                &resolver.storage as *const _ as *const libc::sockaddr,
                &local_storage as *const _ as *const libc::sockaddr,
                0,
                now,
                0,
                &mut path_id,
            )
        };
        if ret == 0 && path_id >= 0 {
            resolver.added = true;
            resolver.retire_pending = false;
            resolver.state = ResolverHealthState::Active;
            resolver.path_id = path_id;
            resolver.activated_at = now;
            resolver.last_success_at = now;
            resolver.success_rate_ewma = (resolver.success_rate_ewma * 0.8) + 0.2;
            resolver.failure_streak = 0;
            resolver.next_probe_at = 0;
            active_paths = active_paths.saturating_add(1);
            resolver.last_probe_reason_code = i32::MIN;
            resolver.last_probe_reason_repeats = 0;
            info!(
                "MULTIPATH: Successfully added secondary path to {} (path_id={})",
                resolver.addr, path_id
            );
            continue;
        }
        resolver.probe_attempts = resolver.probe_attempts.saturating_add(1);
        resolver.failure_streak = resolver.failure_streak.saturating_add(1);
        resolver.last_failure_at = now;
        resolver.state = ResolverHealthState::Cooldown;
        resolver.success_rate_ewma *= 0.9;
        let mut probe_debug = slipstream_path_probe_debug_t::default();
        let _ = unsafe {
            slipstream_get_path_probe_debug(
                cnx,
                &resolver.storage as *const _ as *const libc::sockaddr,
                &local_storage as *const _ as *const libc::sockaddr,
                &mut probe_debug as *mut _,
            )
        };
        let reason_changed = resolver.last_probe_reason_code != probe_debug.reason_code;
        if reason_changed {
            resolver.last_probe_reason_code = probe_debug.reason_code;
            resolver.last_probe_reason_repeats = 1;
        } else {
            resolver.last_probe_reason_repeats =
                resolver.last_probe_reason_repeats.saturating_add(1);
        }
        let delay = path_probe_backoff(resolver.failure_streak, resolver.addr);
        resolver.next_probe_at = now.saturating_add(delay);
        let level_warn =
            should_warn_probe_failure(reason_changed, resolver.last_probe_reason_repeats);
        let reason = probe_reason_label(probe_debug.reason_code);
        if level_warn {
            warn!(
                "Failed adding path {} (attempt {}, reason={} code={} repeats={}, retry={}ms, nb_paths={}, target_limit={}, max_path_id_local={}, max_path_id_remote={}, local_initial_max_path_id={}, remote_initial_max_path_id={}, remote_active_cid_limit={}, has_remote_cid_stash={}, multipath_enabled={}, migration_disabled_local={}, migration_disabled_remote={}, existing_path_id={}, partial_match_path_id={})",
                resolver.addr,
                resolver.probe_attempts,
                reason,
                probe_debug.reason_code,
                resolver.last_probe_reason_repeats,
                delay / 1000,
                probe_debug.nb_paths,
                probe_debug.path_target_limit,
                probe_debug.max_path_id_local,
                probe_debug.max_path_id_remote,
                probe_debug.local_initial_max_path_id,
                probe_debug.remote_initial_max_path_id,
                probe_debug.remote_active_connection_id_limit,
                probe_debug.has_remote_cnxid_stash != 0,
                probe_debug.is_multipath_enabled != 0,
                probe_debug.migration_disabled_local != 0,
                probe_debug.migration_disabled_remote != 0,
                probe_debug.existing_path_id,
                probe_debug.partial_match_path_id
            );
        } else {
            debug!(
                "Failed adding path {} (attempt {}, reason={} code={}, repeats={}, retry={}ms)",
                resolver.addr,
                resolver.probe_attempts,
                reason,
                probe_debug.reason_code,
                resolver.last_probe_reason_repeats,
                delay / 1000
            );
        }
    }

    if default_mode != primary_mode {
        unsafe { slipstream_set_default_path_mode(resolver_mode_to_c(primary_mode)) };
    }

    Ok(())
}

pub(crate) fn resolver_mode_to_c(mode: ResolverMode) -> libc::c_int {
    match mode {
        ResolverMode::Recursive => 1,
        ResolverMode::Authoritative => 2,
    }
}

fn path_probe_backoff(attempts: u32, addr: std::net::SocketAddr) -> u64 {
    let shift = attempts.saturating_sub(1).min(8);
    let delay = PATH_PROBE_INITIAL_DELAY_US.saturating_mul(1u64 << shift);
    apply_probe_jitter(delay.min(PATH_PROBE_MAX_DELAY_US), addr, attempts)
}

fn apply_probe_jitter(delay: u64, addr: std::net::SocketAddr, attempts: u32) -> u64 {
    if delay == 0 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    addr.hash(&mut hasher);
    attempts.hash(&mut hasher);
    let jitter_window = (delay / 5).max(1);
    let span = jitter_window.saturating_mul(2).saturating_add(1);
    let jitter = hasher.finish() % span;
    delay
        .saturating_sub(jitter_window)
        .saturating_add(jitter)
        .min(PATH_PROBE_MAX_DELAY_US)
}
