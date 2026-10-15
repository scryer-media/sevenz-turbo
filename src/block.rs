// Small, fixed header fields need no allocation. Larger coder properties
// and multi-stream tables retain their complete, bounded contents on the heap.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SmallSlice<T, const N: usize> {
    #[cfg(target_pointer_width = "64")]
    Inline {
        len: u8,
        values: [T; N],
    },
    Heap(Box<[T]>),
}

impl<T: Default + Copy, const N: usize> SmallSlice<T, N> {
    pub(crate) fn with_len(len: usize) -> Self {
        // Inline storage fits a Vec-sized field on 64-bit targets. Keep the
        // boxed representation on narrower targets, where that tail would
        // enlarge every folder rather than save space.
        #[cfg(target_pointer_width = "64")]
        if len <= N {
            return Self::Inline {
                len: u8::try_from(len).expect("small inline capacity"),
                values: [T::default(); N],
            };
        }
        Self::Heap(vec![T::default(); len].into_boxed_slice())
    }
}

impl<T: Default + Copy, const N: usize> Default for SmallSlice<T, N> {
    fn default() -> Self {
        Self::with_len(0)
    }
}

impl<T: Default + Copy, const N: usize> From<Vec<T>> for SmallSlice<T, N> {
    fn from(values: Vec<T>) -> Self {
        #[cfg(target_pointer_width = "64")]
        if values.len() <= N {
            let mut result = Self::with_len(values.len());
            result.copy_from_slice(&values);
            return result;
        }
        Self::Heap(values.into_boxed_slice())
    }
}

impl<T, const N: usize> std::ops::Deref for SmallSlice<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match self {
            #[cfg(target_pointer_width = "64")]
            Self::Inline { len, values } => &values[..usize::from(*len)],
            Self::Heap(values) => values,
        }
    }
}

impl<T, const N: usize> std::ops::DerefMut for SmallSlice<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            #[cfg(target_pointer_width = "64")]
            Self::Inline { len, values } => &mut values[..usize::from(*len)],
            Self::Heap(values) => values,
        }
    }
}

impl<T: std::fmt::Debug, const N: usize> std::fmt::Debug for SmallSlice<T, N> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self[..].fmt(formatter)
    }
}

#[cfg(test)]
mod storage_tests {
    use super::{Block, Coder, SmallSlice};

    #[test]
    fn packed_coders_preserve_every_wire_field_across_storage_transitions() {
        assert_eq!(size_of::<Coder>(), 24);
        for id_len in 0..=15 {
            for prop_len in [0, 1, 5, 7, 8, 15, 127, 128, 4096] {
                for counts in [[0, 0], [1, 1], [4, 1], [u64::MAX, u64::MAX]] {
                    let id: Vec<u8> = (0..id_len).map(|i| 255 - i).collect();
                    let props: Vec<u8> = (0..prop_len).map(|i| i as u8).collect();
                    let mut coder = Coder::single_stream();
                    coder.set_id_size(id_len);
                    coder.decompression_method_id_mut().copy_from_slice(&id);
                    coder.set_properties(props.clone().into());
                    coder.set_stream_counts(counts[0], counts[1]);
                    assert_eq!(coder.encoder_method_id(), id);
                    assert_eq!(coder.properties(), props);
                    assert_eq!(coder.counts(), counts);
                    let saved = coder.clone();
                    // Logical equality must not depend on whether storage was
                    // promoted by an earlier exceptional graph or long property.
                    coder.promote();
                    assert_eq!(coder, saved);
                    coder.compact();
                    assert_eq!(coder, saved);
                    if let Some(byte) = coder.properties_mut().first_mut() {
                        *byte ^= 0xff;
                    }
                    if let Some(byte) = coder.decompression_method_id_mut().first_mut() {
                        *byte ^= 0xff;
                    }
                    assert_eq!(saved.encoder_method_id(), id);
                    assert_eq!(saved.properties(), props);
                    coder.set_properties(vec![17; 7].into());
                    coder.set_stream_counts(1, 1);
                    assert!(matches!(coder.storage, super::CoderStorage::Inline { .. }));
                    assert_eq!(coder.properties(), [17; 7]);
                    assert_eq!(coder.counts(), [1, 1]);
                }
            }
        }
    }

    #[test]
    fn coder_counts_preserve_defaults_full_width_and_clone_independence() {
        let default = Coder::default();
        assert_eq!(
            (default.num_in_streams(), default.num_out_streams()),
            (0, 0)
        );
        let mut coder = Coder::single_stream();
        assert_eq!((coder.num_in_streams(), coder.num_out_streams()), (1, 1));
        assert!(matches!(coder.storage, super::CoderStorage::Inline { .. }));
        for pair in [[0, 1], [4, 1], [u64::MAX, 0], [0, u64::MAX]] {
            coder.set_stream_counts(pair[0], pair[1]);
            let mut cloned = coder.clone();
            cloned.set_stream_counts(1, 1);
            assert_eq!(
                (coder.num_in_streams(), coder.num_out_streams()),
                (pair[0], pair[1])
            );
            assert_eq!((cloned.num_in_streams(), cloned.num_out_streams()), (1, 1));
            assert_ne!(coder, cloned);
        }
    }

    #[test]
    fn compact_chain_metadata_matches_general_graph_in_both_directions() {
        use super::{BindPair, BlockStreams, StreamGraph};
        for (input, output, packed) in [(1, 0, 0), (0, 1, 1)] {
            let coders = vec![Coder::single_stream(); 2];
            for sizes in [vec![], vec![0, u64::MAX], vec![u64::MAX, 17]] {
                let binds = vec![BindPair {
                    in_index: input,
                    out_index: output,
                }]
                .into_boxed_slice();
                let mut compact = Block::with_coders(coders.clone());
                compact.set_streams(
                    2,
                    2,
                    binds.clone(),
                    vec![packed].into(),
                    sizes.clone().into(),
                );
                assert!(matches!(
                    compact.streams,
                    BlockStreams::PairForward(_) | BlockStreams::PairReverse(_)
                ));
                let mut general = Block::with_coders(coders.clone());
                general.streams = BlockStreams::Graph(Box::new(StreamGraph {
                    total_input_streams: 2,
                    total_output_streams: 2,
                    bind_pairs: binds,
                    packed_streams: vec![packed].into(),
                    unpack_sizes: sizes.clone().into(),
                }));
                if sizes.is_empty() {
                    compact.prepare_unpack_sizes();
                    general.prepare_unpack_sizes();
                }
                assert_eq!(compact.total_input_streams(), general.total_input_streams());
                assert_eq!(
                    compact.total_output_streams(),
                    general.total_output_streams()
                );
                assert_eq!(compact.bind_pairs(), general.bind_pairs());
                assert_eq!(compact.packed_streams(), general.packed_streams());
                assert_eq!(compact.unpack_sizes(), general.unpack_sizes());
                assert_eq!(compact.get_unpack_size(), general.get_unpack_size());
                let order = |b: &Block| b.ordered_coder_iter().map(|(i, _)| i).collect::<Vec<_>>();
                assert_eq!(order(&compact), order(&general));
                let mut cloned = compact.clone();
                cloned.unpack_sizes_mut()[0] ^= 1;
                assert_eq!(compact.unpack_sizes(), general.unpack_sizes());
                assert_ne!(cloned.unpack_sizes(), compact.unpack_sizes());
            }
        }
    }

    #[test]
    fn single_stream_metadata_preserves_full_sizes_and_clone_independence() {
        let empty = Block::default();
        assert_eq!(empty.get_unpack_size(), 0);
        assert_eq!(empty.get_unpack_size_at_index(0), 0);
        assert_eq!(empty.ordered_coder_iter().count(), 0);
        let mut block = Block::with_coders(vec![Coder::single_stream()]);
        block.set_streams(1, 1, Box::default(), vec![0].into(), vec![u64::MAX].into());
        let mut cloned = block.clone();
        cloned.unpack_sizes_mut()[0] = 17;
        assert_eq!(block.get_unpack_size(), u64::MAX);
        assert_eq!(block.get_unpack_size_for_coder(&block.coders[0]), u64::MAX);
        assert_eq!(block.get_unpack_size_at_index(1), 0);
        assert_eq!(cloned.get_unpack_size(), 17);
        assert_eq!(cloned.ordered_coder_iter().count(), 1);
        assert_eq!(cloned.packed_streams(), &[0]);
    }

    #[test]
    fn small_header_fields_preserve_slice_values_and_clone_independence() {
        assert!(size_of::<SmallSlice<u8, 15>>() <= size_of::<Vec<u8>>());
        assert!(size_of::<SmallSlice<u64, 1>>() <= size_of::<Vec<u64>>());
        for len in [0, 1, 2, 15, 16, 255, 256, 4096] {
            let expected: Vec<_> = (0..len).map(|i| i as u8).collect();
            let mut field: SmallSlice<u8, 15> = expected.clone().into();
            let cloned = field.clone();
            assert_eq!(format!("{field:?}"), format!("{expected:?}"));
            for byte in field.iter_mut() {
                *byte = byte.wrapping_add(1);
            }
            assert_eq!(&cloned[..], expected);
            assert_eq!(
                &field[..],
                expected
                    .iter()
                    .map(|b| b.wrapping_add(1))
                    .collect::<Vec<_>>()
            );

            let mut sizes = SmallSlice::<u64, 1>::with_len(len);
            for (i, value) in sizes.iter_mut().enumerate() {
                *value = u64::MAX - i as u64;
            }
            let expected: Vec<_> = (0..len).map(|i| u64::MAX - i as u64).collect();
            assert_eq!(&sizes[..], expected);
            assert_eq!(sizes, expected.into());
        }
    }
}

/// Represents a compression block.
///
/// A block contains one or more coders (compression/filter methods) that are chained
/// together to process data.
#[derive(Debug, Default, Clone)]
pub struct Block {
    /// Coders (compression/filter methods) in this block.
    pub coders: Vec<Coder>,
    /// Whether this block has a CRC checksum.
    pub has_crc: bool,
    /// CRC32 checksum of the block data.
    pub crc: u64,
    streams: BlockStreams,
    pub(crate) num_unpack_sub_streams: usize,
}

/// A one-input, one-output folder has no graph edges and its packed input
/// is always zero. A two-coder chain has one of two fixed edge directions.
/// Store only their output sizes; retain full tables for every other graph.
#[derive(Debug, Default, Clone)]
enum BlockStreams {
    #[default]
    Empty,
    Single(u64),
    PairForward(Box<[u64; 2]>),
    PairReverse(Box<[u64; 2]>),
    Graph(Box<StreamGraph>),
}

#[derive(Debug, Clone)]
struct StreamGraph {
    total_input_streams: usize,
    total_output_streams: usize,
    bind_pairs: Box<[BindPair]>,
    packed_streams: SmallSlice<u64, 1>,
    unpack_sizes: SmallSlice<u64, 1>,
}

impl Block {
    pub(crate) fn with_coders(coders: Vec<Coder>) -> Self {
        Self {
            coders,
            ..Self::default()
        }
    }

    pub(crate) fn set_streams(
        &mut self,
        total_input_streams: usize,
        total_output_streams: usize,
        bind_pairs: Box<[BindPair]>,
        packed_streams: SmallSlice<u64, 1>,
        unpack_sizes: SmallSlice<u64, 1>,
    ) {
        // Preserve arbitrary graphs verbatim. These two chain forms encode
        // exactly the edges and packed input recorded by the header, including
        // both the SDK's coder order and the writer's opposite order.
        let pair = total_input_streams == 2
            && total_output_streams == 2
            && bind_pairs.len() == 1
            && (unpack_sizes.is_empty() || unpack_sizes.len() == 2);
        let pair_sizes = || {
            Box::new([
                unpack_sizes.first().copied().unwrap_or(0),
                unpack_sizes.get(1).copied().unwrap_or(0),
            ])
        };
        self.streams = if total_input_streams == 1
            && total_output_streams == 1
            && bind_pairs.is_empty()
            && packed_streams[..] == [0]
            && unpack_sizes.len() <= 1
        {
            BlockStreams::Single(unpack_sizes.first().copied().unwrap_or(0))
        } else if pair
            && bind_pairs[0].in_index == 1
            && bind_pairs[0].out_index == 0
            && packed_streams[..] == [0]
        {
            BlockStreams::PairForward(pair_sizes())
        } else if pair
            && bind_pairs[0].in_index == 0
            && bind_pairs[0].out_index == 1
            && packed_streams[..] == [1]
        {
            BlockStreams::PairReverse(pair_sizes())
        } else {
            BlockStreams::Graph(Box::new(StreamGraph {
                total_input_streams,
                total_output_streams,
                bind_pairs,
                packed_streams,
                unpack_sizes,
            }))
        };
    }

    pub(crate) fn total_input_streams(&self) -> usize {
        match &self.streams {
            BlockStreams::Empty => 0,
            BlockStreams::Single(_) => 1,
            BlockStreams::PairForward(_) | BlockStreams::PairReverse(_) => 2,
            BlockStreams::Graph(graph) => graph.total_input_streams,
        }
    }

    pub(crate) fn total_output_streams(&self) -> usize {
        match &self.streams {
            BlockStreams::Empty => 0,
            BlockStreams::Single(_) => 1,
            BlockStreams::PairForward(_) | BlockStreams::PairReverse(_) => 2,
            BlockStreams::Graph(graph) => graph.total_output_streams,
        }
    }

    pub(crate) fn bind_pairs(&self) -> &[BindPair] {
        match &self.streams {
            BlockStreams::PairForward(_) => &[BindPair {
                in_index: 1,
                out_index: 0,
            }],
            BlockStreams::PairReverse(_) => &[BindPair {
                in_index: 0,
                out_index: 1,
            }],
            BlockStreams::Graph(graph) => &graph.bind_pairs,
            _ => &[],
        }
    }

    pub(crate) fn packed_streams(&self) -> &[u64] {
        match &self.streams {
            BlockStreams::Empty => &[],
            BlockStreams::Single(_) | BlockStreams::PairForward(_) => &[0],
            BlockStreams::PairReverse(_) => &[1],
            BlockStreams::Graph(graph) => &graph.packed_streams,
        }
    }

    pub(crate) fn unpack_sizes(&self) -> &[u64] {
        match &self.streams {
            BlockStreams::Empty => &[],
            BlockStreams::Single(size) => std::slice::from_ref(size),
            BlockStreams::PairForward(sizes) | BlockStreams::PairReverse(sizes) => &sizes[..],
            BlockStreams::Graph(graph) => &graph.unpack_sizes,
        }
    }

    pub(crate) fn unpack_sizes_mut(&mut self) -> &mut [u64] {
        match &mut self.streams {
            BlockStreams::Empty => &mut [],
            BlockStreams::Single(size) => std::slice::from_mut(size),
            BlockStreams::PairForward(sizes) | BlockStreams::PairReverse(sizes) => &mut sizes[..],
            BlockStreams::Graph(graph) => &mut graph.unpack_sizes,
        }
    }

    /// Called only after read_block has bounded the output stream count.
    pub(crate) fn prepare_unpack_sizes(&mut self) {
        if let BlockStreams::Graph(graph) = &mut self.streams {
            graph.unpack_sizes = SmallSlice::with_len(graph.total_output_streams);
        }
    }

    pub(crate) fn find_bind_pair_for_in_stream(&self, index: u64) -> Option<&BindPair> {
        self.bind_pairs().iter().find(|bp| bp.in_index == index)
    }

    pub(crate) fn find_bind_pair_for_out_stream(&self, index: u64) -> Option<&BindPair> {
        self.bind_pairs().iter().find(|bp| bp.out_index == index)
    }

    /// Returns the total uncompressed size of data in this block.
    pub fn get_unpack_size(&self) -> u64 {
        for i in (0..self.total_output_streams()).rev() {
            if self.find_bind_pair_for_out_stream(i as u64).is_none() {
                return self.unpack_sizes()[i];
            }
        }
        0
    }

    /// Returns the uncompressed size for a specific coder within this block.
    ///
    /// # Arguments
    /// * `coder` - The coder to get the unpack size for
    pub fn get_unpack_size_for_coder(&self, coder: &Coder) -> u64 {
        for i in 0..self.coders.len() {
            if std::ptr::eq(&self.coders[i], coder) {
                return self.unpack_sizes()[i];
            }
        }
        0
    }

    /// Returns the uncompressed size for the coder at the specified index.
    ///
    /// # Arguments
    /// * `index` - The index of the coder to get the unpack size for
    pub fn get_unpack_size_at_index(&self, index: usize) -> u64 {
        self.unpack_sizes().get(index).cloned().unwrap_or_default()
    }

    /// Returns an iterator over the coders in their processing order.
    ///
    /// Coders are chained together in blocks, and this iterator follows the chain
    /// from the first coder to the last in their proper execution order.
    pub fn ordered_coder_iter(&self) -> OrderedCoderIter<'_> {
        OrderedCoderIter::new(self)
    }

    /// Whether any coder of this block decrypts.
    ///
    /// The one case in which a decode failure may be the wrong password
    /// rather than damage. It is read from the block's own coders and not
    /// from whether the caller supplied a password: a consumer that hands
    /// every archive of a job the job's password must still be told that an
    /// unencrypted block is damaged.
    pub(crate) fn is_encrypted(&self) -> bool {
        self.coders.iter().any(|coder| {
            coder.encoder_method_id() == crate::archive::EncoderMethod::ID_AES256_SHA256
        })
    }
}

/// Represents a single coder within a compression block.
///
/// A coder defines a specific compression method, filter, or encryption method
/// used to process data within a block.
#[derive(Debug, Clone)]
pub struct Coder {
    storage: CoderStorage,
}

// C: SzGetNextFolderItem reconstructs compact CSzCoderInfo records from
// packed headers. Keep the common record compact without adopting the SDK's
// narrower method-ID, property-size or stream-count restrictions.
#[derive(Debug, Clone)]
#[repr(u8)]
enum CoderStorage {
    Inline {
        id: [u8; 15],
        properties: [u8; 7],
        // Low four bits: ID length. Next three: property length.
        // High bit: default 0/0 counts; otherwise the common 1/1 counts.
        descriptor: u8,
    },
    Full(Box<FullCoder>),
}

#[derive(Debug, Clone)]
struct FullCoder {
    id: [u8; 15],
    id_size: u8,
    counts: [u64; 2],
    properties: SmallSlice<u8, 7>,
}

impl PartialEq for Coder {
    fn eq(&self, other: &Self) -> bool {
        self.encoder_method_id() == other.encoder_method_id()
            && self.properties() == other.properties()
            && self.counts() == other.counts()
    }
}

impl Eq for Coder {}

impl Default for Coder {
    fn default() -> Self {
        let mut coder = Self::single_stream();
        coder.set_stream_counts(0, 0);
        coder
    }
}

impl Coder {
    pub(crate) fn single_stream() -> Self {
        Self {
            storage: CoderStorage::Inline {
                id: [0; 15],
                properties: [0; 7],
                descriptor: 0,
            },
        }
    }

    fn counts(&self) -> [u64; 2] {
        match &self.storage {
            CoderStorage::Inline { descriptor, .. } => [u64::from(descriptor & 0x80 == 0); 2],
            CoderStorage::Full(full) => full.counts,
        }
    }

    pub(crate) fn num_in_streams(&self) -> u64 {
        self.counts()[0]
    }

    pub(crate) fn num_out_streams(&self) -> u64 {
        self.counts()[1]
    }

    fn promote(&mut self) -> &mut FullCoder {
        if let CoderStorage::Inline {
            id,
            properties,
            descriptor,
        } = &self.storage
        {
            let len = usize::from((descriptor >> 4) & 7);
            let mut props = SmallSlice::with_len(len);
            props.copy_from_slice(&properties[..len]);
            self.storage = CoderStorage::Full(Box::new(FullCoder {
                id: *id,
                id_size: descriptor & 15,
                counts: [u64::from(descriptor & 0x80 == 0); 2],
                properties: props,
            }));
        }
        match &mut self.storage {
            CoderStorage::Full(full) => full,
            CoderStorage::Inline { .. } => unreachable!("promoted coder"),
        }
    }

    fn compact(&mut self) {
        if let CoderStorage::Full(full) = &self.storage
            && full.properties.len() <= 7
            && matches!(full.counts, [0, 0] | [1, 1])
        {
            let mut properties = [0; 7];
            properties[..full.properties.len()].copy_from_slice(&full.properties);
            self.storage = CoderStorage::Inline {
                id: full.id,
                properties,
                descriptor: full.id_size
                    | ((full.properties.len() as u8) << 4)
                    | if full.counts == [0, 0] { 0x80 } else { 0 },
            };
        }
    }

    pub(crate) fn set_stream_counts(&mut self, input: u64, output: u64) {
        if let CoderStorage::Inline { descriptor, .. } = &mut self.storage
            && matches!((input, output), (0, 0) | (1, 1))
        {
            *descriptor = (*descriptor & 0x7f) | if input == 0 { 0x80 } else { 0 };
            return;
        }
        self.promote().counts = [input, output];
        self.compact();
    }

    pub(crate) fn set_id_size(&mut self, size: u8) {
        assert!(size <= 15, "method ID exceeds wire width");
        match &mut self.storage {
            CoderStorage::Inline { descriptor, .. } => *descriptor = (*descriptor & 0xf0) | size,
            CoderStorage::Full(full) => full.id_size = size,
        }
    }

    pub(crate) fn set_properties(&mut self, props: SmallSlice<u8, 7>) {
        if let CoderStorage::Inline {
            properties,
            descriptor,
            ..
        } = &mut self.storage
            && props.len() <= 7
        {
            properties.fill(0);
            properties[..props.len()].copy_from_slice(&props);
            *descriptor = (*descriptor & 0x8f) | ((props.len() as u8) << 4);
            return;
        }
        self.promote().properties = props;
        self.compact();
    }

    /// Returns the encoder method ID for this coder.
    pub fn encoder_method_id(&self) -> &[u8] {
        match &self.storage {
            CoderStorage::Inline { id, descriptor, .. } => &id[..usize::from(descriptor & 15)],
            CoderStorage::Full(full) => &full.id[..usize::from(full.id_size)],
        }
    }

    /// Returns this coder's method-specific properties, empty for a method
    /// that takes none. See [`encoder_method_id`](Self::encoder_method_id).
    pub fn properties(&self) -> &[u8] {
        match &self.storage {
            CoderStorage::Inline {
                properties,
                descriptor,
                ..
            } => &properties[..usize::from((descriptor >> 4) & 7)],
            CoderStorage::Full(full) => &full.properties,
        }
    }

    #[cfg(test)]
    pub(crate) fn properties_mut(&mut self) -> &mut [u8] {
        match &mut self.storage {
            CoderStorage::Inline {
                properties,
                descriptor,
                ..
            } => &mut properties[..usize::from((*descriptor >> 4) & 7)],
            CoderStorage::Full(full) => &mut full.properties,
        }
    }

    pub(crate) fn decompression_method_id_mut(&mut self) -> &mut [u8] {
        match &mut self.storage {
            CoderStorage::Inline { id, descriptor, .. } => &mut id[..usize::from(*descriptor & 15)],
            CoderStorage::Full(full) => &mut full.id[..usize::from(full.id_size)],
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BindPair {
    pub(crate) in_index: u64,
    pub(crate) out_index: u64,
}

/// Iterator that yields coders in their processing order within a block.
///
/// Coders are chained together in blocks, and this iterator follows the chain
/// from the first coder to the last in their proper execution order.
pub struct OrderedCoderIter<'a> {
    block: &'a Block,
    current: Option<u64>,
    remaining: usize,
}

impl<'a> OrderedCoderIter<'a> {
    fn new(block: &'a Block) -> Self {
        let current = block.packed_streams().first().copied();
        Self {
            block,
            current,
            remaining: block.coders.len(),
        }
    }
}

impl<'a> Iterator for OrderedCoderIter<'a> {
    type Item = (usize, &'a Coder);

    fn next(&mut self) -> Option<Self::Item> {
        // An acyclic coder chain visits each coder at most once, so more than
        // `coders.len()` yields means the bind pairs form a cycle. Stop instead of
        // yielding forever (which would nest decoders until memory is exhausted); the
        // caller then fails cleanly when the truncated decode stack produces bad data.
        if self.remaining == 0 {
            return None;
        }
        let i = self.current?;
        self.current = self
            .block
            .find_bind_pair_for_out_stream(i)
            .map(|bp| bp.in_index);
        let item = self
            .block
            .coders
            .get(i as usize)
            .map(|item| (i as usize, item));
        if item.is_some() {
            self.remaining -= 1;
        }
        item
    }
}
