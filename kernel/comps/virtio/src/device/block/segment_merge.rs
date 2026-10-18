// SPDX-License-Identifier: MPL-2.0

//! Coalesces adjacent data DMA ranges without changing BIO ownership.

use alloc::vec::Vec;

use ostd::mm::{Daddr, HasDaddr};

use crate::dma_buf::DmaBuf;

/// Borrows a range whose source buffers remain owned by the submitted BIOs.
/// DMA synchronization still runs on every original buffer.
pub(super) struct DmaSpan<'a, T: DmaBuf> {
    first: &'a T,
    len: usize,
}

impl<'a, T: DmaBuf> DmaSpan<'a, T> {
    pub(super) fn single(buffer: &'a T) -> Self {
        Self {
            first: buffer,
            len: buffer.len(),
        }
    }
}

impl<T: DmaBuf> HasDaddr for DmaSpan<'_, T> {
    fn daddr(&self) -> Daddr {
        self.first.daddr()
    }
}

impl<T: DmaBuf> DmaBuf for DmaSpan<'_, T> {
    fn len(&self) -> usize {
        self.len
    }
}

/// Appends a data buffer, joining only an adjacent device-address range.
/// Callers keep headers/status outside this list and retain all source BIOs
/// through completion, including buffers covered by an extended span.
pub(super) fn append_data<'a, T: DmaBuf>(
    spans: &mut Vec<DmaSpan<'a, T>>,
    buffer: &'a T,
    limit: usize,
) {
    if let Some(last) = spans.last_mut() {
        let end = last.daddr().checked_add(last.len);
        let length = last.len.checked_add(buffer.len());
        if end == Some(buffer.daddr())
            && let Some(length) = length
            && length <= limit
            && length < u32::MAX as usize
        {
            last.len = length;
            return;
        }
    }
    spans.push(DmaSpan::single(buffer));
}

#[cfg(ktest)]
mod tests {
    use alloc::sync::Arc;

    use aster_util::mem_obj_slice::Slice;
    use ostd::{mm::dma::DmaStream, prelude::ktest};

    use super::*;

    #[ktest]
    fn dma_segment_merge_preserves_order_gaps_and_limits() {
        let dma: Arc<DmaStream> = Arc::new(DmaStream::alloc(32, false).unwrap());
        let pages: Vec<_> = (0..32)
            .map(|page| Slice::new(dma.clone(), page * 4096..(page + 1) * 4096))
            .collect();
        let mut spans = Vec::new();
        for page in &pages {
            append_data(&mut spans, page, u32::MAX as usize - 1);
        }
        assert_eq!(
            spans.len(),
            1,
            "128 KiB of consecutive DMA pages must use one data descriptor"
        );
        assert_eq!(spans[0].len(), 128 * 1024);
        assert_eq!(spans[0].daddr(), pages[0].daddr());

        let mut spans = Vec::new();
        for index in [0, 2, 3, 1, 1] {
            append_data(&mut spans, &pages[index], 8192);
        }
        assert_eq!(
            spans.len(),
            4,
            "gaps, reverse order and aliasing must not be merged"
        );
        assert_eq!(spans[1].daddr(), pages[2].daddr());
        assert_eq!(spans[1].len(), 8192);
        assert_eq!(spans[2].daddr(), pages[1].daddr());
        assert_eq!(spans[3].daddr(), pages[1].daddr());

        let mut spans = Vec::new();
        for page in &pages[..3] {
            append_data(&mut spans, page, 8192);
        }
        assert_eq!(
            spans.len(),
            2,
            "a descriptor must not exceed its length limit"
        );
        assert_eq!(spans[0].len(), 8192);
        assert_eq!(spans[1].len(), 4096);
    }
}
