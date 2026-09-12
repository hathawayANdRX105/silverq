#![cfg(feature = "meow")]

use silverq::dataplane::udp::*;
use std::net::SocketAddr;

#[test]
fn parse_ipv4_header() {
    // RSV RSV FRAG ATYP=1 1.1.1.1 :53 + payload
    let mut pkt = vec![0x00, 0x00, 0x00, 0x01, 1, 1, 1, 1];
    pkt.extend_from_slice(&53u16.to_be_bytes());
    pkt.extend_from_slice(b"query");

    let h = parse_udp_header(&pkt).unwrap();
    assert_eq!(h.dst_host, "1.1.1.1");
    assert_eq!(h.dst_port, 53);
    assert_eq!(h.frag, 0);
    assert_eq!(&pkt[h.payload_offset..], b"query");
}

#[test]
fn parse_domain_header() {
    let host = b"example.com";
    let mut pkt = vec![0x00, 0x00, 0x00, 0x03, host.len() as u8];
    pkt.extend_from_slice(host);
    pkt.extend_from_slice(&443u16.to_be_bytes());
    pkt.extend_from_slice(b"data");

    let h = parse_udp_header(&pkt).unwrap();
    assert_eq!(h.dst_host, "example.com");
    assert_eq!(h.dst_port, 443);
    assert_eq!(&pkt[h.payload_offset..], b"data");
}

#[test]
fn parse_ipv6_header() {
    let mut pkt = vec![0x00, 0x00, 0x00, 0x04];
    pkt.extend_from_slice(&[0u8; 15]);
    pkt.push(1); // ::1
    pkt.extend_from_slice(&53u16.to_be_bytes());
    pkt.extend_from_slice(b"x");

    let h = parse_udp_header(&pkt).unwrap();
    assert_eq!(h.dst_host, "::1");
    assert_eq!(h.dst_port, 53);
    assert_eq!(&pkt[h.payload_offset..], b"x");
}

#[test]
fn reject_truncated_and_bad_atyp() {
    assert!(parse_udp_header(&[0x00, 0x00, 0x00]).is_none(), "太短");
    // ATYP=1 但地址被截断
    assert!(parse_udp_header(&[0x00, 0x00, 0x00, 0x01, 1, 1]).is_none());
    // 域名长度声明超出实际
    assert!(parse_udp_header(&[0x00, 0x00, 0x00, 0x03, 99, b'a']).is_none());
    // 未知 ATYP
    assert!(parse_udp_header(&[0x00, 0x00, 0x00, 0x09, 1, 2, 3, 4, 0, 53]).is_none());
}

#[test]
fn reply_roundtrips_through_parser() {
    let src: SocketAddr = "8.8.8.8:53".parse().unwrap();
    let framed = encode_udp_reply(src, b"answer");

    // 回程头部格式与请求头部一致，可以用同一个解析器验证
    let h = parse_udp_header(&framed).unwrap();
    assert_eq!(h.dst_host, "8.8.8.8");
    assert_eq!(h.dst_port, 53);
    assert_eq!(&framed[h.payload_offset..], b"answer");
}

#[test]
fn reply_encodes_ipv6_source() {
    let src: SocketAddr = "[::1]:443".parse().unwrap();
    let framed = encode_udp_reply(src, b"p");
    assert_eq!(framed[3], 0x04, "IPv6 来源必须用 ATYP=4");
    let h = parse_udp_header(&framed).unwrap();
    assert_eq!(h.dst_host, "::1");
    assert_eq!(h.dst_port, 443);
}
