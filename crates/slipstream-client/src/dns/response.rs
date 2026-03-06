use crate::error::ClientError;
use slipstream_dns::{decode_response, RR_A, RR_AAAA};
use slipstream_ffi::picoquic::{
    picoquic_cnx_t, picoquic_current_time, picoquic_incoming_packet_ex, picoquic_quic_t,
    PICOQUIC_PACKET_LOOP_RECV_MAX,
};
use slipstream_ffi::{socket_addr_to_storage, ResolverMode};
use std::collections::HashMap;
use std::net::SocketAddr;
use tracing::warn;

use super::resolver::{ResolverHealthState, ResolverState};
use slipstream_core::normalize_dual_stack_addr;

const MAX_POLL_BURST: usize = PICOQUIC_PACKET_LOOP_RECV_MAX;
const RECURSIVE_AAAA_FALLBACK_FAILURES: u32 = 3;

pub(crate) struct DnsResponseContext<'a> {
    pub(crate) quic: *mut picoquic_quic_t,
    pub(crate) local_addr_storage: &'a libc::sockaddr_storage,
    pub(crate) resolvers: &'a mut [ResolverState],
    pub(crate) resolver_addr_index: &'a HashMap<SocketAddr, usize>,
    pub(crate) resolver_path_index: &'a HashMap<libc::c_int, usize>,
}

pub(crate) fn handle_dns_response(
    buf: &[u8],
    peer: SocketAddr,
    ctx: &mut DnsResponseContext<'_>,
) -> Result<(), ClientError> {
    let peer = normalize_dual_stack_addr(peer);
    let response_id = dns_response_id(buf);
    let peer_resolver_index = ctx.resolver_addr_index.get(&peer).copied();
    if let Some(payload) = decode_response(buf) {
        let mut peer_storage = socket_addr_to_storage(peer);
        let mut local_storage = if let Some(index) = peer_resolver_index {
            ctx.resolvers[index]
                .local_addr_storage
                .as_ref()
                .map(|storage| unsafe { std::ptr::read(storage) })
                .unwrap_or_else(|| unsafe { std::ptr::read(ctx.local_addr_storage) })
        } else {
            unsafe { std::ptr::read(ctx.local_addr_storage) }
        };
        let mut first_cnx: *mut picoquic_cnx_t = std::ptr::null_mut();
        let mut first_path: libc::c_int = -1;
        let current_time = unsafe { picoquic_current_time() };
        let ret = unsafe {
            picoquic_incoming_packet_ex(
                ctx.quic,
                payload.as_ptr() as *mut u8,
                payload.len(),
                &mut peer_storage as *mut _ as *mut libc::sockaddr,
                &mut local_storage as *mut _ as *mut libc::sockaddr,
                0,
                0,
                &mut first_cnx,
                &mut first_path,
                current_time,
            )
        };
        if ret < 0 {
            return Err(ClientError::new("Failed processing inbound QUIC packet"));
        }
        let resolver_index = if first_path >= 0 {
            ctx.resolver_path_index.get(&first_path).copied()
        } else {
            None
        }
        .or(peer_resolver_index);
        if let Some(index) = resolver_index {
            let resolver = &mut ctx.resolvers[index];
            if first_path >= 0 && !resolver.retire_pending && resolver.path_id != first_path {
                resolver.path_id = first_path;
                resolver.added = true;
                resolver.state = ResolverHealthState::Active;
                if resolver.activated_at == 0 {
                    resolver.activated_at = current_time;
                }
            }
            resolver.last_success_at = current_time;
            resolver.success_rate_ewma = (resolver.success_rate_ewma * 0.8) + 0.2;
            resolver.recursive_aaaa_failures = 0;
            resolver.debug.dns_responses = resolver.debug.dns_responses.saturating_add(1);
            if let Some(response_id) = response_id {
                if resolver.mode == ResolverMode::Authoritative {
                    resolver.inflight_poll_ids.remove(&response_id);
                }
            }
            if resolver.mode == ResolverMode::Recursive {
                resolver.pending_polls =
                    resolver.pending_polls.saturating_add(1).min(MAX_POLL_BURST);
            }
        }
    } else if let Some(response_id) = response_id {
        if let Some(index) = peer_resolver_index {
            let resolver = &mut ctx.resolvers[index];
            resolver.debug.dns_responses = resolver.debug.dns_responses.saturating_add(1);
            if resolver.mode == ResolverMode::Authoritative {
                resolver.inflight_poll_ids.remove(&response_id);
            }
            maybe_fallback_recursive_qtype(resolver, buf);
        }
    }
    Ok(())
}

fn dns_response_meta(packet: &[u8]) -> Option<(u8, u16)> {
    if packet.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if flags & 0x8000 == 0 {
        return None;
    }
    let rcode = (flags & 0x000F) as u8;
    let ancount = u16::from_be_bytes([packet[6], packet[7]]);
    Some((rcode, ancount))
}

fn maybe_fallback_recursive_qtype(resolver: &mut ResolverState, packet: &[u8]) {
    if resolver.mode != ResolverMode::Recursive || resolver.transport_qtype() != RR_AAAA {
        return;
    }
    let Some((rcode, ancount)) = dns_response_meta(packet) else {
        return;
    };
    if rcode == 0 && ancount == 0 {
        // Empty NOERROR poll responses are expected and should not trigger fallback.
        return;
    }
    if rcode == 0 {
        resolver.recursive_aaaa_failures = 0;
        return;
    }
    resolver.recursive_aaaa_failures = resolver.recursive_aaaa_failures.saturating_add(1);
    if resolver.recursive_aaaa_failures >= RECURSIVE_AAAA_FALLBACK_FAILURES {
        resolver.set_recursive_transport_qtype(RR_A);
        resolver.recursive_aaaa_failures = 0;
        warn!(
            "Resolver {} returned repeated rcode={} for recursive AAAA transport; falling back to A",
            resolver.addr, rcode
        );
    }
}

fn dns_response_id(packet: &[u8]) -> Option<u16> {
    if packet.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([packet[0], packet[1]]);
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if flags & 0x8000 == 0 {
        return None;
    }
    Some(id)
}
