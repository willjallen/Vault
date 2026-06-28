use vault_server::exports::{ZipHeaderProbeInput, zip_header_probe};

#[test]
fn zip_headers_keep_classic_fields_when_values_fit() {
    let probe = zip_header_probe(ZipHeaderProbeInput {
        name: "small.bin",
        compressed_size: 11,
        uncompressed_size: 11,
        local_header_offset: 32,
        entry_count: 1,
        central_directory_size: 64,
        central_directory_offset: 128,
    })
    .expect("zip probe");

    assert_eq!(le_u32(&probe.local_file_header, 0), 0x0403_4b50);
    assert_eq!(le_u16(&probe.local_file_header, 4), 20);
    assert_eq!(le_u32(&probe.local_file_header, 18), 11);
    assert_eq!(le_u32(&probe.local_file_header, 22), 11);
    assert_eq!(le_u16(&probe.local_file_header, 28), 0);

    assert_eq!(le_u32(&probe.central_directory_header, 0), 0x0201_4b50);
    assert_eq!(le_u16(&probe.central_directory_header, 4), 20);
    assert_eq!(le_u16(&probe.central_directory_header, 6), 20);
    assert_eq!(le_u32(&probe.central_directory_header, 20), 11);
    assert_eq!(le_u32(&probe.central_directory_header, 24), 11);
    assert_eq!(le_u16(&probe.central_directory_header, 30), 0);
    assert_eq!(le_u32(&probe.central_directory_header, 42), 32);

    assert_eq!(probe.end_of_central_directory.len(), 22);
    assert_eq!(le_u32(&probe.end_of_central_directory, 0), 0x0605_4b50);
    assert_eq!(le_u16(&probe.end_of_central_directory, 8), 1);
    assert_eq!(le_u16(&probe.end_of_central_directory, 10), 1);
    assert_eq!(le_u32(&probe.end_of_central_directory, 12), 64);
    assert_eq!(le_u32(&probe.end_of_central_directory, 16), 128);
}

#[test]
fn zip_headers_use_zip64_extra_when_entry_size_exceeds_classic_fields() {
    let compressed_size = u64::from(u32::MAX) + 9;
    let uncompressed_size = u64::from(u32::MAX) + 17;
    let local_header_offset = 1234;
    let probe = zip_header_probe(ZipHeaderProbeInput {
        name: "huge.bin",
        compressed_size,
        uncompressed_size,
        local_header_offset,
        entry_count: 1,
        central_directory_size: 64,
        central_directory_offset: 128,
    })
    .expect("zip probe");

    assert_eq!(le_u16(&probe.local_file_header, 4), 45);
    assert_eq!(le_u32(&probe.local_file_header, 18), u32::MAX);
    assert_eq!(le_u32(&probe.local_file_header, 22), u32::MAX);
    assert_eq!(le_u16(&probe.local_file_header, 28), 20);
    let local_extra_offset = 30 + "huge.bin".len();
    assert_eq!(
        zip64_extra_values(&probe.local_file_header[local_extra_offset..]),
        vec![uncompressed_size, compressed_size],
    );

    assert_eq!(le_u16(&probe.central_directory_header, 4), 45);
    assert_eq!(le_u16(&probe.central_directory_header, 6), 45);
    assert_eq!(le_u32(&probe.central_directory_header, 20), u32::MAX);
    assert_eq!(le_u32(&probe.central_directory_header, 24), u32::MAX);
    assert_eq!(le_u16(&probe.central_directory_header, 30), 28);
    assert_eq!(le_u32(&probe.central_directory_header, 42), u32::MAX);
    let central_extra_offset = 46 + "huge.bin".len();
    assert_eq!(
        zip64_extra_values(&probe.central_directory_header[central_extra_offset..]),
        vec![uncompressed_size, compressed_size, local_header_offset],
    );
}

#[test]
fn zip_footer_uses_zip64_records_when_archive_directory_exceeds_classic_fields() {
    let entry_count = u16::MAX as usize + 1;
    let central_directory_size = u64::from(u32::MAX) + 1;
    let central_directory_offset = u64::from(u32::MAX) + 2;
    let probe = zip_header_probe(ZipHeaderProbeInput {
        name: "small.bin",
        compressed_size: 11,
        uncompressed_size: 11,
        local_header_offset: 32,
        entry_count,
        central_directory_size,
        central_directory_offset,
    })
    .expect("zip probe");
    let footer = probe.end_of_central_directory;

    assert_eq!(footer.len(), 98);
    assert_eq!(le_u32(&footer, 0), 0x0606_4b50);
    assert_eq!(le_u64(&footer, 4), 44);
    assert_eq!(le_u16(&footer, 12), 45);
    assert_eq!(le_u16(&footer, 14), 45);
    let entry_count_u64 = u64::try_from(entry_count).expect("entry count");
    assert_eq!(le_u64(&footer, 24), entry_count_u64);
    assert_eq!(le_u64(&footer, 32), entry_count_u64);
    assert_eq!(le_u64(&footer, 40), central_directory_size);
    assert_eq!(le_u64(&footer, 48), central_directory_offset);

    assert_eq!(le_u32(&footer, 56), 0x0706_4b50);
    assert_eq!(
        le_u64(&footer, 64),
        central_directory_offset + central_directory_size,
    );
    assert_eq!(le_u32(&footer, 72), 1);

    assert_eq!(le_u32(&footer, 76), 0x0605_4b50);
    assert_eq!(le_u16(&footer, 84), u16::MAX);
    assert_eq!(le_u16(&footer, 86), u16::MAX);
    assert_eq!(le_u32(&footer, 88), u32::MAX);
    assert_eq!(le_u32(&footer, 92), u32::MAX);
}

fn zip64_extra_values(extra: &[u8]) -> Vec<u64> {
    assert_eq!(le_u16(extra, 0), 0x0001);
    let length = le_u16(extra, 2) as usize;
    assert_eq!(extra.len(), 4 + length);
    extra[4..]
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("zip64 value")))
        .collect()
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}
