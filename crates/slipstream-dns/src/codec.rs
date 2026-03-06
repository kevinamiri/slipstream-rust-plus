use crate::base32;
use crate::dots;

use crate::name::{encode_name, extract_subdomain_multi, parse_name};
use crate::types::{
    DecodeQueryError, DecodedQuery, DnsError, QueryParams, Rcode, ResponseParams, EDNS_UDP_PAYLOAD,
    RR_A, RR_AAAA, RR_OPT, RR_TXT,
};
use crate::wire::{
    parse_header, parse_question, parse_question_for_reply, read_u16, read_u32, write_u16,
    write_u32,
};

pub fn decode_query(packet: &[u8], domain: &str) -> Result<DecodedQuery, DecodeQueryError> {
    decode_query_with_domains(packet, &[domain])
}

pub fn decode_query_with_domains(
    packet: &[u8],
    domains: &[&str],
) -> Result<DecodedQuery, DecodeQueryError> {
    let header = match parse_header(packet) {
        Some(header) => header,
        None => return Err(DecodeQueryError::Drop),
    };

    let rd = header.rd;
    let cd = header.cd;

    if header.is_response {
        let question = parse_question_for_reply(packet, header.qdcount, header.offset)?;
        return Err(DecodeQueryError::Reply {
            id: header.id,
            rd,
            cd,
            question,
            rcode: Rcode::FormatError,
        });
    }

    if header.qdcount != 1 {
        let question = parse_question_for_reply(packet, header.qdcount, header.offset)?;
        return Err(DecodeQueryError::Reply {
            id: header.id,
            rd,
            cd,
            question,
            rcode: Rcode::FormatError,
        });
    }

    let question = match parse_question(packet, header.offset) {
        Ok((question, _)) => question,
        Err(_) => return Err(DecodeQueryError::Drop),
    };

    if question.qtype != RR_TXT && question.qtype != RR_AAAA && question.qtype != RR_A {
        return Err(DecodeQueryError::Reply {
            id: header.id,
            rd,
            cd,
            question: Some(question),
            rcode: Rcode::NameError,
        });
    }

    // Check if payload is in EDNS0 OPT record (high-speed mode)
    if let Some(opt_payload) = try_extract_opt_payload(packet, &header) {
        // Verify the question matches one of our domains
        let matches_domain = domains.iter().any(|domain| {
            let domain_with_dot = if domain.ends_with('.') {
                domain.to_string()
            } else {
                format!("{}.", domain)
            };
            question.name == domain_with_dot
        });

        if !matches_domain {
            return Err(DecodeQueryError::Reply {
                id: header.id,
                rd,
                cd,
                question: Some(question),
                rcode: Rcode::NameError,
            });
        }

        return Ok(DecodedQuery {
            id: header.id,
            rd,
            cd,
            question,
            payload: opt_payload,
        });
    }

    // Fall back to QNAME encoding (legacy/resilient mode)
    let subdomain_raw = match extract_subdomain_multi(&question.name, domains) {
        Ok(subdomain_raw) => subdomain_raw,
        Err(rcode) => {
            return Err(DecodeQueryError::Reply {
                id: header.id,
                rd,
                cd,
                question: Some(question),
                rcode,
            })
        }
    };

    let undotted = dots::undotify(&subdomain_raw);
    if undotted.is_empty() {
        return Err(DecodeQueryError::Reply {
            id: header.id,
            rd,
            cd,
            question: Some(question),
            rcode: Rcode::NameError,
        });
    }

    let payload = match base32::decode(&undotted) {
        Ok(payload) => payload,
        Err(_) => {
            return Err(DecodeQueryError::Reply {
                id: header.id,
                rd,
                cd,
                question: Some(question),
                rcode: Rcode::ServerFailure,
            })
        }
    };

    Ok(DecodedQuery {
        id: header.id,
        rd,
        cd,
        question,
        payload,
    })
}

/// Try to extract payload from EDNS0 OPT record
/// Returns Some(payload) if found, None otherwise
fn try_extract_opt_payload(packet: &[u8], header: &crate::wire::Header) -> Option<Vec<u8>> {
    // EDNS0 is in Additional Records section (ARCOUNT)
    if header.arcount == 0 {
        return None;
    }

    // Skip over question section
    let mut offset = header.offset;
    for _ in 0..header.qdcount {
        let (_, new_offset) = parse_name(packet, offset).ok()?;
        offset = new_offset;
        if offset + 4 > packet.len() {
            return None;
        }
        offset += 4; // Skip QTYPE and QCLASS
    }

    // Skip over answer and authority sections
    for _ in 0..(header.ancount + header.nscount) {
        let (_, new_offset) = parse_name(packet, offset).ok()?;
        offset = new_offset;
        if offset + 10 > packet.len() {
            return None;
        }
        offset += 8; // Skip TYPE, CLASS, TTL
        let rdlen = read_u16(packet, offset)? as usize;
        offset += 2;
        if offset + rdlen > packet.len() {
            return None;
        }
        offset += rdlen;
    }

    // Check additional records for OPT
    for _ in 0..header.arcount {
        let _name_start = offset;
        let (name, new_offset) = parse_name(packet, offset).ok()?;
        offset = new_offset;

        if offset + 10 > packet.len() {
            return None;
        }

        let rr_type = read_u16(packet, offset)?;
        offset += 2;
        let _class = read_u16(packet, offset)?;
        offset += 2;
        let _ttl = read_u32(packet, offset)?;
        offset += 4;
        let rdlen = read_u16(packet, offset)? as usize;
        offset += 2;

        if offset + rdlen > packet.len() {
            return None;
        }

        // Check if this is an OPT record with root name
        if rr_type == RR_OPT && name.is_empty() {
            // Extract the payload from RDATA
            let payload = packet[offset..offset + rdlen].to_vec();
            return Some(payload);
        }

        offset += rdlen;
    }

    None
}

pub fn encode_query(params: &QueryParams<'_>) -> Result<Vec<u8>, DnsError> {
    let mut out = Vec::with_capacity(256);
    let mut flags = 0u16;
    if !params.is_query {
        flags |= 0x8000;
    }
    if params.rd {
        flags |= 0x0100;
    }
    if params.cd {
        flags |= 0x0010;
    }

    write_u16(&mut out, params.id);
    write_u16(&mut out, flags);
    write_u16(&mut out, params.qdcount);
    write_u16(&mut out, 0);
    write_u16(&mut out, 0);
    write_u16(&mut out, 1);

    if params.qdcount > 0 {
        encode_name(params.qname, &mut out)?;
        write_u16(&mut out, params.qtype);
        write_u16(&mut out, params.qclass);
    }

    encode_opt_record(&mut out)?;

    Ok(out)
}

pub fn encode_response(params: &ResponseParams<'_>) -> Result<Vec<u8>, DnsError> {
    let payload_len = params.payload.map(|payload| payload.len()).unwrap_or(0);

    let mut rcode = params.rcode.unwrap_or(if payload_len > 0 {
        Rcode::Ok
    } else {
        Rcode::NameError
    });

    let mut ancount = 0u16;
    if payload_len > 0 && rcode == Rcode::Ok {
        ancount = match params.question.qtype {
            RR_TXT => 1,
            RR_A => {
                let framed_len = payload_len
                    .checked_add(2)
                    .ok_or_else(|| DnsError::new("payload too long"))?;
                // Each A answer has:
                // - 1 byte sequence index
                // - 3 bytes payload chunk
                let answers = framed_len.div_ceil(3);
                if answers > (u8::MAX as usize + 1) {
                    return Err(DnsError::new("payload too long"));
                }
                answers as u16
            }
            RR_AAAA => {
                let framed_len = payload_len
                    .checked_add(2)
                    .ok_or_else(|| DnsError::new("payload too long"))?;
                // Each AAAA answer has:
                // - 2 bytes sequence index
                // - 14 bytes payload chunk
                let answers = framed_len.div_ceil(14);
                if answers > u16::MAX as usize {
                    return Err(DnsError::new("payload too long"));
                }
                answers as u16
            }
            _ => return Err(DnsError::new("unsupported qtype for payload")),
        };
    } else if params.rcode.is_some() {
        rcode = params.rcode.unwrap_or(Rcode::Ok);
    }

    let mut out = Vec::with_capacity(256);
    let mut flags = 0x8000 | 0x0400;
    if params.rd {
        flags |= 0x0100;
    }
    if params.cd {
        flags |= 0x0010;
    }
    flags |= rcode.to_u8() as u16;

    write_u16(&mut out, params.id);
    write_u16(&mut out, flags);
    write_u16(&mut out, 1);
    write_u16(&mut out, ancount);
    write_u16(&mut out, 0);
    write_u16(&mut out, 1);

    encode_name(&params.question.name, &mut out)?;
    write_u16(&mut out, params.question.qtype);
    write_u16(&mut out, params.question.qclass);

    if ancount > 0 {
        let payload = params.payload.unwrap_or(&[]);
        match params.question.qtype {
            RR_TXT => {
                out.extend_from_slice(&[0xC0, 0x0C]);
                write_u16(&mut out, params.question.qtype);
                write_u16(&mut out, params.question.qclass);
                write_u32(&mut out, 60);
                let chunk_count = payload_len.div_ceil(255);
                let rdata_len = payload_len + chunk_count;
                if rdata_len > u16::MAX as usize {
                    return Err(DnsError::new("payload too long"));
                }
                write_u16(&mut out, rdata_len as u16);
                let mut remaining = payload_len;
                let mut cursor = 0;
                while remaining > 0 {
                    let chunk_len = remaining.min(255);
                    out.push(chunk_len as u8);
                    out.extend_from_slice(&payload[cursor..cursor + chunk_len]);
                    cursor += chunk_len;
                    remaining -= chunk_len;
                }
            }
            RR_A => {
                if payload_len > u16::MAX as usize {
                    return Err(DnsError::new("payload too long"));
                }
                let mut framed = Vec::with_capacity(payload_len + 2);
                framed.extend_from_slice(&(payload_len as u16).to_be_bytes());
                framed.extend_from_slice(payload);
                for (seq, chunk) in framed.chunks(3).enumerate() {
                    let seq = u8::try_from(seq).map_err(|_| DnsError::new("too many A chunks"))?;
                    out.extend_from_slice(&[0xC0, 0x0C]);
                    write_u16(&mut out, params.question.qtype);
                    write_u16(&mut out, params.question.qclass);
                    write_u32(&mut out, 60);
                    write_u16(&mut out, 4);
                    out.push(seq);
                    out.extend_from_slice(chunk);
                    if chunk.len() < 3 {
                        out.resize(out.len() + (3 - chunk.len()), 0);
                    }
                }
            }
            RR_AAAA => {
                if payload_len > u16::MAX as usize {
                    return Err(DnsError::new("payload too long"));
                }
                let mut framed = Vec::with_capacity(payload_len + 2);
                framed.extend_from_slice(&(payload_len as u16).to_be_bytes());
                framed.extend_from_slice(payload);
                for (seq, chunk) in framed.chunks(14).enumerate() {
                    let seq =
                        u16::try_from(seq).map_err(|_| DnsError::new("too many AAAA chunks"))?;
                    out.extend_from_slice(&[0xC0, 0x0C]);
                    write_u16(&mut out, params.question.qtype);
                    write_u16(&mut out, params.question.qclass);
                    write_u32(&mut out, 60);
                    write_u16(&mut out, 16);
                    out.extend_from_slice(&seq.to_be_bytes());
                    out.extend_from_slice(chunk);
                    if chunk.len() < 14 {
                        out.resize(out.len() + (14 - chunk.len()), 0);
                    }
                }
            }
            _ => return Err(DnsError::new("unsupported qtype for payload")),
        }
    }

    encode_opt_record(&mut out)?;

    Ok(out)
}

pub fn decode_response(packet: &[u8]) -> Option<Vec<u8>> {
    let header = parse_header(packet)?;
    if !header.is_response {
        return None;
    }
    let rcode = header.rcode?;
    if rcode != Rcode::Ok {
        return None;
    }
    if header.ancount == 0 {
        return None;
    }

    let mut offset = header.offset;
    for _ in 0..header.qdcount {
        let (_, new_offset) = parse_name(packet, offset).ok()?;
        offset = new_offset;
        if offset + 4 > packet.len() {
            return None;
        }
        offset += 4;
    }

    let mut answer_qtype = None;
    let mut txt_payload = Vec::new();
    let mut a_chunks = Vec::new();
    let mut aaaa_chunks = Vec::new();

    for _ in 0..header.ancount {
        let (_, new_offset) = parse_name(packet, offset).ok()?;
        offset = new_offset;
        if offset + 10 > packet.len() {
            return None;
        }
        let qtype = read_u16(packet, offset)?;
        offset += 2;
        let _qclass = read_u16(packet, offset)?;
        offset += 2;
        let _ttl = read_u32(packet, offset)?;
        offset += 4;
        let rdlen = read_u16(packet, offset)? as usize;
        offset += 2;
        if offset + rdlen > packet.len() || rdlen < 1 {
            return None;
        }

        if let Some(existing) = answer_qtype {
            if existing != qtype {
                return None;
            }
        } else {
            answer_qtype = Some(qtype);
        }

        match qtype {
            RR_TXT => {
                let mut remaining = rdlen;
                let mut cursor = offset;
                while remaining > 0 {
                    let txt_len = packet[cursor] as usize;
                    cursor += 1;
                    remaining -= 1;
                    if txt_len > remaining {
                        return None;
                    }
                    txt_payload.extend_from_slice(&packet[cursor..cursor + txt_len]);
                    cursor += txt_len;
                    remaining -= txt_len;
                }
            }
            RR_A => {
                if rdlen != 4 {
                    return None;
                }
                let seq = packet[offset];
                let mut chunk = [0u8; 3];
                chunk.copy_from_slice(&packet[offset + 1..offset + 4]);
                a_chunks.push((seq, chunk));
            }
            RR_AAAA => {
                if rdlen != 16 {
                    return None;
                }
                let seq = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
                let mut chunk = [0u8; 14];
                chunk.copy_from_slice(&packet[offset + 2..offset + 16]);
                aaaa_chunks.push((seq, chunk));
            }
            _ => return None,
        }

        offset += rdlen;
    }

    match answer_qtype {
        Some(RR_TXT) => {
            if txt_payload.is_empty() {
                return None;
            }
            Some(txt_payload)
        }
        Some(RR_A) => {
            if a_chunks.is_empty() {
                return None;
            }

            // Recursive resolvers may reorder RRsets; sort by sequence and validate contiguity.
            a_chunks.sort_unstable_by_key(|(seq, _)| *seq);
            for (expected, (seq, _)) in a_chunks.iter().enumerate() {
                if *seq as usize != expected {
                    return None;
                }
            }

            let mut framed = Vec::with_capacity(a_chunks.len() * 3);
            for (_, chunk) in &a_chunks {
                framed.extend_from_slice(chunk);
            }
            if framed.len() < 2 {
                return None;
            }

            let payload_len = u16::from_be_bytes([framed[0], framed[1]]) as usize;
            if payload_len == 0 {
                return None;
            }
            let end = 2usize.checked_add(payload_len)?;
            if end > framed.len() {
                return None;
            }
            Some(framed[2..end].to_vec())
        }
        Some(RR_AAAA) => {
            if aaaa_chunks.is_empty() {
                return None;
            }

            // Recursive resolvers may reorder RRsets; sort by sequence and validate contiguity.
            aaaa_chunks.sort_unstable_by_key(|(seq, _)| *seq);
            for (expected, (seq, _)) in aaaa_chunks.iter().enumerate() {
                if *seq as usize != expected {
                    return None;
                }
            }

            let mut framed = Vec::with_capacity(aaaa_chunks.len() * 14);
            for (_, chunk) in &aaaa_chunks {
                framed.extend_from_slice(chunk);
            }
            if framed.len() < 2 {
                return None;
            }

            let payload_len = u16::from_be_bytes([framed[0], framed[1]]) as usize;
            if payload_len == 0 {
                return None;
            }
            let end = 2usize.checked_add(payload_len)?;
            if end > framed.len() {
                return None;
            }
            Some(framed[2..end].to_vec())
        }
        _ => None,
    }
}

pub fn is_response(packet: &[u8]) -> bool {
    parse_header(packet)
        .map(|header| header.is_response)
        .unwrap_or(false)
}

fn encode_opt_record(out: &mut Vec<u8>) -> Result<(), DnsError> {
    out.push(0);
    write_u16(out, RR_OPT);
    write_u16(out, EDNS_UDP_PAYLOAD);
    write_u32(out, 0);
    write_u16(out, 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{decode_response, encode_response};
    use crate::name::parse_name;
    use crate::types::{Question, ResponseParams, CLASS_IN, RR_A, RR_AAAA, RR_TXT};
    use crate::wire::{parse_header, read_u16};

    fn reorder_answers_reverse(packet: &[u8]) -> Vec<u8> {
        let header = parse_header(packet).expect("header");
        let mut offset = header.offset;

        for _ in 0..header.qdcount {
            let (_, new_offset) = parse_name(packet, offset).expect("question name");
            offset = new_offset + 4;
        }
        let question_end = offset;

        let mut answers = Vec::new();
        for _ in 0..header.ancount {
            let rr_start = offset;
            let (_, name_end) = parse_name(packet, offset).expect("answer name");
            if name_end + 10 > packet.len() {
                panic!("truncated answer header");
            }
            let rdlen = read_u16(packet, name_end + 8).expect("rdlen") as usize;
            let rr_end = name_end + 10 + rdlen;
            if rr_end > packet.len() {
                panic!("truncated answer rdata");
            }
            answers.push(packet[rr_start..rr_end].to_vec());
            offset = rr_end;
        }

        let mut out = Vec::with_capacity(packet.len());
        out.extend_from_slice(&packet[..question_end]);
        for rr in answers.iter().rev() {
            out.extend_from_slice(rr);
        }
        out.extend_from_slice(&packet[offset..]);
        out
    }

    #[test]
    fn encode_response_rejects_large_payload() {
        let question = Question {
            name: "a.test.com.".to_string(),
            qtype: RR_TXT,
            qclass: CLASS_IN,
        };
        let payload = vec![0u8; u16::MAX as usize];
        let params = ResponseParams {
            id: 0x1234,
            rd: false,
            cd: false,
            question: &question,
            payload: Some(&payload),
            rcode: None,
        };
        assert!(encode_response(&params).is_err());
    }

    #[test]
    fn encode_decode_response_aaaa_roundtrip() {
        let question = Question {
            name: "a.test.com.".to_string(),
            qtype: RR_AAAA,
            qclass: CLASS_IN,
        };
        let payload = vec![0xAB; 73];
        let params = ResponseParams {
            id: 0x4321,
            rd: true,
            cd: false,
            question: &question,
            payload: Some(&payload),
            rcode: None,
        };
        let encoded = encode_response(&params).expect("encode response");
        let decoded = decode_response(&encoded).expect("decode response");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn encode_decode_response_a_roundtrip() {
        let question = Question {
            name: "a.test.com.".to_string(),
            qtype: RR_A,
            qclass: CLASS_IN,
        };
        let payload = vec![0xCD; 73];
        let params = ResponseParams {
            id: 0x1234,
            rd: true,
            cd: false,
            question: &question,
            payload: Some(&payload),
            rcode: None,
        };
        let encoded = encode_response(&params).expect("encode response");
        let decoded = decode_response(&encoded).expect("decode response");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn decode_response_aaaa_survives_answer_reorder() {
        let question = Question {
            name: "a.test.com.".to_string(),
            qtype: RR_AAAA,
            qclass: CLASS_IN,
        };
        let payload = (0u8..96u8).collect::<Vec<_>>();
        let params = ResponseParams {
            id: 0x2222,
            rd: false,
            cd: false,
            question: &question,
            payload: Some(&payload),
            rcode: None,
        };
        let encoded = encode_response(&params).expect("encode response");
        let reordered = reorder_answers_reverse(&encoded);
        let decoded = decode_response(&reordered).expect("decode response");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn decode_response_a_survives_answer_reorder() {
        let question = Question {
            name: "a.test.com.".to_string(),
            qtype: RR_A,
            qclass: CLASS_IN,
        };
        let payload = (0u8..96u8).collect::<Vec<_>>();
        let params = ResponseParams {
            id: 0x3333,
            rd: false,
            cd: false,
            question: &question,
            payload: Some(&payload),
            rcode: None,
        };
        let encoded = encode_response(&params).expect("encode response");
        let reordered = reorder_answers_reverse(&encoded);
        let decoded = decode_response(&reordered).expect("decode response");
        assert_eq!(decoded, payload);
    }
}
