//! Frame reordering buffer for proper presentation timestamp ordering.
//!
//! Uses a simple Vec since the buffer is typically 2-3 frames.

use crate::{codec::video::VideoFrame, time::Timestamp};

/// A buffer for reordering video frames by presentation timestamp.
///
/// Collects frames until it reaches capacity, then outputs them in PTS order.
/// This handles B-frames and other out-of-order decode scenarios.
pub struct ReorderBuffer {
    frames: Vec<VideoFrame>,
    capacity: usize,
}

impl ReorderBuffer {
    /// Create a new reorder buffer with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            frames: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Get the current capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Check if the buffer is full.
    pub fn is_full(&self) -> bool {
        self.frames.len() >= self.capacity
    }

    /// Get the current number of frames in the buffer.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Push a frame into the buffer.
    ///
    /// Returns `Ok(())` if successful, or `Err(frame)` if the buffer is full.
    pub fn push(&mut self, frame: VideoFrame) -> Result<(), VideoFrame> {
        if self.is_full() {
            return Err(frame);
        }

        self.frames.push(frame);
        Ok(())
    }

    /// Pop the frame with the earliest PTS from the buffer.
    ///
    /// Returns `None` if the buffer is empty.
    pub fn pop(&mut self) -> Option<VideoFrame> {
        if self.frames.is_empty() {
            return None;
        }

        let min_idx = self.index_of_min_pts();
        Some(self.frames.swap_remove(min_idx))
    }

    /// Peek at the frame with the earliest PTS without removing it.
    ///
    /// Returns `None` if the buffer is empty.
    pub fn peek(&self) -> Option<&VideoFrame> {
        if self.frames.is_empty() {
            return None;
        }

        let min_idx = self.index_of_min_pts();
        Some(&self.frames[min_idx])
    }

    /// Remove and return a frame with the given PTS, if present.
    pub fn take_by_pts(&mut self, target: Timestamp) -> Option<VideoFrame> {
        let idx = self.frames.iter().position(|f| f.pts() == target)?;
        Some(self.frames.swap_remove(idx))
    }

    /// Clear all frames from the buffer.
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Get all frames in PTS order, clearing the buffer.
    pub fn drain(&mut self) -> Vec<VideoFrame> {
        let mut frames = Vec::with_capacity(self.frames.len());
        while let Some(frame) = self.pop() {
            frames.push(frame);
        }
        frames
    }

    /// Debug helper: return all PTS values currently in the buffer.
    pub fn pts_values(&self) -> Vec<i64> {
        self.frames.iter().map(|f| f.pts().timestamp()).collect()
    }

    fn index_of_min_pts(&self) -> usize {
        self.frames
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.pts()
                    .partial_cmp(&b.pts())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
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
    fn test_reorder_buffer_take_by_pts() {
        let mut buffer = ReorderBuffer::new(3);

        buffer.push(create_test_frame(300)).unwrap();
        buffer.push(create_test_frame(100)).unwrap();
        buffer.push(create_test_frame(200)).unwrap();

        // Take a specific PTS
        let frame = buffer.take_by_pts(Timestamp::new(200, TimeBase::MICROSECONDS));
        assert!(frame.is_some());
        assert_eq!(frame.unwrap().pts().timestamp(), 200);
        assert_eq!(buffer.len(), 2);

        // Missing PTS returns None
        let frame = buffer.take_by_pts(Timestamp::new(999, TimeBase::MICROSECONDS));
        assert!(frame.is_none());
        assert_eq!(buffer.len(), 2);
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
