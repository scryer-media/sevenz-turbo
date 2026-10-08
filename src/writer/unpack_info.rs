use std::{io::Write, sync::Arc};

use super::*;
use crate::EncoderConfiguration;
#[derive(Debug, Clone, Default)]
pub(crate) struct UnpackInfo {
    pub(crate) blocks: Vec<BlockInfo>,
}

impl UnpackInfo {
    pub(crate) fn add(
        &mut self,
        methods: Arc<Vec<EncoderConfiguration>>,
        sizes: Vec<u64>,
        crc: u32,
    ) -> &mut BlockInfo {
        self.blocks.push(BlockInfo {
            methods,
            sizes,
            crc,
            num_sub_unpack_streams: 1,
            ..Default::default()
        });
        self.blocks.last_mut().expect("just pushed")
    }

    pub(crate) fn add_multiple(
        &mut self,
        methods: Arc<Vec<EncoderConfiguration>>,
        sizes: Vec<u64>,
        crc: u32,
        num_sub_unpack_streams: u64,
        sub_stream_sizes: Vec<u64>,
        sub_stream_crcs: Vec<u32>,
    ) -> &mut BlockInfo {
        self.blocks.push(BlockInfo {
            methods,
            sizes,
            crc,
            num_sub_unpack_streams,
            sub_stream_crcs,
            sub_stream_sizes,
            bcj2: None,
        });
        self.blocks.last_mut().expect("just pushed")
    }

    pub(crate) fn write_to<H: Write>(&mut self, header: &mut H) -> std::io::Result<()> {
        header.write_u8(K_UNPACK_INFO)?;
        header.write_u8(K_FOLDER)?;
        write_u64(header, self.blocks.len() as u64)?;
        header.write_u8(0)?;
        let mut cache = Vec::with_capacity(32);
        for block in self.blocks.iter() {
            block.write_to(header, &mut cache)?;
        }
        header.write_u8(K_CODERS_UNPACK_SIZE)?;
        for block in self.blocks.iter() {
            // A BCJ2 block's call and jump coders come first in its coder
            // list, so their output sizes do too; see `write_bcj2_to`.
            if let Some(bcj2) = block.bcj2 {
                write_u64(header, bcj2.jump)?;
                write_u64(header, bcj2.call)?;
            }
            for size in block.sizes.iter().copied() {
                write_u64(header, size)?;
            }
        }
        // 7zip doesn't write CRC values in the folder section of the unpack info. Instead,
        // it writes it only in the substreams info (even for non-solid archives).
        header.write_u8(K_END)?;
        Ok(())
    }

    pub(crate) fn write_substreams<H: Write>(&self, header: &mut H) -> std::io::Result<()> {
        header.write_u8(K_SUB_STREAMS_INFO)?;

        // Only write K_NUM_UNPACK_STREAM if any folder has != 1 substream.
        let needs_num_unpack_stream = self.blocks.iter().any(|f| f.num_sub_unpack_streams != 1);

        if needs_num_unpack_stream {
            header.write_u8(K_NUM_UNPACK_STREAM)?;
            for f in &self.blocks {
                write_u64(header, f.num_sub_unpack_streams)?;
            }
        }

        // Only write K_SIZE if there are folders with > 1 substream.
        let needs_sizes = self.blocks.iter().any(|f| f.sub_stream_sizes.len() > 1);

        if needs_sizes {
            header.write_u8(K_SIZE)?;
            for f in &self.blocks {
                if f.sub_stream_sizes.len() > 1 {
                    debug_assert_eq!(f.sub_stream_sizes.len(), f.num_sub_unpack_streams as usize);

                    // Write N-1 sizes (last size is calculated).
                    for i in 0..f.sub_stream_sizes.len() - 1 {
                        let size = f.sub_stream_sizes[i];
                        write_u64(header, size)?;
                    }
                }
            }
        }

        // We always write the CRC values in the substreams info.
        let mut crcs_to_write = Vec::new();
        for f in &self.blocks {
            if f.num_sub_unpack_streams > 1 {
                // Multiple substreams - write all CRCs.
                for &crc in &f.sub_stream_crcs {
                    crcs_to_write.push(crc);
                }
            } else if f.num_sub_unpack_streams == 1 {
                // Single substream - write CRC here and not in the folder section.
                match f.sub_stream_crcs.first() {
                    None => {
                        crcs_to_write.push(f.crc);
                    }
                    Some(crc) => {
                        crcs_to_write.push(*crc);
                    }
                };
            }
        }

        if !crcs_to_write.is_empty() {
            header.write_u8(K_CRC)?;
            header.write_u8(1)?; // all CRCs defined.
            for crc in crcs_to_write {
                header.write_u32(crc)?;
            }
        }

        header.write_u8(K_END)?;
        Ok(())
    }
}

/// The output sizes of a BCJ2 block's call and jump coders, which the block's
/// method list does not name.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Bcj2SideSizes {
    pub(crate) call: u64,
    pub(crate) jump: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct BlockInfo {
    pub(crate) methods: Arc<Vec<EncoderConfiguration>>,
    pub(crate) sizes: Vec<u64>,
    pub(crate) crc: u32,
    pub(crate) num_sub_unpack_streams: u64,
    pub(crate) sub_stream_sizes: Vec<u64>,
    pub(crate) sub_stream_crcs: Vec<u32>,
    /// Set when `methods` ends in BCJ2: the block is then the four-stream
    /// folder 7-Zip writes, not a linear chain.
    pub(crate) bcj2: Option<Bcj2SideSizes>,
}

impl BlockInfo {
    pub(crate) fn write_to<W: Write>(
        &self,
        header: &mut W,
        cache: &mut Vec<u8>,
    ) -> std::io::Result<()> {
        if self.bcj2.is_some() {
            return self.write_bcj2_to(header, cache);
        }
        cache.clear();
        let mut num_coders = 0;
        for mc in self.methods.iter() {
            num_coders += 1;
            self.write_single_codec(mc, cache)?;
        }
        write_u64(header, num_coders as u64)?;
        header.write_all(cache)?;
        for i in 0..num_coders - 1 {
            write_u64(header, i as u64 + 1)?;
            write_u64(header, i as u64)?;
        }
        Ok(())
    }

    /// The folder 7-Zip writes for `BCJ2` over a main coder chain, which is
    /// what `CEncoder::SetFolder` in `CPP/7zip/Archive/7z/7zEncode.cpp` makes
    /// of the methods `AddBcj2Methods` in `7zUpdate.cpp` sets up. With `k`
    /// coders `m[0..k]` on the main stream (`m[0]` reading its pack stream,
    /// as in a linear chain) the coders are, in order:
    ///
    /// | coder   | method          | in streams        | out stream |
    /// |---------|-----------------|-------------------|------------|
    /// | 0       | LZMA, jump      | 0                 | 0          |
    /// | 1       | LZMA, call      | 1                 | 1          |
    /// | 2 + i   | `m[i]`          | 2 + i             | 2 + i      |
    /// | 2 + k   | BCJ2            | 2 + k ..= 5 + k   | 2 + k      |
    ///
    /// BCJ2's four inputs are main, call, jump and rc. The bind pairs feed
    /// main from `m[k - 1]`, call from coder 1 and jump from coder 0, and
    /// chain the main coders as a linear chain does; they are written in
    /// descending input order, as 7-Zip writes them. The four pack streams
    /// are, in file order, main (`m[0]`'s input), rc (BCJ2's last input,
    /// stored raw), call and jump.
    fn write_bcj2_to<W: Write>(&self, header: &mut W, cache: &mut Vec<u8>) -> std::io::Result<()> {
        let k = self.methods.len() as u64 - 1;
        let bcj2 = 2 + k;
        cache.clear();
        let side = encoder::bcj2_side_properties();
        for _ in 0..2 {
            let id = EncoderMethod::ID_LZMA;
            cache.write_u8(id.len() as u8 | 0x20)?;
            cache.write_all(id)?;
            cache.write_u8(side.len() as u8)?;
            cache.write_all(&side)?;
        }
        for mc in &self.methods[..k as usize] {
            self.write_single_codec(mc, cache)?;
        }
        // BCJ2 itself: a four-byte ID, no properties, and the 0x10 flag that
        // says its stream counts follow - four in, one out.
        let id = EncoderMethod::ID_BCJ2;
        cache.write_u8(id.len() as u8 | 0x10)?;
        cache.write_all(id)?;
        write_u64(cache, 4)?;
        write_u64(cache, 1)?;

        write_u64(header, k + 3)?;
        header.write_all(cache)?;

        // Bind pairs, as (in, out).
        write_u64(header, bcj2 + 2)?;
        write_u64(header, 0)?;
        write_u64(header, bcj2 + 1)?;
        write_u64(header, 1)?;
        write_u64(header, bcj2)?;
        write_u64(header, bcj2 - 1)?;
        for i in (1..k).rev() {
            write_u64(header, 2 + i)?;
            write_u64(header, 1 + i)?;
        }
        // Pack streams, in file order.
        for in_index in [2, bcj2 + 3, 1, 0] {
            write_u64(header, in_index)?;
        }
        Ok(())
    }

    fn write_single_codec<H: Write>(
        &self,
        mc: &EncoderConfiguration,
        out: &mut H,
    ) -> std::io::Result<()> {
        let id = mc.method.id();
        let mut temp = [0u8; 256];
        let props = encoder::get_options_as_properties(mc.method, mc.options.as_ref(), &mut temp);
        let mut codec_flags = id.len() as u8;
        if !props.is_empty() {
            codec_flags |= 0x20;
        }
        out.write_u8(codec_flags)?;
        out.write_all(id)?;
        if !props.is_empty() {
            out.write_u8(props.len() as u8)?;
            out.write_all(props)?;
        }
        Ok(())
    }
}
