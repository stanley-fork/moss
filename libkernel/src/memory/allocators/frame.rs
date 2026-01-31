use intrusive_collections::{LinkedListLink, UnsafeRef, intrusive_adapter};

use crate::memory::page::PageFrame;

#[derive(Clone, Copy, Debug)]
pub struct AllocatedInfo {
    /// Current ref count of the allocated block.
    pub ref_count: u32,
    /// The order of the entire allocated block.
    pub order: u8,
}

/// Holds metadata for a page that is part of an allocated block but is not the head.
/// It simply points back to the head of the block.
#[derive(Clone, Copy, Debug)]
pub struct TailInfo {
    pub head: PageFrame,
}

#[derive(Debug, Clone)]
pub enum FrameState {
    /// The frame has not yet been processed by the allocator's init function.
    Uninitialized,
    /// The frame is the head of a free block of a certain order.
    Free { order: u8 },
    /// The frame is the head of an allocated block.
    AllocatedHead(AllocatedInfo),
    /// The frame is a tail page of an allocated block.
    AllocatedTail(TailInfo),
    /// The frame is part of the kernel's own image.
    Kernel,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub state: FrameState,
    pub link: LinkedListLink, // only used in free nodes.
    pub pfn: PageFrame,
}

intrusive_adapter!(pub FrameAdapter = UnsafeRef<Frame>: Frame { link: LinkedListLink });

impl Frame {
    pub fn new(pfn: PageFrame) -> Self {
        Self {
            state: FrameState::Uninitialized,
            link: LinkedListLink::new(),
            pfn,
        }
    }
}
