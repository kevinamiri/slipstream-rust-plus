use crate::error::ClientError;
use crate::pacing::{PacingBudgetSnapshot, PacingPollBudget};
use slipstream_core::{normalize_dual_stack_addr, resolve_host_port};
use slipstream_dns::{RR_A, RR_AAAA, RR_TXT};
use slipstream_ffi::{socket_addr_to_storage, ResolverMode, ResolverSpec};
use std::collections::HashMap;
use std::net::SocketAddr;
use tracing::warn;

use super::debug::DebugMetrics;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolverHealthState {
    Active,
    Probe,
    Cooldown,
    Retiring,
    Disabled,
}

pub(crate) struct ResolverState {
    pub(crate) addr: SocketAddr,
    pub(crate) storage: libc::sockaddr_storage,
    pub(crate) local_addr_storage: Option<libc::sockaddr_storage>,
    pub(crate) mode: ResolverMode,
    pub(crate) state: ResolverHealthState,
    pub(crate) added: bool,
    pub(crate) retire_pending: bool,
    pub(crate) path_id: libc::c_int,
    pub(crate) unique_path_id: Option<u64>,
    pub(crate) probe_attempts: u32,
    pub(crate) failure_streak: u32,
    pub(crate) next_probe_at: u64,
    pub(crate) last_probe_reason_code: i32,
    pub(crate) last_probe_reason_repeats: u32,
    pub(crate) activated_at: u64,
    pub(crate) last_success_at: u64,
    pub(crate) last_failure_at: u64,
    pub(crate) success_rate_ewma: f64,
    pub(crate) throughput_ewma: f64,
    pub(crate) rtt_ewma: f64,
    pub(crate) loss_ewma: f64,
    pub(crate) score_ewma: f64,
    pub(crate) recursive_qtype: u16,
    pub(crate) recursive_transport_failures: u32,
    pub(crate) pending_polls: usize,
    pub(crate) inflight_poll_ids: HashMap<u16, u64>,
    pub(crate) pacing_budget: Option<PacingPollBudget>,
    pub(crate) last_pacing_snapshot: Option<PacingBudgetSnapshot>,
    pub(crate) scheduler_credit: f64,
    pub(crate) prepare_failures: u32,
    pub(crate) cooldown_until: u64,
    pub(crate) poor_quality_streak: u32,
    pub(crate) last_quality_eval_at: u64,
    pub(crate) path_lookup_misses: u32,
    pub(crate) debug: DebugMetrics,
}

impl ResolverState {
    pub(crate) fn label(&self) -> String {
        format!(
            "path_id={} unique_id={:?} resolver={} mode={:?} state={:?} retire_pending={}",
            self.path_id,
            self.unique_path_id,
            self.addr,
            self.mode,
            self.state,
            self.retire_pending
        )
    }

    pub(crate) fn is_path_occupied(&self) -> bool {
        self.added || self.retire_pending
    }

    pub(crate) fn is_probe_due(&self, now: u64) -> bool {
        if self.is_path_occupied() || self.next_probe_at > now {
            return false;
        }
        matches!(
            self.state,
            ResolverHealthState::Probe | ResolverHealthState::Cooldown
        )
    }

    pub(crate) fn is_schedulable(&self, now: u64) -> bool {
        self.path_id >= 0
            && !self.retire_pending
            && self.cooldown_until <= now
            && matches!(self.state, ResolverHealthState::Active)
    }

    pub(crate) fn transport_qtype(&self) -> u16 {
        match self.mode {
            ResolverMode::Authoritative => RR_AAAA,
            ResolverMode::Recursive => self.recursive_qtype,
        }
    }

    pub(crate) fn set_recursive_transport_qtype(&mut self, qtype: u16) {
        if self.mode != ResolverMode::Recursive {
            return;
        }
        if qtype == RR_A || qtype == RR_AAAA || qtype == RR_TXT {
            self.recursive_qtype = qtype;
        }
    }
}

pub(crate) fn resolve_resolvers_with_bootstrap(
    resolvers: &[ResolverSpec],
    mtu: u32,
    debug_poll: bool,
    bootstrap_index: usize,
) -> Result<Vec<ResolverState>, ClientError> {
    let mut resolved = Vec::with_capacity(resolvers.len());
    let mut seen = HashMap::new();
    if resolvers.is_empty() {
        return Ok(resolved);
    }
    let start = bootstrap_index % resolvers.len();
    for offset in 0..resolvers.len() {
        let idx = (start + offset) % resolvers.len();
        let resolver = &resolvers[idx];
        let addr = resolve_host_port(&resolver.resolver)
            .map_err(|err| ClientError::new(err.to_string()))?;
        let addr = normalize_dual_stack_addr(addr);
        if let Some(existing_mode) = seen.get(&addr) {
            return Err(ClientError::new(format!(
                "Duplicate resolver address {} (modes: {:?} and {:?})",
                addr, existing_mode, resolver.mode
            )));
        }
        seen.insert(addr, resolver.mode);
        let is_primary = offset == 0;
        resolved.push(ResolverState {
            addr,
            storage: socket_addr_to_storage(addr),
            local_addr_storage: None,
            mode: resolver.mode,
            state: if is_primary {
                ResolverHealthState::Active
            } else {
                ResolverHealthState::Probe
            },
            added: is_primary,
            retire_pending: false,
            path_id: if is_primary { 0 } else { -1 },
            unique_path_id: if is_primary { Some(0) } else { None },
            probe_attempts: 0,
            failure_streak: 0,
            next_probe_at: 0,
            last_probe_reason_code: i32::MIN,
            last_probe_reason_repeats: 0,
            activated_at: if is_primary { 1 } else { 0 },
            last_success_at: 0,
            last_failure_at: 0,
            success_rate_ewma: if is_primary { 0.9 } else { 0.5 },
            throughput_ewma: 0.0,
            rtt_ewma: 100_000.0,
            loss_ewma: 0.0,
            score_ewma: if is_primary { 1.5 } else { 1.0 },
            recursive_qtype: RR_AAAA,
            recursive_transport_failures: 0,
            pending_polls: 0,
            inflight_poll_ids: HashMap::new(),
            pacing_budget: match resolver.mode {
                ResolverMode::Authoritative => Some(PacingPollBudget::new(mtu)),
                ResolverMode::Recursive => None,
            },
            last_pacing_snapshot: None,
            scheduler_credit: 0.0,
            prepare_failures: 0,
            cooldown_until: 0,
            poor_quality_streak: 0,
            last_quality_eval_at: 0,
            path_lookup_misses: 0,
            debug: DebugMetrics::new(debug_poll),
        });
    }
    Ok(resolved)
}

pub(crate) fn reset_resolver_path(resolver: &mut ResolverState) {
    warn!(
        "Path for resolver {} became unavailable; resetting state",
        resolver.addr
    );
    let disabled = matches!(resolver.state, ResolverHealthState::Disabled);
    resolver.added = false;
    resolver.retire_pending = false;
    resolver.path_id = -1;
    resolver.unique_path_id = None;
    resolver.local_addr_storage = None;
    resolver.pending_polls = 0;
    resolver.inflight_poll_ids.clear();
    resolver.last_pacing_snapshot = None;
    resolver.scheduler_credit = 0.0;
    resolver.prepare_failures = 0;
    resolver.cooldown_until = 0;
    resolver.poor_quality_streak = 0;
    resolver.last_quality_eval_at = 0;
    resolver.path_lookup_misses = 0;
    resolver.probe_attempts = 0;
    resolver.next_probe_at = 0;
    resolver.activated_at = 0;
    resolver.recursive_transport_failures = 0;
    resolver.last_probe_reason_code = i32::MIN;
    resolver.last_probe_reason_repeats = 0;
    resolver.state = if disabled {
        ResolverHealthState::Disabled
    } else {
        ResolverHealthState::Probe
    };
}

pub(crate) fn sockaddr_storage_to_socket_addr(
    storage: &libc::sockaddr_storage,
) -> Result<SocketAddr, ClientError> {
    slipstream_ffi::sockaddr_storage_to_socket_addr(storage).map_err(ClientError::new)
}

#[cfg(test)]
mod tests {
    use super::resolve_resolvers_with_bootstrap;
    use slipstream_core::{AddressFamily, HostPort};
    use slipstream_ffi::{ResolverMode, ResolverSpec};

    #[test]
    fn rejects_duplicate_resolver_addr() {
        let resolvers = vec![
            ResolverSpec {
                resolver: HostPort {
                    host: "127.0.0.1".to_string(),
                    port: 8853,
                    family: AddressFamily::V4,
                },
                mode: ResolverMode::Recursive,
            },
            ResolverSpec {
                resolver: HostPort {
                    host: "127.0.0.1".to_string(),
                    port: 8853,
                    family: AddressFamily::V4,
                },
                mode: ResolverMode::Authoritative,
            },
        ];

        match resolve_resolvers_with_bootstrap(&resolvers, 900, false, 0) {
            Ok(_) => panic!("expected duplicate resolver error"),
            Err(err) => assert!(err.to_string().contains("Duplicate resolver address")),
        }
    }

    #[test]
    fn bootstrap_rotation_promotes_selected_resolver() {
        let resolvers = vec![
            ResolverSpec {
                resolver: HostPort {
                    host: "127.0.0.1".to_string(),
                    port: 5301,
                    family: AddressFamily::V4,
                },
                mode: ResolverMode::Recursive,
            },
            ResolverSpec {
                resolver: HostPort {
                    host: "127.0.0.1".to_string(),
                    port: 5302,
                    family: AddressFamily::V4,
                },
                mode: ResolverMode::Recursive,
            },
            ResolverSpec {
                resolver: HostPort {
                    host: "127.0.0.1".to_string(),
                    port: 5303,
                    family: AddressFamily::V4,
                },
                mode: ResolverMode::Recursive,
            },
        ];

        let resolved =
            resolve_resolvers_with_bootstrap(&resolvers, 900, false, 1).expect("should resolve");
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].addr.port(), 5302);
        assert!(resolved[0].added);
        assert_eq!(resolved[0].path_id, 0);
        assert_eq!(resolved[1].addr.port(), 5303);
        assert!(!resolved[1].added);
        assert_eq!(resolved[2].addr.port(), 5301);
        assert!(!resolved[2].added);
    }
}
