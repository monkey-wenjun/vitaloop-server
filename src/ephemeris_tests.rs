use super::*;

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
