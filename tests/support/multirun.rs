//! Generated 7z fixture with explicit LZMA2 dictionary-reset runs.
//! Independent of the archive writer's threaded/unthreaded scheduling policy.

fn number(out: &mut Vec<u8>, value: u64) {
    out.push(0xff);
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
    use lzma_turbo::{LzmaEncProps, crc::crc32, encode_lzma2};
    let input: Vec<u8> = members
        .iter()
        .flat_map(|(_, bytes)| bytes.iter().copied())
        .collect();
    let props = LzmaEncProps::new().with_level(1).with_dict_size(1 << 16);
    let mut packed = Vec::new();
    let mut property = None;
    for chunk in input.chunks(1 << 20) {
        let (prop, mut bytes) = encode_lzma2(chunk, &props).unwrap();
        assert_eq!(*property.get_or_insert(prop), prop);
        assert_eq!(bytes.pop(), Some(0)); // One EOS after all independent runs.
        packed.extend_from_slice(&bytes);
    }
    packed.push(0);
    // Header/MainStreamsInfo/PackInfo, one pack stream at offset zero.
    let mut h = vec![1, 4, 6];
    number(&mut h, 0);
    number(&mut h, 1);
    h.push(9);
    number(&mut h, packed.len() as u64);
    // UnpackInfo/Folder: one LZMA2 coder with one dictionary property byte.
    h.extend_from_slice(&[0, 7, 11, 1, 0, 1, 0x21, 0x21, 1, property.unwrap()]);
    h.push(12);
    number(&mut h, input.len() as u64);
    // SubStreamsInfo, explicit sizes except the last, and CRC for every member.
    h.extend_from_slice(&[0, 8, 13]);
    number(&mut h, members.len() as u64);
    h.push(9);
    for (_, bytes) in &members[..members.len() - 1] {
        number(&mut h, bytes.len() as u64);
    }
    h.extend_from_slice(&[10, 1]);
    for (_, bytes) in members {
        h.extend_from_slice(&crc32(bytes).to_le_bytes());
    }
    h.extend_from_slice(&[0, 0, 5]);
    number(&mut h, members.len() as u64);
    let names: Vec<u8> = members
        .iter()
        .flat_map(|(name, _)| name.encode_utf16().chain([0]).flat_map(u16::to_le_bytes))
        .collect();
    h.push(17);
    number(&mut h, names.len() as u64 + 1);
    h.push(0);
    h.extend_from_slice(&names);
    h.extend_from_slice(&[0, 0]);
    let mut start = Vec::new();
    start.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    start.extend_from_slice(&(h.len() as u64).to_le_bytes());
    start.extend_from_slice(&crc32(&h).to_le_bytes());
    let mut archive = vec![0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c, 0, 4];
    archive.extend_from_slice(&crc32(&start).to_le_bytes());
    archive.extend_from_slice(&start);
    archive.extend_from_slice(&packed);
    archive.extend_from_slice(&h);
    archive
}
