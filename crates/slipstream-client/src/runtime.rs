mod path;
mod setup;

use self::path::{
    apply_path_mode, drain_path_events, fetch_path_quality, find_resolver_by_addr_mut,
    loop_burst_total, path_poll_burst_max, path_scheduler_weight,
    retire_underperforming_path_if_needed,
};
use self::setup::{bind_tcp_listener, bind_udp_socket, map_io};
use crate::dns::{
    add_paths, expire_inflight_polls, handle_dns_response, maybe_report_debug,
    refresh_resolver_path, resolve_resolvers, resolver_mode_to_c, send_poll_queries,
    sockaddr_storage_to_socket_addr, DnsResponseContext,
};
use crate::error::ClientError;
use crate::pacing::{cwnd_target_polls, inflight_packet_estimate};
use crate::pinning::configure_pinned_certificate;
use crate::streams::{
    acceptor::ClientAcceptor, client_callback, drain_commands, drain_stream_data, handle_command,
    ClientState, Command,
};
use slipstream_core::{net::is_transient_udp_error, normalize_dual_stack_addr};
use slipstream_dns::{build_qname, encode_query, QueryParams, CLASS_IN, RR_A, RR_AAAA};
use slipstream_ffi::{
    configure_quic_with_custom,
    picoquic::{
        picoquic_close, picoquic_create, picoquic_create_client_cnx, picoquic_current_time,
        picoquic_disable_keep_alive, picoquic_enable_keep_alive, picoquic_enable_path_callbacks,
        picoquic_enable_path_callbacks_default, picoquic_get_next_wake_delay,
        picoquic_prepare_packet_ex, picoquic_set_callback, slipstream_get_path_target_limit,
        slipstream_has_ready_stream, slipstream_is_flow_blocked, slipstream_mixed_cc_algorithm,
        slipstream_set_cc_override, slipstream_set_default_path_mode,
        slipstream_set_initial_max_path_id, PICOQUIC_MAX_PACKET_SIZE,
        PICOQUIC_PACKET_LOOP_RECV_MAX, PICOQUIC_PACKET_LOOP_SEND_MAX,
    },
    socket_addr_to_storage, take_crypto_errors, ClientConfig, QuicGuard, ResolverMode,
};
use std::ffi::CString;
use std::net::Ipv6Addr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

// Protocol defaults; see docs/config.md for details.
const SLIPSTREAM_ALPN: &str = "picoquic_sample";
const SLIPSTREAM_SNI: &str = "test.example.com";
const DNS_WAKE_DELAY_MAX_US: i64 = 10_000_000;
const DNS_POLL_SLICE_US: u64 = 10_000;
const RECONNECT_SLEEP_MIN_MS: u64 = 1;
const RECONNECT_SLEEP_MAX_MS: u64 = 5_000;
const FLOW_BLOCKED_LOG_INTERVAL_US: u64 = 1_000_000;
const PATH_PREPARE_FAILURE_COOLDOWN_US: u64 = 1_000_000;
const PATH_PREPARE_FAILURE_THRESHOLD: u32 = 3;
const PICOQUIC_ERROR_DISCONNECTED: i32 = 0x400 + 20;

fn is_ipv6_unspecified(host: &str) -> bool {
    host.parse::<Ipv6Addr>()
        .map(|addr| addr.is_unspecified())
        .unwrap_or(false)
}

fn drain_disconnected_commands(command_rx: &mut mpsc::UnboundedReceiver<Command>) -> usize {
    let mut dropped = 0usize;
    while let Ok(command) = command_rx.try_recv() {
        dropped += 1;
        if let Command::NewStream { stream, .. } = command {
            drop(stream);
        }
    }
    dropped
}

pub async fn run_client(config: &ClientConfig<'_>) -> Result<i32, ClientError> {
    let mtu = slipstream_dns::max_payload_len_for_domain(config.domain)
        .map_err(|err| ClientError::new(err.to_string()))? as u32;
    let udp = bind_udp_socket().await?;

    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let data_notify = Arc::new(Notify::new());
    let acceptor = ClientAcceptor::new();
    let debug_streams = config.debug_streams;
    let tcp_host = config.tcp_listen_host;
    let tcp_port = config.tcp_listen_port;
    let mut bound_host = tcp_host.to_string();
    let listener = match bind_tcp_listener(tcp_host, tcp_port).await {
        Ok(listener) => listener,
        Err(err) => {
            if is_ipv6_unspecified(tcp_host) {
                warn!(
                    "Failed to bind TCP listener on {}:{} ({}); falling back to 0.0.0.0",
                    tcp_host, tcp_port, err
                );
                match bind_tcp_listener("0.0.0.0", tcp_port).await {
                    Ok(listener) => {
                        bound_host = "0.0.0.0".to_string();
                        listener
                    }
                    Err(fallback_err) => {
                        return Err(ClientError::new(format!(
                            "Failed to bind TCP listener on {}:{} ({}) or 0.0.0.0:{} ({})",
                            tcp_host, tcp_port, err, tcp_port, fallback_err
                        )));
                    }
                }
            } else {
                return Err(err);
            }
        }
    };
    acceptor.spawn(listener, command_tx.clone());
    info!("Listening on TCP port {} (host {})", tcp_port, bound_host);

    let alpn = CString::new(SLIPSTREAM_ALPN)
        .map_err(|_| ClientError::new("ALPN contains an unexpected null byte"))?;
    let sni = CString::new(SLIPSTREAM_SNI)
        .map_err(|_| ClientError::new("SNI contains an unexpected null byte"))?;
    let cc_override = match config.congestion_control {
        Some(value) => Some(CString::new(value).map_err(|_| {
            ClientError::new("Congestion control contains an unexpected null byte")
        })?),
        None => None,
    };

    let mut state = Box::new(ClientState::new(
        command_tx,
        data_notify.clone(),
        debug_streams,
        acceptor,
    ));
    let state_ptr: *mut ClientState = &mut *state;
    let _state = state;

    let mut reconnect_delay = Duration::from_millis(RECONNECT_SLEEP_MIN_MS);

    loop {
        let mut resolvers = resolve_resolvers(config.resolvers, mtu, config.debug_poll)?;
        if resolvers.is_empty() {
            return Err(ClientError::new("At least one resolver is required"));
        }

        let mut local_addr_storage = socket_addr_to_storage(udp.local_addr().map_err(map_io)?);

        let current_time = unsafe { picoquic_current_time() };
        let quic = unsafe {
            picoquic_create(
                8,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                alpn.as_ptr(),
                Some(client_callback),
                state_ptr as *mut _,
                None,
                std::ptr::null_mut(),
                std::ptr::null(),
                current_time,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        };
        if quic.is_null() {
            let crypto_errors = take_crypto_errors();
            if crypto_errors.is_empty() {
                return Err(ClientError::new("Could not create QUIC context"));
            }
            return Err(ClientError::new(format!(
                "Could not create QUIC context (TLS errors: {})",
                crypto_errors.join("; ")
            )));
        }
        let _quic_guard = QuicGuard::new(quic);
        let mixed_cc = unsafe { slipstream_mixed_cc_algorithm };
        if mixed_cc.is_null() {
            return Err(ClientError::new("Could not load mixed congestion control"));
        }
        unsafe {
            configure_quic_with_custom(quic, mixed_cc, mtu);
            let path_target_limit = (slipstream_get_path_target_limit() as usize).max(1);
            let desired_paths = resolvers.len().min(path_target_limit);
            let desired_initial_max_path_id = desired_paths.saturating_sub(1) as u64;
            slipstream_set_initial_max_path_id(quic, desired_initial_max_path_id);
            if resolvers.len() > path_target_limit {
                warn!(
                    "Configured {} resolvers but path target limit is {}; extra resolvers will be queued until a path slot opens",
                    resolvers.len(),
                    path_target_limit
                );
            }
            picoquic_enable_path_callbacks_default(quic, 1);
            let override_ptr = cc_override
                .as_ref()
                .map(|value| value.as_ptr())
                .unwrap_or(std::ptr::null());
            slipstream_set_cc_override(override_ptr);
        }
        unsafe {
            slipstream_set_default_path_mode(resolver_mode_to_c(resolvers[0].mode));
        }
        if let Some(cert) = config.cert {
            configure_pinned_certificate(quic, cert).map_err(ClientError::new)?;
        }
        let mut server_storage = resolvers[0].storage;
        // picoquic_create_client_cnx calls picoquic_start_client_cnx internally (see picoquic/quicctx.c).
        let cnx = unsafe {
            picoquic_create_client_cnx(
                quic,
                &mut server_storage as *mut _ as *mut libc::sockaddr,
                current_time,
                0,
                sni.as_ptr(),
                alpn.as_ptr(),
                Some(client_callback),
                state_ptr as *mut _,
            )
        };
        if cnx.is_null() {
            return Err(ClientError::new("Could not create QUIC connection"));
        }

        apply_path_mode(cnx, &mut resolvers[0])?;

        unsafe {
            picoquic_set_callback(cnx, Some(client_callback), state_ptr as *mut _);
            picoquic_enable_path_callbacks(cnx, 1);
            if config.keep_alive_interval > 0 {
                picoquic_enable_keep_alive(cnx, config.keep_alive_interval as u64 * 1000);
            } else {
                picoquic_disable_keep_alive(cnx);
            }
        }

        if config.gso {
            warn!("GSO is not implemented in the Rust client loop yet.");
        }

        let mut dns_id = 1u16;
        let mut recv_buf = vec![0u8; 4096];
        let mut send_buf = vec![0u8; PICOQUIC_MAX_PACKET_SIZE];
        let packet_loop_send_max = loop_burst_total(&resolvers, PICOQUIC_PACKET_LOOP_SEND_MAX);
        let packet_loop_recv_max = loop_burst_total(&resolvers, PICOQUIC_PACKET_LOOP_RECV_MAX);
        let mut zero_send_loops = 0u64;
        let mut zero_send_with_streams = 0u64;
        let mut last_flow_block_log_at = 0u64;
        let mut current_resolver_index = 0usize;

        loop {
            let current_time = unsafe { picoquic_current_time() };
            drain_commands(cnx, state_ptr, &mut command_rx);
            drain_stream_data(cnx, state_ptr);
            let closing = unsafe { (*state_ptr).is_closing() };
            if closing {
                break;
            }

            let ready = unsafe { (*state_ptr).is_ready() };
            if ready {
                unsafe {
                    (*state_ptr).update_acceptor_limit(cnx);
                }
                if reconnect_delay != Duration::from_millis(RECONNECT_SLEEP_MIN_MS) {
                    reconnect_delay = Duration::from_millis(RECONNECT_SLEEP_MIN_MS);
                }
                retire_underperforming_path_if_needed(cnx, &mut resolvers, current_time);
                add_paths(cnx, &mut resolvers)?;
                for resolver in resolvers.iter_mut() {
                    if resolver.added && !resolver.retire_pending {
                        apply_path_mode(cnx, resolver)?;
                    }
                }
            }
            drain_path_events(cnx, &mut resolvers, state_ptr);

            for resolver in resolvers.iter_mut() {
                if resolver.mode == ResolverMode::Authoritative {
                    expire_inflight_polls(&mut resolver.inflight_poll_ids, current_time);
                }
            }

            let delay_us =
                unsafe { picoquic_get_next_wake_delay(quic, current_time, DNS_WAKE_DELAY_MAX_US) };
            let delay_us = if delay_us < 0 { 0 } else { delay_us as u64 };
            let streams_len_for_sleep = unsafe { (*state_ptr).streams_len() };
            let mut has_work = streams_len_for_sleep > 0;
            for resolver in resolvers.iter_mut() {
                if !refresh_resolver_path(cnx, resolver) {
                    continue;
                }
                if resolver.retire_pending {
                    continue;
                }
                let pending_for_sleep = match resolver.mode {
                    ResolverMode::Authoritative => {
                        let quality = fetch_path_quality(cnx, resolver);
                        let snapshot = resolver
                            .pacing_budget
                            .as_mut()
                            .map(|budget| budget.target_inflight(&quality, delay_us.max(1)));
                        resolver.last_pacing_snapshot = snapshot;
                        let target = snapshot
                            .map(|snapshot| snapshot.target_inflight)
                            .unwrap_or_else(|| cwnd_target_polls(quality.cwin, mtu));
                        let inflight_packets =
                            inflight_packet_estimate(quality.bytes_in_transit, mtu);
                        target.saturating_sub(inflight_packets)
                    }
                    ResolverMode::Recursive => resolver.pending_polls,
                };
                if pending_for_sleep > 0 {
                    has_work = true;
                }
                if resolver.mode == ResolverMode::Authoritative
                    && !resolver.inflight_poll_ids.is_empty()
                {
                    has_work = true;
                }
            }
            // Avoid a tight poll loop when idle, but keep the short slice during active transfers.
            let timeout_us = if has_work {
                delay_us.clamp(1, DNS_POLL_SLICE_US)
            } else {
                delay_us.max(1)
            };
            let timeout = Duration::from_micros(timeout_us);

            tokio::select! {
                command = command_rx.recv() => {
                    if let Some(command) = command {
                        handle_command(cnx, state_ptr, command);
                    }
                }
                _ = data_notify.notified() => {}
                recv = udp.recv_from(&mut recv_buf) => {
                    match recv {
                        Ok((size, peer)) => {
                            let mut response_ctx = DnsResponseContext {
                                quic,
                                local_addr_storage: &local_addr_storage,
                                resolvers: &mut resolvers,
                            };
                            handle_dns_response(&recv_buf[..size], peer, &mut response_ctx)?;
                            for _ in 1..packet_loop_recv_max {
                                match udp.try_recv_from(&mut recv_buf) {
                                    Ok((size, peer)) => {
                                        handle_dns_response(&recv_buf[..size], peer, &mut response_ctx)?;
                                    }
                                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                                    Err(err) => {
                                        if is_transient_udp_error(&err) {
                                            break;
                                        }
                                        return Err(map_io(err));
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            if !is_transient_udp_error(&err) {
                                return Err(map_io(err));
                            }
                        }
                    }
                }
                _ = sleep(timeout) => {}
            }

            drain_commands(cnx, state_ptr, &mut command_rx);
            drain_stream_data(cnx, state_ptr);
            drain_path_events(cnx, &mut resolvers, state_ptr);

            for _ in 0..packet_loop_send_max {
                let current_time = unsafe { picoquic_current_time() };
                let mut send_length: libc::size_t = 0;
                let mut addr_to: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
                let mut addr_from: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
                let mut if_index: libc::c_int = 0;
                let mut send_msg_size: libc::size_t = 0;

                let mut packet_produced = false;
                let mut disconnected_prepare = false;
                let resolver_count = resolvers.len();
                if resolver_count == 0 {
                    break;
                }
                for resolver in resolvers.iter_mut() {
                    if !resolver.is_schedulable(current_time) {
                        continue;
                    }
                    let weight = path_scheduler_weight(cnx, resolver);
                    resolver.scheduler_credit =
                        (resolver.scheduler_credit + weight).clamp(-4.0, 12.0);
                }

                let mut candidate_indices: Vec<usize> = (0..resolver_count)
                    .filter(|idx| {
                        let resolver = &resolvers[*idx];
                        resolver.is_schedulable(current_time)
                    })
                    .collect();
                if candidate_indices.is_empty() {
                    break;
                }
                candidate_indices.sort_by(|a, b| {
                    let a_credit = resolvers[*a].scheduler_credit;
                    let b_credit = resolvers[*b].scheduler_credit;
                    b_credit.total_cmp(&a_credit).then_with(|| {
                        let a_dist = (resolver_count + *a).wrapping_sub(current_resolver_index)
                            % resolver_count;
                        let b_dist = (resolver_count + *b).wrapping_sub(current_resolver_index)
                            % resolver_count;
                        a_dist.cmp(&b_dist)
                    })
                });

                for idx in candidate_indices {
                    let resolver = &mut resolvers[idx];
                    let ret = unsafe {
                        picoquic_prepare_packet_ex(
                            cnx,
                            resolver.path_id,
                            current_time,
                            send_buf.as_mut_ptr(),
                            mtu as usize,
                            &mut send_length,
                            &mut addr_to,
                            &mut addr_from,
                            &mut if_index,
                            &mut send_msg_size,
                        )
                    };

                    if ret != 0 {
                        if ret == PICOQUIC_ERROR_DISCONNECTED {
                            disconnected_prepare = true;
                            warn!(
                                "picoquic_prepare_packet_ex returned disconnected (code {}) on path {}",
                                ret,
                                resolver.addr
                            );
                            break;
                        }
                        resolver.prepare_failures = resolver.prepare_failures.saturating_add(1);
                        resolver.scheduler_credit = (resolver.scheduler_credit - 0.5).max(-4.0);
                        if resolver.prepare_failures >= PATH_PREPARE_FAILURE_THRESHOLD {
                            resolver.cooldown_until =
                                current_time.saturating_add(PATH_PREPARE_FAILURE_COOLDOWN_US);
                            resolver.prepare_failures = 0;
                            warn!(
                                "Temporarily cooling down path {} after repeated prepare failures",
                                resolver.addr
                            );
                        }
                        tracing::error!(
                            "picoquic_prepare_packet_ex failed with code {} on path {}",
                            ret,
                            resolver.addr
                        );
                        continue;
                    }

                    if ret == 0 && send_length > 0 {
                        packet_produced = true;
                        resolver.prepare_failures = 0;
                        resolver.cooldown_until = 0;
                        resolver.scheduler_credit = (resolver.scheduler_credit - 1.0).max(-4.0);
                        current_resolver_index = (idx + 1) % resolver_count;
                        break;
                    } else {
                        resolver.scheduler_credit = (resolver.scheduler_credit - 0.1).max(-4.0);
                    }
                }
                if disconnected_prepare {
                    break;
                }

                if !packet_produced {
                    // No packet produced from any path
                    zero_send_loops = zero_send_loops.saturating_add(1);
                    let streams_len = unsafe { (*state_ptr).streams_len() };
                    if streams_len > 0 {
                        zero_send_with_streams = zero_send_with_streams.saturating_add(1);
                        let flow_blocked = unsafe { slipstream_is_flow_blocked(cnx) } != 0;
                        if flow_blocked {
                            for resolver in resolvers.iter_mut() {
                                if resolver.mode == ResolverMode::Recursive
                                    && resolver.added
                                    && !resolver.retire_pending
                                {
                                    resolver.pending_polls = resolver.pending_polls.max(1);
                                }
                            }
                        }
                    }
                    break;
                }

                if addr_to.ss_family == 0 {
                    break;
                }
                let mut transport_qtype = RR_AAAA;
                if let Ok(dest) = sockaddr_storage_to_socket_addr(&addr_to) {
                    let dest = normalize_dual_stack_addr(dest);
                    if let Some(resolver) = find_resolver_by_addr_mut(&mut resolvers, dest) {
                        resolver.local_addr_storage = Some(unsafe { std::ptr::read(&addr_from) });
                        resolver.debug.send_packets = resolver.debug.send_packets.saturating_add(1);
                        resolver.debug.send_bytes =
                            resolver.debug.send_bytes.saturating_add(send_length as u64);
                        transport_qtype = if resolver.mode == ResolverMode::Recursive {
                            RR_A
                        } else {
                            RR_AAAA
                        };
                    }
                }

                // Adaptive MTU: Choose encoding based on packet size
                // Small packets (<= 200 bytes): Use QNAME encoding (resilient mode)
                // Large packets (> 200 bytes): Use EDNS0 OPT encoding (high-speed mode)
                let packet =
                    if transport_qtype == RR_A || send_length <= slipstream_dns::EDNS0_THRESHOLD {
                        // QNAME encoding (legacy/resilient mode)
                        let qname = build_qname(&send_buf[..send_length], config.domain)
                            .map_err(|err| ClientError::new(err.to_string()))?;
                        let params = QueryParams {
                            id: dns_id,
                            qname: &qname,
                            qtype: transport_qtype,
                            qclass: CLASS_IN,
                            rd: true,
                            cd: false,
                            qdcount: 1,
                            is_query: true,
                        };
                        encode_query(&params).map_err(|err| ClientError::new(err.to_string()))?
                    } else {
                        // EDNS0 OPT encoding (high-speed mode)
                        slipstream_dns::build_query_with_edns0_payload(
                            &send_buf[..send_length],
                            config.domain,
                            dns_id,
                        )
                        .map_err(|err| ClientError::new(err.to_string()))?
                    };
                dns_id = dns_id.wrapping_add(1);

                let dest = sockaddr_storage_to_socket_addr(&addr_to)?;
                let dest = normalize_dual_stack_addr(dest);
                local_addr_storage = addr_from;
                if let Err(err) = udp.send_to(&packet, dest).await {
                    if !is_transient_udp_error(&err) {
                        return Err(map_io(err));
                    }
                }
            }

            let has_ready_stream = unsafe { slipstream_has_ready_stream(cnx) != 0 };
            let flow_blocked = unsafe { slipstream_is_flow_blocked(cnx) != 0 };
            let streams_len = unsafe { (*state_ptr).streams_len() };
            if streams_len > 0 && has_ready_stream && flow_blocked {
                let now = unsafe { picoquic_current_time() };
                if now.saturating_sub(last_flow_block_log_at) >= FLOW_BLOCKED_LOG_INTERVAL_US {
                    let metrics = unsafe { (*state_ptr).stream_debug_metrics() };
                    let backlog = unsafe { (*state_ptr).stream_backlog_summaries(8) };
                    let (enqueued_bytes, last_enqueue_at) =
                        unsafe { (*state_ptr).debug_snapshot() };
                    let last_enqueue_ms = if last_enqueue_at == 0 {
                        0
                    } else {
                        now.saturating_sub(last_enqueue_at) / 1_000
                    };
                    error!(
                        "connection flow blocked: streams={} streams_with_rx_queued={} queued_bytes_total={} streams_with_recv_fin={} streams_with_send_fin={} streams_discarding={} streams_with_unconsumed_rx={} enqueued_bytes={} last_enqueue_ms={} zero_send_with_streams={} zero_send_loops={} flow_blocked={} has_ready_stream={} backlog={:?}",
                        streams_len,
                        metrics.streams_with_rx_queued,
                        metrics.queued_bytes_total,
                        metrics.streams_with_recv_fin,
                        metrics.streams_with_send_fin,
                        metrics.streams_discarding,
                        metrics.streams_with_unconsumed_rx,
                        enqueued_bytes,
                        last_enqueue_ms,
                        zero_send_with_streams,
                        zero_send_loops,
                        flow_blocked,
                        has_ready_stream,
                        backlog
                    );
                    last_flow_block_log_at = now;
                }
            }
            for resolver in resolvers.iter_mut() {
                if !refresh_resolver_path(cnx, resolver) {
                    continue;
                }
                if resolver.retire_pending {
                    continue;
                }
                match resolver.mode {
                    ResolverMode::Authoritative => {
                        let quality = fetch_path_quality(cnx, resolver);
                        let snapshot = resolver.last_pacing_snapshot;
                        let pacing_target = snapshot
                            .map(|snapshot| snapshot.target_inflight)
                            .unwrap_or_else(|| cwnd_target_polls(quality.cwin, mtu));
                        let inflight_packets =
                            inflight_packet_estimate(quality.bytes_in_transit, mtu);
                        let mut poll_deficit = pacing_target.saturating_sub(inflight_packets);
                        if has_ready_stream && !flow_blocked {
                            poll_deficit = 0;
                        }
                        if poll_deficit > 0 && resolver.debug.enabled {
                            debug!(
                                "cc_state: {} cwnd={} in_transit={} rtt_us={} flow_blocked={} deficit={}",
                                resolver.label(),
                                quality.cwin,
                                quality.bytes_in_transit,
                                quality.rtt,
                                flow_blocked,
                                poll_deficit
                            );
                        }
                        if poll_deficit > 0 {
                            let burst_max = path_poll_burst_max(resolver);
                            let mut to_send = poll_deficit.min(burst_max);
                            send_poll_queries(
                                cnx,
                                &udp,
                                config,
                                &mut local_addr_storage,
                                &mut dns_id,
                                resolver,
                                &mut to_send,
                                &mut send_buf,
                            )
                            .await?;
                        }
                    }
                    ResolverMode::Recursive => {
                        resolver.last_pacing_snapshot = None;
                        if resolver.pending_polls > 0 {
                            let burst_max = path_poll_burst_max(resolver);
                            if resolver.pending_polls > burst_max {
                                let mut to_send = burst_max;
                                send_poll_queries(
                                    cnx,
                                    &udp,
                                    config,
                                    &mut local_addr_storage,
                                    &mut dns_id,
                                    resolver,
                                    &mut to_send,
                                    &mut send_buf,
                                )
                                .await?;
                                resolver.pending_polls = resolver
                                    .pending_polls
                                    .saturating_sub(burst_max)
                                    .saturating_add(to_send);
                            } else {
                                let mut pending = resolver.pending_polls;
                                send_poll_queries(
                                    cnx,
                                    &udp,
                                    config,
                                    &mut local_addr_storage,
                                    &mut dns_id,
                                    resolver,
                                    &mut pending,
                                    &mut send_buf,
                                )
                                .await?;
                                resolver.pending_polls = pending;
                            }
                        }
                    }
                }
            }

            let report_time = unsafe { picoquic_current_time() };
            let (enqueued_bytes, last_enqueue_at) = unsafe { (*state_ptr).debug_snapshot() };
            let streams_len = unsafe { (*state_ptr).streams_len() };
            for resolver in resolvers.iter_mut() {
                resolver.debug.enqueued_bytes = enqueued_bytes;
                resolver.debug.last_enqueue_at = last_enqueue_at;
                resolver.debug.zero_send_loops = zero_send_loops;
                resolver.debug.zero_send_with_streams = zero_send_with_streams;
                if !refresh_resolver_path(cnx, resolver) {
                    continue;
                }
                if resolver.retire_pending {
                    continue;
                }
                let inflight_polls = resolver.inflight_poll_ids.len();
                let pending_for_debug = match resolver.mode {
                    ResolverMode::Authoritative => {
                        let quality = fetch_path_quality(cnx, resolver);
                        let inflight_packets =
                            inflight_packet_estimate(quality.bytes_in_transit, mtu);
                        resolver
                            .last_pacing_snapshot
                            .map(|snapshot| {
                                snapshot.target_inflight.saturating_sub(inflight_packets)
                            })
                            .unwrap_or(0)
                    }
                    ResolverMode::Recursive => resolver.pending_polls,
                };
                maybe_report_debug(
                    resolver,
                    report_time,
                    streams_len,
                    pending_for_debug,
                    inflight_polls,
                    resolver.last_pacing_snapshot,
                );
            }
        }

        unsafe {
            picoquic_close(cnx, 0);
        }

        unsafe {
            (*state_ptr).reset_for_reconnect();
        }
        let dropped = drain_disconnected_commands(&mut command_rx);
        if dropped > 0 {
            warn!("Dropped {} queued commands while reconnecting", dropped);
        }
        warn!(
            "Connection closed; reconnecting in {}ms",
            reconnect_delay.as_millis()
        );
        // Sleep in small chunks and drop commands that arrive while disconnected.
        let mut remaining_sleep = reconnect_delay;
        while remaining_sleep > Duration::ZERO {
            let chunk = remaining_sleep.min(Duration::from_millis(100));
            sleep(chunk).await;
            remaining_sleep -= chunk;
            let _ = drain_disconnected_commands(&mut command_rx);
        }
        reconnect_delay = (reconnect_delay * 2).min(Duration::from_millis(RECONNECT_SLEEP_MAX_MS));
    }
}
