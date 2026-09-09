use ogg::{PacketWriteEndInfo, PacketWriter};

pub fn synthetic_ogg_opus() -> Vec<u8> {
    const SERIAL: u32 = 0x56_54_01;
    let mut head = Vec::from(*b"OpusHead");
    head.extend_from_slice(&[1, 1]);
    head.extend_from_slice(&312_u16.to_le_bytes());
    head.extend_from_slice(&48_000_u32.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.push(0);
    let mut tags = Vec::from(*b"OpusTags");
    tags.extend_from_slice(&0_u32.to_le_bytes());
    tags.extend_from_slice(&0_u32.to_le_bytes());

    let mut writer = PacketWriter::new(Vec::new());
    writer
        .write_packet(head, SERIAL, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(tags, SERIAL, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(
            vec![0xf8, 0xff, 0xfe],
            SERIAL,
            PacketWriteEndInfo::EndStream,
            960,
        )
        .unwrap();
    writer.into_inner()
}
