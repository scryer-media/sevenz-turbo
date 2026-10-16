use std::{
    io::Write,
    sync::{Arc, Weak},
};

use super::*;
use crate::EncoderConfiguration;
use crate::block::SmallSlice;
#[derive(Debug, Clone, Default)]
pub(crate) struct UnpackInfo {
    pub(crate) blocks: Vec<BlockInfo>,
    last_source: Weak<Vec<EncoderConfiguration>>,
    last_methods: Arc<[HeaderMethod]>,
    last_header: Arc<[u8]>,
}

impl UnpackInfo {
    fn header_methods(&mut self, methods: &Arc<Vec<EncoderConfiguration>>) -> Arc<[u8]> {
        if self.last_source.as_ptr() == Arc::as_ptr(methods) {
            return Arc::clone(&self.last_header);
        }
        // Different folder sizes can serialize to the same dictionary size.
        // Share those descriptors too; encrypted folders' fresh IVs compare
        // unequal and keep their own properties.
        if methods.len() == self.last_methods.len()
            && methods
                .iter()
                .zip(self.last_methods.iter())
                .all(|(source, stored)| stored.matches(source))
        {
            self.last_source = Arc::downgrade(methods);
            return Arc::clone(&self.last_header);
        }
        // Finished folders need only decoder properties. Do not retain the
        // encoder settings, passwords or key-cache handles until finish.
        let stored: Arc<[HeaderMethod]> = methods.iter().map(HeaderMethod::from).collect();
        let mut header = Vec::new();
        HeaderMethod::write_folder(&stored, &mut header)
            .expect("writing a folder descriptor to Vec cannot fail");
        self.last_source = Arc::downgrade(methods);
        self.last_methods = stored;
        self.last_header = header.into();
        Arc::clone(&self.last_header)
    }

    pub(crate) fn add(
        &mut self,
        methods: Arc<Vec<EncoderConfiguration>>,
        sizes: Vec<u64>,
        crc: u32,
    ) -> &mut BlockInfo {
        let methods = self.header_methods(&methods);
        self.blocks.push(BlockInfo {
            methods,
            sizes: sizes.into(),
            crc,
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
        let methods = self.header_methods(&methods);
        self.blocks.push(BlockInfo {
            methods,
            sizes: sizes.into(),
            crc,
            extra: Some(Box::new(BlockExtra {
                num_sub_unpack_streams,
                sub_stream_crcs: sub_stream_crcs.into_boxed_slice(),
                sub_stream_sizes: sub_stream_sizes.into_boxed_slice(),
                bcj2: None,
            })),
        });
        self.blocks.last_mut().expect("just pushed")
    }

    /// How many pack streams the recorded blocks read: four for a BCJ2 block
    /// (main, rc, call, jump), one for any other.
    pub(crate) fn pack_streams(&self) -> usize {
        self.blocks
            .iter()
            .map(|block| {
                if block
                    .extra
                    .as_ref()
                    .is_some_and(|extra| extra.bcj2.is_some())
                {
                    4
                } else {
                    1
                }
            })
            .sum()
    }

    pub(crate) fn write_to<H: Write>(&mut self, header: &mut H) -> std::io::Result<()> {
        header.write_u8(K_UNPACK_INFO)?;
        header.write_u8(K_FOLDER)?;
        write_u64(header, self.blocks.len() as u64)?;
        header.write_u8(0)?;
        for block in self.blocks.iter() {
            header.write_all(&block.methods)?;
        }
        header.write_u8(K_CODERS_UNPACK_SIZE)?;
        for block in self.blocks.iter() {
            // A BCJ2 block's call and jump coders come first in its coder
            // list, so their output sizes do too; see `write_bcj2_to`.
            if let Some(bcj2) = block.extra.as_ref().and_then(|extra| extra.bcj2) {
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
        let needs_num_unpack_stream = self.blocks.iter().any(|f| f.num_sub_unpack_streams() != 1);

        if needs_num_unpack_stream {
            header.write_u8(K_NUM_UNPACK_STREAM)?;
            for f in &self.blocks {
                write_u64(header, f.num_sub_unpack_streams())?;
            }
        }

        // Only write K_SIZE if there are folders with > 1 substream.
        let needs_sizes = self.blocks.iter().any(|f| f.sub_stream_sizes().len() > 1);

        if needs_sizes {
            header.write_u8(K_SIZE)?;
            for f in &self.blocks {
                let sizes = f.sub_stream_sizes();
                if sizes.len() > 1 {
                    debug_assert_eq!(sizes.len(), f.num_sub_unpack_streams() as usize);

                    // Write N-1 sizes (last size is calculated).
                    for &size in &sizes[..sizes.len() - 1] {
                        write_u64(header, size)?;
                    }
                }
            }
        }

        // Write the existing CRCs directly instead of retaining a second
        // array for every file while serializing the archive header.
        if self.blocks.iter().any(|f| {
            f.num_sub_unpack_streams() == 1
                || (f.num_sub_unpack_streams() > 1 && !f.sub_stream_crcs().is_empty())
        }) {
            header.write_u8(K_CRC)?;
            header.write_u8(1)?; // all CRCs defined.
            for f in &self.blocks {
                if f.num_sub_unpack_streams() > 1 {
                    for &crc in f.sub_stream_crcs() {
                        header.write_u32(crc)?;
                    }
                } else if f.num_sub_unpack_streams() == 1 {
                    header.write_u32(f.sub_stream_crcs().first().copied().unwrap_or(f.crc))?;
                }
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
    /// Serialized coder properties and bindings; no encoder options or
    /// per-method allocations survive with a completed folder.
    methods: Arc<[u8]>,
    pub(crate) sizes: SmallSlice<u64, 1>,
    pub(crate) crc: u32,
    // Ordinary non-solid folders need neither sub-stream tables nor BCJ2
    // side sizes. Keep those fields only for the folders that use them.
    extra: Option<Box<BlockExtra>>,
}

#[derive(Debug, Clone)]
struct BlockExtra {
    num_sub_unpack_streams: u64,
    sub_stream_sizes: Box<[u64]>,
    sub_stream_crcs: Box<[u32]>,
    bcj2: Option<Bcj2SideSizes>,
}

impl BlockInfo {
    fn num_sub_unpack_streams(&self) -> u64 {
        self.extra
            .as_ref()
            .map_or(1, |extra| extra.num_sub_unpack_streams)
    }

    fn sub_stream_sizes(&self) -> &[u64] {
        self.extra
            .as_ref()
            .map_or(&[], |extra| &extra.sub_stream_sizes)
    }

    fn sub_stream_crcs(&self) -> &[u32] {
        self.extra
            .as_ref()
            .map_or(&[], |extra| &extra.sub_stream_crcs)
    }

    pub(crate) fn set_bcj2(&mut self, sizes: Option<Bcj2SideSizes>) {
        if sizes.is_some() {
            self.extra
                .get_or_insert_with(|| {
                    Box::new(BlockExtra {
                        num_sub_unpack_streams: 1,
                        sub_stream_sizes: Box::default(),
                        sub_stream_crcs: Box::default(),
                        bcj2: None,
                    })
                })
                .bcj2 = sizes;
        } else if let Some(extra) = &mut self.extra {
            extra.bcj2 = None;
        }
    }
}

#[derive(Debug)]
struct HeaderMethod {
    id: &'static [u8],
    properties: SmallSlice<u8, 15>,
}

impl HeaderMethod {
    fn matches(&self, method: &EncoderConfiguration) -> bool {
        if self.id != method.method.id() {
            return false;
        }
        let mut buffer = [0; 256];
        self.properties[..]
            == *encoder::get_options_as_properties(
                method.method,
                method.options.as_ref(),
                &mut buffer,
            )
    }
}

impl From<&EncoderConfiguration> for HeaderMethod {
    fn from(method: &EncoderConfiguration) -> Self {
        let mut buffer = [0; 256];
        let bytes =
            encoder::get_options_as_properties(method.method, method.options.as_ref(), &mut buffer);
        let mut properties = SmallSlice::with_len(bytes.len());
        properties.copy_from_slice(bytes);
        Self {
            id: method.method.id(),
            properties,
        }
    }
}

impl HeaderMethod {
    fn write_folder<W: Write>(methods: &[Self], header: &mut W) -> std::io::Result<()> {
        if methods
            .last()
            .is_some_and(|m| m.id == EncoderMethod::ID_BCJ2)
        {
            return Self::write_bcj2_to(methods, header);
        }
        let num_coders = methods.len();
        write_u64(header, num_coders as u64)?;
        for mc in methods {
            mc.write_single_codec(header)?;
        }
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
    fn write_bcj2_to<W: Write>(methods: &[Self], header: &mut W) -> std::io::Result<()> {
        let k = methods.len() as u64 - 1;
        let bcj2 = 2 + k;
        write_u64(header, k + 3)?;
        let side = encoder::bcj2_side_properties();
        for _ in 0..2 {
            let id = EncoderMethod::ID_LZMA;
            header.write_u8(id.len() as u8 | 0x20)?;
            header.write_all(id)?;
            header.write_u8(side.len() as u8)?;
            header.write_all(&side)?;
        }
        for mc in &methods[..k as usize] {
            mc.write_single_codec(header)?;
        }
        // BCJ2 itself: a four-byte ID, no properties, and the 0x10 flag that
        // says its stream counts follow - four in, one out.
        let id = EncoderMethod::ID_BCJ2;
        header.write_u8(id.len() as u8 | 0x10)?;
        header.write_all(id)?;
        write_u64(header, 4)?;
        write_u64(header, 1)?;

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

    fn write_single_codec<H: Write>(&self, out: &mut H) -> std::io::Result<()> {
        let id = self.id;
        let props = &self.properties[..];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_folders_keep_properties_without_retaining_encoder_options() {
        let methods = Arc::new(vec![EncoderConfiguration::new(EncoderMethod::LZMA2)]);
        let source = Arc::downgrade(&methods);
        let mut info = UnpackInfo::default();
        info.add(Arc::clone(&methods), vec![100], 7);
        info.add(Arc::clone(&methods), vec![200], 8);
        assert!(Arc::ptr_eq(
            &info.blocks[0].methods,
            &info.blocks[1].methods
        ));
        assert_eq!(Arc::strong_count(&methods), 1);
        let mut before = Vec::new();
        info.write_to(&mut before).unwrap();
        drop(methods);
        assert!(source.upgrade().is_none());
        let mut after = Vec::new();
        info.write_to(&mut after).unwrap();
        assert_eq!(before, after);
    }
}
