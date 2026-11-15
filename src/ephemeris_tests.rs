use super::*;

#[test]
fn listing_parser_handles_html_and_ftp() {
    let html = r#"<tr><td><a href="BRDC00WRD_R_20263180000_01D_MN.rnx.gz">x</a></td></tr>
<tr><td><a href="BRD400DLR_S_20263180000_01D_MN.rnx.gz">y</a></td></tr>"#;
    assert_eq!(
        parse_ftp_names(html),
        vec![
            "BRDC00WRD_R_20263180000_01D_MN.rnx.gz",
            "BRD400DLR_S_20263180000_01D_MN.rnx.gz"
        ]
    );
    let ftp = "-rw-r--r-- 1 0 0 1234 Jan 1 00:00 BRDC00WRD_R_20263180000_01D_MN.rnx.gz\n";
    assert_eq!(
        parse_ftp_names(ftp),
        vec!["BRDC00WRD_R_20263180000_01D_MN.rnx.gz"]
    );
}

#[test]
fn crc24_known_vector() {
    assert_eq!(crc24q(b"123456789"), [0xcd, 0xe7, 0x03]);
}

#[test]
fn bit_writer_round_trip() {
    let mut writer = BitWriter::new();
    writer.put(0b1010, 4).unwrap();
    writer.put(0x1234, 16).unwrap();
    assert_eq!(writer.size(), 20);
    assert_eq!(writer.payload(), [0xa1, 0x23, 0x40]);
}

#[test]
fn frame_length_and_crc() {
    let payload = vec![0x3f, 0xd0, 0x01];
    let raw = frame(&payload);
    assert_eq!(raw.len(), 9);
    assert_eq!(raw[0], 0xd3);
    assert_eq!(
        ((raw[1] as usize & 3) << 8) | raw[2] as usize,
        payload.len()
    );
    assert_eq!(crc24q(&raw[..6]), [raw[6], raw[7], raw[8]]);
}

#[test]
fn gps_encoder_size() {
    let entry = NavEntry {
        sv: 1,
        af0: 0.0,
        af1: 0.0,
        af2: 0.0,
        iode: 1,
        crs: 0.0,
        deltan: 0.0,
        m0: 0.0,
        cuc: 0.0,
        e: 0.01,
        cus: 0.0,
        sqrta: 5200.0,
        toe: 16.0,
        cic: 0.0,
        omg0: 0.0,
        cis: 0.0,
        i0: 0.9,
        crc: 0.0,
        omg: 0.0,
        omgdot: 0.0,
        idot: 0.0,
        week: 2444,
        kind: NavKind::Gps,
    };
    let message = encode_nav(&entry, 1019, SC).unwrap();
    assert_eq!(message.len(), 61);
    assert_eq!(decode_message_type(&message), 1019);
}

#[test]
fn glonass_encoder_size() {
    let entry = GloEntry {
        sv: 1,
        freq: 1.0,
        clk: 0.0,
        gamma: 0.0,
        health: 0,
        x: 10000.0,
        vx: 1.0,
        ax: 0.0,
        y: 20000.0,
        vy: -1.0,
        ay: 0.0,
        z: 30000.0,
        vz: 1.0,
        az: 0.0,
        abs_epoch: 0.0,
        epoch_minutes: 120,
    };
    let message = encode_glo(&entry).unwrap();
    assert_eq!(message.len(), 45);
    assert_eq!(decode_message_type(&message), 1020);
}

#[test]
fn rinex_flex_floats() {
    assert_eq!(
        parse_floats(" -.156471505761e-04 .2034e+06 -1.2D-04"),
        [-0.0000156471505761, 203400.0, -0.00012]
    );
}

#[test]
fn rinex3_epoch_prefix_is_stripped() {
    // BKG 的 RINEX 3 记录以"年 月 日 时 分 秒"开头，必须剥离后再按 GPS/BDS/GAL
    // 字段取值，否则 af0 被当作年份、周数整体错位。
    let text = "\
     3.05           NAVIGATION DATA     MIXED               RINEX VERSION / TYPE
                                                            END OF HEADER
G04 2026 11 14 00 00 00 6.467755883932E-05-3.410605131648E-13 0.000000000000E+00
     6.000000000000E+01-7.459375000000E+01 3.931949495765E-09 1.787140290252E+00
    -3.794208168983E-06 4.104259773158E-03 9.303912520409E-06 5.153651403427E+03
     5.184000000000E+05-5.029141902924E-08 6.831821066484E-01 1.862645149231E-08
     9.736762439238E-01 2.079062500000E+02-2.895474028184E+00-7.678891285216E-09
     1.707213969457E-10 1.000000000000E+00 2.444000000000E+03 0.000000000000E+00
     2.000000000000E+00 0.000000000000E+00-4.656612873077E-09 6.000000000000E+01
     5.164860000000E+05 4.000000000000E+00
";
    let records = parse_nav(text);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.system, 'G');
    assert_eq!(record.sv, 4);
    assert!((record.vals[0] - 6.467755883932e-05).abs() < 1e-18);
    let entry = nav_entry(records.into_iter().next().unwrap(), 0).unwrap();
    assert_eq!(entry.sv, 4);
    assert!((entry.af0 - 6.467755883932e-05).abs() < 1e-18);
    assert!((entry.sqrta - 5153.651403427).abs() < 1e-9);
}

#[test]
fn zip_contains_local_agnss_only() {
    let zip = build_zip(b"rtcm", 1794637801);
    let names = zip_name_entries(&zip);
    assert_eq!(
        names,
        vec![
            "time",
            "ephemeris_config.json",
            "agnss/HW_AGNSS_RTCM_33",
            "agnss/HW_AGNSS"
        ]
    );
    assert!(zip
        .windows(4)
        .any(|window| window == [0x50, 0x4b, 0x05, 0x06]));
    // 严格解析器要求 EOCD 是完整 22 字节：4 字节签名 + 18 字节字段。
    let eocd = zip
        .windows(4)
        .position(|window| window == [0x50, 0x4b, 0x05, 0x06])
        .expect("missing eocd");
    assert_eq!(zip.len() - eocd, 22);
    assert_eq!(
        u16::from_le_bytes([zip[eocd + 10], zip[eocd + 11]]),
        4,
        "central directory entry count"
    );
    // 中央目录必须紧挨在 EOCD 前面，且大小字段与偏移一致。
    let central_offset = u32::from_le_bytes([
        zip[eocd + 16],
        zip[eocd + 17],
        zip[eocd + 18],
        zip[eocd + 19],
    ]) as usize;
    let central_size = u32::from_le_bytes([
        zip[eocd + 12],
        zip[eocd + 13],
        zip[eocd + 14],
        zip[eocd + 15],
    ]) as usize;
    assert_eq!(central_offset + central_size, eocd);
    assert_eq!(&zip[central_offset..central_offset + 4], &[0x50, 0x4b, 0x01, 0x02]);
}

#[test]
fn fixture_counts() {
    let text = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hourly_multi.rnx"
    ))
    .unwrap();
    let records = parse_nav(&text);
    let gps = records.iter().filter(|record| record.system == 'G').count();
    let bds = records.iter().filter(|record| record.system == 'C').count();
    let galileo = records.iter().filter(|record| record.system == 'E').count();
    let glonass = parse_glo(&text).len();
    assert!(gps >= 16);
    assert!(bds >= 16);
    assert!(galileo >= 8);
    assert!(glonass >= 8);
}

fn decode_message_type(payload: &[u8]) -> u32 {
    ((payload[0] as u32) << 4) | ((payload[1] as u32) >> 4)
}

fn zip_name_entries(zip: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut index = 0;
    while index + 30 <= zip.len() {
        if zip[index..index + 4] == [0x50, 0x4b, 0x03, 0x04] {
            let length = u16::from_le_bytes([zip[index + 26], zip[index + 27]]) as usize;
            let start = index + 30;
            names.push(String::from_utf8_lossy(&zip[start..start + length]).to_string());
            let compressed = u32::from_le_bytes([
                zip[index + 18],
                zip[index + 19],
                zip[index + 20],
                zip[index + 21],
            ]) as usize;
            index = start + length + compressed;
        } else {
            index += 1;
        }
    }
    names
}

#[test]
fn civil_day_conversion_matches_epoch() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2026, 11, 15), 20_772);
    assert_eq!(
        days_from_civil(2026, 11, 15) * 86400 + 2 * 3600 + 15 * 60,
        1_794_708_900
    );
    assert_eq!(chrono_parts(1_794_708_900.0), (2026, 11, 15, 2, 15, 0));
}

#[test]
fn ntrip_extract_keeps_ephemeris_frames_only() {
    let payload = vec![0u8; 40];
    let mut message = vec![0x3f, 0xb0];
    message.extend_from_slice(&payload);
    let mut frame = vec![0xd3, 0x00, message.len() as u8];
    frame.extend_from_slice(&message);
    let crc = crc24q(&frame);
    frame.extend_from_slice(&crc);

    let mut buf = frame;
    let mut out = Vec::new();
    extract_rtcm(&mut buf, &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, 1019);
    assert!(buf.is_empty());
}

#[test]
fn ntrip_extract_drops_corrupted_frames() {
    let frame = vec![0xd3, 0x00, 0x04, 0x3f, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mut buf = frame;
    let mut out = Vec::new();
    extract_rtcm(&mut buf, &mut out);
    assert!(out.is_empty());
}

#[test]
fn ntrip_base64_matches_rfc4648_vectors() {
    fn decode(input: &str) -> Vec<u8> {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut bits = 0;
        for ch in input.bytes().filter(|b| *b != b'=') {
            let value = TABLE.iter().position(|c| *c == ch).unwrap() as u32;
            acc = (acc << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        out
    }
    for sample in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "a@b.c:x"] {
        let encoded = base64(sample);
        assert_eq!(encoded.len() % 4, 0, "长度必须是 4 的倍数");
        assert_eq!(decode(&encoded), sample.as_bytes());
    }
    // 整组（6 字节）不应带填充；2 字节输入补一个 '='，1 字节补两个。
    assert!(!base64("foobar").contains('='));
    assert!(base64("fo").ends_with('='));
    assert!(base64("f").ends_with("=="));
}
