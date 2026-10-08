use super::*;

#[derive(Debug, Default, Clone)]
pub(crate) struct PackInfo {
    pub(crate) crcs: Vec<u32>,
    pub(crate) sizes: Vec<u64>,
    pub(crate) pos: u64,
}

impl PackInfo {
    pub(crate) fn write_to<H: Write>(&mut self, header: &mut H) -> std::io::Result<()> {
        header.write_u8(K_PACK_INFO)?;
        write_u64(header, self.pos)?;
        write_u64(header, self.len() as u64)?;
        header.write_u8(K_SIZE)?;
        for size in &self.sizes {
            write_u64(header, *size)?;
        }
        header.write_u8(K_CRC)?;
        let all_crc_defined = self.crcs.iter().all(|f| *f != 0);
        if all_crc_defined {
            header.write_u8(1)?; // all defined
            for crc in self.crcs.iter() {
                header.write_u32(*crc)?;
            }
        } else {
            header.write_u8(0)?; // not all defined
            let mut crc_define_bits = BitSet::with_capacity(self.crcs.len());

            for (i, crc) in self.crcs.iter().cloned().enumerate() {
                if crc != 0 {
                    crc_define_bits.insert(i);
                }
            }
            let mut temp = Vec::with_capacity(self.len());
            write_bit_set(&mut temp, &crc_define_bits)?;
            header.write_all(&temp)?;
            // 7-Zip's `WriteHashDigests`: the values of the defined digests
            // follow the bit vector. A packed stream whose CRC is 0 is
            // recorded as undefined, so its slot is skipped.
            for crc in self.crcs.iter().filter(|crc| **crc != 0) {
                header.write_u32(*crc)?;
            }
        }

        header.write_u8(K_END)?;
        Ok(())
    }
}

impl PackInfo {
    #[inline]
    pub(crate) fn add_stream(&mut self, size: u64, crc: u32) {
        self.sizes.push(size);
        self.crcs.push(crc);
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.sizes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(info: &mut PackInfo) -> Vec<u8> {
        let mut out = Vec::new();
        info.write_to(&mut out).unwrap();
        out
    }

    #[test]
    fn all_defined_crcs_are_written_after_the_all_defined_byte() {
        let mut info = PackInfo::default();
        info.add_stream(100, 0x1111_1111);
        info.add_stream(200, 0x2222_2222);
        let bytes = written(&mut info);
        let crc_at = bytes.iter().position(|b| *b == K_CRC).unwrap();
        assert_eq!(bytes[crc_at + 1], 1);
        assert_eq!(
            &bytes[crc_at + 2..crc_at + 6],
            &0x1111_1111u32.to_le_bytes()
        );
        assert_eq!(
            &bytes[crc_at + 6..crc_at + 10],
            &0x2222_2222u32.to_le_bytes()
        );
        assert_eq!(bytes[crc_at + 10], K_END);
    }

    #[test]
    fn a_zero_crc_stream_is_undefined_and_the_defined_values_still_follow_the_bits() {
        let mut info = PackInfo::default();
        info.add_stream(100, 0x1111_1111);
        info.add_stream(200, 0);
        info.add_stream(300, 0x3333_3333);
        let bytes = written(&mut info);
        let crc_at = bytes.iter().position(|b| *b == K_CRC).unwrap();
        assert_eq!(bytes[crc_at + 1], 0, "not all defined");
        assert_eq!(bytes[crc_at + 2], 0b1010_0000, "streams 0 and 2 defined");
        assert_eq!(
            &bytes[crc_at + 3..crc_at + 7],
            &0x1111_1111u32.to_le_bytes()
        );
        assert_eq!(
            &bytes[crc_at + 7..crc_at + 11],
            &0x3333_3333u32.to_le_bytes()
        );
        assert_eq!(bytes[crc_at + 11], K_END);
    }
}
