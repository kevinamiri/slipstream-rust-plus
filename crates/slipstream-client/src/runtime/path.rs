use crate::dns::{
    refresh_resolver_path, reset_resolver_path, resolver_mode_to_c,
    sockaddr_storage_to_socket_addr, ResolverHealthState, ResolverState,
};
use crate::error::ClientError;
use crate::streams::{ClientState, PathEvent};
use libc::c_char;
use slipstream_core::normalize_dual_stack_addr;
use slipstream_ffi::picoquic::{
    picoquic_abandon_path, picoquic_cnx_t, picoquic_current_time,
    picoquic_get_default_path_quality, picoquic_get_path_addr, picoquic_get_path_quality,
    slipstream_get_path_id_from_unique, slipstream_get_path_target_limit,
    slipstream_set_path_ack_delay, slipstream_set_path_mode, PICOQUIC_PACKET_LOOP_SEND_MAX,
};
use slipstream_ffi::ResolverMode;
use std::net::SocketAddr;
use tracing::warn;

const AUTHORITATIVE_LOOP_MULTIPLIER: usize = 4;
const PATH_HEALTH_SAMPLE_INTERVAL_US: u64 = 250_000;
const PATH_POOR_SCORE_THRESHOLD: f64 = 0.45;
const PATH_POOR_STREAK_THRESHOLD: u32 = 8;
const PATH_RETIRED_RETRY_DELAY_US: u64 = 10_000_000;
const PATH_MIN_DWELL_US: u64 = 5_000_000;
const PATH_RETIRE_REASON: u64 = 0x73735f7061746852;
const EWMA_ALPHA: f64 = 0.2;

pub(crate) fn apply_path_mode(
    cnx: *mut picoquic_cnx_t,
    resolver: &mut ResolverState,
) -> Result<(), ClientError> {
    if resolver.retire_pending {
        return Ok(());
    }
    if !refresh_resolver_path(cnx, resolver) {
        return Ok(());
    }
    unsafe {
        slipstream_set_path_mode(cnx, resolver.path_id, resolver_mode_to_c(resolver.mode));
        let disable_ack_delay = matches!(resolver.mode, ResolverMode::Authoritative) as libc::c_int;
        slipstream_set_path_ack_delay(cnx, resolver.path_id, disable_ack_delay);
    }
    Ok(())
}

pub(crate) fn fetch_path_quality(
    cnx: *mut picoquic_cnx_t,
    resolver: &ResolverState,
) -> slipstream_ffi::picoquic::picoquic_path_quality_t {
    let mut quality = slipstream_ffi::picoquic::picoquic_path_quality_t::default();
    let mut ret = -1;
    if let Some(unique_path_id) = resolver.unique_path_id {
        ret = unsafe { picoquic_get_path_quality(cnx, unique_path_id, &mut quality as *mut _) };
    }
    if ret != 0 {
        unsafe {
            picoquic_get_default_path_quality(cnx, &mut quality as *mut _);
        }
    }
    quality
}

pub(crate) fn drain_path_events(
    cnx: *mut picoquic_cnx_t,
    resolvers: &mut [ResolverState],
    state_ptr: *mut ClientState,
) {
    if state_ptr.is_null() {
        return;
    }
    let events = unsafe { (*state_ptr).take_path_events() };
    if events.is_empty() {
        return;
    }
    for event in events {
        match event {
            PathEvent::Available(unique_path_id) => {
                if let Some(addr) = path_peer_addr(cnx, unique_path_id) {
                    if let Some(resolver) = find_resolver_by_addr_mut(resolvers, addr) {
                        let path_id =
                            unsafe { slipstream_get_path_id_from_unique(cnx, unique_path_id) };
                        if path_id >= 0 {
                            resolver.unique_path_id = Some(unique_path_id);
                            resolver.path_id = path_id;
                            resolver.added = true;
                            resolver.path_lookup_misses = 0;
                            resolver.retire_pending = false;
                            resolver.state = ResolverHealthState::Active;
                            if resolver.activated_at == 0 {
                                resolver.activated_at = unsafe { picoquic_current_time() };
                            }
                        } else {
                            resolver.unique_path_id = None;
                        }
                    }
                }
            }
            PathEvent::Deleted(unique_path_id) => {
                if let Some(resolver) = find_resolver_by_unique_id_mut(resolvers, unique_path_id) {
                    let now = unsafe { picoquic_current_time() };
                    let was_retiring = resolver.retire_pending;
                    reset_resolver_path(resolver);
                    if was_retiring {
                        resolver.state = ResolverHealthState::Cooldown;
                        resolver.last_failure_at = now;
                        resolver.next_probe_at = now.saturating_add(PATH_RETIRED_RETRY_DELAY_US);
                        resolver.cooldown_until = resolver.next_probe_at;
                    }
                }
            }
        }
    }
}

fn path_peer_addr(cnx: *mut picoquic_cnx_t, unique_path_id: u64) -> Option<SocketAddr> {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let ret = unsafe { picoquic_get_path_addr(cnx, unique_path_id, 2, &mut storage) };
    if ret != 0 {
        return None;
    }
    sockaddr_storage_to_socket_addr(&storage).ok()
}

pub(crate) fn loop_burst_total(resolvers: &[ResolverState], base: usize) -> usize {
    resolvers.iter().fold(0usize, |acc, resolver| {
        acc.saturating_add(base.saturating_mul(path_loop_multiplier(resolver.mode)))
    })
}

pub(crate) fn path_poll_burst_max(resolver: &ResolverState) -> usize {
    PICOQUIC_PACKET_LOOP_SEND_MAX.saturating_mul(path_loop_multiplier(resolver.mode))
}

pub(crate) fn path_scheduler_weight(cnx: *mut picoquic_cnx_t, resolver: &mut ResolverState) -> f64 {
    if !resolver.is_schedulable(unsafe { picoquic_current_time() }) {
        return 0.0;
    }
    let quality = fetch_path_quality(cnx, resolver);
    let rtt_sample = quality.rtt.max(1) as f64;
    let cwin = quality.cwin.max(1) as f64;
    let inflight = quality.bytes_in_transit as f64;
    let sent = quality.sent as f64;
    let lost = quality.lost as f64;
    let delivered = (sent - lost).max(0.0);

    let success_sample = if sent > 0.0 {
        (delivered / sent).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let throughput_sample = if rtt_sample > 0.0 {
        (cwin / rtt_sample).max(0.0)
    } else {
        0.0
    };
    let loss_sample = if sent > 0.0 {
        (lost / sent).clamp(0.0, 1.0)
    } else {
        0.0
    };

    resolver.success_rate_ewma = ewma(resolver.success_rate_ewma, success_sample, EWMA_ALPHA);
    resolver.throughput_ewma = ewma(resolver.throughput_ewma, throughput_sample, EWMA_ALPHA);
    resolver.rtt_ewma = ewma(resolver.rtt_ewma.max(1.0), rtt_sample, EWMA_ALPHA);
    resolver.loss_ewma = ewma(resolver.loss_ewma, loss_sample, EWMA_ALPHA);

    let success_component = resolver.success_rate_ewma;
    let throughput_component = (resolver.throughput_ewma / 1.5).clamp(0.0, 2.0);
    let rtt_component = (100_000.0 / resolver.rtt_ewma.max(1.0)).clamp(0.1, 2.5);
    let transit_penalty = if cwin > 0.0 {
        (inflight / cwin).clamp(0.0, 1.5)
    } else {
        1.5
    };
    let timeout_penalty =
        (resolver.prepare_failures as f64 * 0.15) + (resolver.loss_ewma * 0.75) + transit_penalty;
    let raw_score = success_component + throughput_component + rtt_component - timeout_penalty;
    resolver.score_ewma = ewma(resolver.score_ewma, raw_score, EWMA_ALPHA);

    resolver.score_ewma.clamp(0.0, 4.0)
}

pub(crate) fn retire_underperforming_path_if_needed(
    cnx: *mut picoquic_cnx_t,
    resolvers: &mut [ResolverState],
    current_time: u64,
) {
    if resolvers.len() <= 1 {
        return;
    }
    if resolvers.iter().any(|resolver| resolver.retire_pending) {
        return;
    }
    let path_target_limit = (unsafe { slipstream_get_path_target_limit() } as usize).max(1);
    let active_paths = resolvers
        .iter()
        .filter(|resolver| resolver.is_path_occupied())
        .count();
    let queued_ready = resolvers
        .iter()
        .skip(1)
        .any(|resolver| resolver.is_probe_due(current_time));
    if active_paths < path_target_limit || !queued_ready {
        return;
    }

    let mut candidate: Option<(usize, f64, u64)> = None;
    for (idx, resolver) in resolvers.iter_mut().enumerate().skip(1) {
        if !resolver.added
            || resolver.path_id < 0
            || resolver.retire_pending
            || !matches!(resolver.state, ResolverHealthState::Active)
        {
            continue;
        }
        let Some(unique_path_id) = resolver.unique_path_id else {
            continue;
        };
        if resolver.pending_polls > 0 || !resolver.inflight_poll_ids.is_empty() {
            continue;
        }
        if resolver.activated_at > 0
            && current_time.saturating_sub(resolver.activated_at) < PATH_MIN_DWELL_US
        {
            continue;
        }
        if current_time.saturating_sub(resolver.last_quality_eval_at)
            < PATH_HEALTH_SAMPLE_INTERVAL_US
        {
            continue;
        }
        resolver.last_quality_eval_at = current_time;
        let score = path_scheduler_weight(cnx, resolver);
        if score < PATH_POOR_SCORE_THRESHOLD {
            resolver.poor_quality_streak = resolver.poor_quality_streak.saturating_add(1);
        } else {
            resolver.poor_quality_streak = 0;
            resolver.state = ResolverHealthState::Active;
            continue;
        }
        if resolver.poor_quality_streak < PATH_POOR_STREAK_THRESHOLD {
            continue;
        }
        match candidate {
            Some((_, best_score, _)) if score >= best_score => {}
            _ => candidate = Some((idx, score, unique_path_id)),
        }
    }

    let Some((idx, score, unique_path_id)) = candidate else {
        return;
    };
    let ret = unsafe {
        picoquic_abandon_path(
            cnx,
            unique_path_id,
            PATH_RETIRE_REASON,
            std::ptr::null::<c_char>(),
            current_time,
        )
    };
    if ret == 0 {
        let resolver = &mut resolvers[idx];
        let addr = resolver.addr;
        warn!(
            "Retiring underperforming path {} (score {:.3}, streak >= {}) to free slot for queued resolvers",
            addr, score, PATH_POOR_STREAK_THRESHOLD
        );
        resolver.retire_pending = true;
        resolver.state = ResolverHealthState::Retiring;
        resolver.last_failure_at = current_time;
    } else {
        warn!(
            "Failed to retire path {} (score {:.3}): picoquic_abandon_path ret={}",
            resolvers[idx].addr, score, ret
        );
    }
}

fn path_loop_multiplier(mode: ResolverMode) -> usize {
    match mode {
        ResolverMode::Authoritative => AUTHORITATIVE_LOOP_MULTIPLIER,
        ResolverMode::Recursive => 1,
    }
}

pub(crate) fn find_resolver_by_addr_mut(
    resolvers: &mut [ResolverState],
    addr: SocketAddr,
) -> Option<&mut ResolverState> {
    let addr = normalize_dual_stack_addr(addr);
    resolvers.iter_mut().find(|resolver| resolver.addr == addr)
}

fn find_resolver_by_unique_id_mut(
    resolvers: &mut [ResolverState],
    unique_path_id: u64,
) -> Option<&mut ResolverState> {
    resolvers
        .iter_mut()
        .find(|resolver| resolver.unique_path_id == Some(unique_path_id))
}

fn ewma(prev: f64, sample: f64, alpha: f64) -> f64 {
    if !prev.is_finite() {
        return sample;
    }
    (prev * (1.0 - alpha)) + (sample * alpha)
}
