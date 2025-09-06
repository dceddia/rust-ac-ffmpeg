//! Frame reordering buffer for proper presentation timestamp ordering.

use std::{cmp::Ordering, collections::BinaryHeap};

use crate::{codec::video::VideoFrame, time::Timestamp};

/// A wrapper around VideoFrame that implements reverse ordering for use in BinaryHeap.
/// BinaryHeap is a max-heap, but we want min-heap behavior (earliest PTS first).
#[derive(Clone)]
struct OrderedFrame {
    frame: VideoFrame,
}

impl OrderedFrame {
    fn new(frame: VideoFrame) -> Self {
        Self { frame }
    }

    fn pts(&self) -> Timestamp {
        self.frame.pts()
    }
}

impl PartialEq for OrderedFrame {
    fn eq(&self, other: &Self) -> bool {
        self.pts() == other.pts()
    }
}

impl Eq for OrderedFrame {}

impl PartialOrd for OrderedFrame {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedFrame {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for min-heap behavior in BinaryHeap (max-heap)
        other
            .pts()
            .partial_cmp(&self.pts())
            .unwrap_or(Ordering::Equal)
    }
}

/// A buffer for reordering video frames by presentation timestamp.
///
/// This buffer collects frames until it reaches capacity, then outputs
/// them in PTS order. This is useful for handling B-frames and other
/// out-of-order frame scenarios.
pub struct ReorderBuffer {
    heap: BinaryHeap<OrderedFrame>,
    capacity: usize,
}

impl ReorderBuffer {
    /// Create a new reorder buffer with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(capacity),
            capacity,
        }
    }

    /// Get the current capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Resize the buffer capacity.
    ///
    /// If the new capacity is smaller than the current number of frames,
    /// this will not remove any frames but future pushes may fail.
    pub fn resize(&mut self, new_capacity: usize) {
        self.capacity = new_capacity;
        if new_capacity > self.heap.capacity() {
            self.heap.reserve(new_capacity - self.heap.capacity());
        }
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Check if the buffer is full.
    pub fn is_full(&self) -> bool {
        self.heap.len() >= self.capacity
    }

    /// Get the current number of frames in the buffer.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Push a frame into the buffer.
    ///
    /// Returns `Ok(())` if successful, or `Err(frame)` if the buffer is full.
    pub fn push(&mut self, frame: VideoFrame) -> Result<(), VideoFrame> {
        if self.is_full() {
            return Err(frame);
        }

        self.heap.push(OrderedFrame::new(frame));
        Ok(())
    }

    /// Pop the frame with the earliest PTS from the buffer.
    ///
    /// Returns `None` if the buffer is empty.
    ///
    /// For reordering to work properly, check is_full() is true before popping.
    pub fn pop(&mut self) -> Option<VideoFrame> {
        self.heap.pop().map(|ordered| ordered.frame)
    }

    /// Peek at the frame with the earliest PTS without removing it.
    ///
    /// Returns `None` if the buffer is empty.
    pub fn peek(&self) -> Option<&VideoFrame> {
        self.heap.peek().map(|ordered| &ordered.frame)
    }

    /// Clear all frames from the buffer.
    pub fn clear(&mut self) {
        self.heap.clear();
    }

    /// Get all frames in PTS order, clearing the buffer.
    /// This is useful for flushing at the end of a stream.
    pub fn drain(&mut self) -> Vec<VideoFrame> {
        let mut frames = Vec::with_capacity(self.heap.len());
        while let Some(frame) = self.pop() {
            frames.push(frame);
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::TimeBase;

    fn create_test_frame(pts: i64) -> VideoFrame {
        // This is a simplified test helper - in real usage frames would be properly constructed
        unsafe {
            VideoFrame::from_raw_ptr(std::ptr::null_mut(), TimeBase::MICROSECONDS, 0.0)
                .with_pts(Timestamp::new(pts, TimeBase::MICROSECONDS))
        }
    }

    #[test]
    fn test_reorder_buffer_basic() {
        let mut buffer = ReorderBuffer::new(3);

        assert!(buffer.is_empty());
        assert!(!buffer.is_full());
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity(), 3);

        // Push frames in reverse PTS order
        buffer.push(create_test_frame(300)).unwrap();
        buffer.push(create_test_frame(100)).unwrap();
        buffer.push(create_test_frame(200)).unwrap();

        assert!(buffer.is_full());
        assert_eq!(buffer.len(), 3);

        // Should pop in PTS order (earliest first)
        assert_eq!(buffer.pop().unwrap().pts().timestamp(), 100);
        assert_eq!(buffer.pop().unwrap().pts().timestamp(), 200);
        assert_eq!(buffer.pop().unwrap().pts().timestamp(), 300);

        assert!(buffer.is_empty());
    }

    #[test]
    fn test_reorder_buffer_overflow() {
        let mut buffer = ReorderBuffer::new(2);

        buffer.push(create_test_frame(100)).unwrap();
        buffer.push(create_test_frame(200)).unwrap();

        // Buffer is full, should return the frame
        let overflow_frame = create_test_frame(150);
        let returned_frame = buffer.push(overflow_frame).unwrap_err();
        assert_eq!(returned_frame.pts().timestamp(), 150);
    }

    #[test]
    fn test_reorder_buffer_resize() {
        let mut buffer = ReorderBuffer::new(2);

        buffer.push(create_test_frame(100)).unwrap();
        buffer.push(create_test_frame(200)).unwrap();
        assert!(buffer.is_full());

        buffer.resize(4);
        assert!(!buffer.is_full());
        assert_eq!(buffer.capacity(), 4);

        buffer.push(create_test_frame(150)).unwrap();
        buffer.push(create_test_frame(250)).unwrap();
        assert!(buffer.is_full());
    }

    #[test]
    fn test_reorder_buffer_drain() {
        let mut buffer = ReorderBuffer::new(3);

        buffer.push(create_test_frame(300)).unwrap();
        buffer.push(create_test_frame(100)).unwrap();
        buffer.push(create_test_frame(200)).unwrap();

        let frames = buffer.drain();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].pts().timestamp(), 100);
        assert_eq!(frames[1].pts().timestamp(), 200);
        assert_eq!(frames[2].pts().timestamp(), 300);

        assert!(buffer.is_empty());
    }
}
