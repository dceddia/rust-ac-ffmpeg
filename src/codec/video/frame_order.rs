//! Puts a decoder's frames in display order, each as soon as no frame shown before it can still
//! come.
//!
//! Decoding timestamps never go down, and no frame is shown before it's decoded (PTS >= DTS). So
//! once a packet with DTS `d` has been pushed, no packet pushed after it shows a frame before `d`:
//! a decoded frame with PTS <= `d` can only be preceded by frames of packets already pushed. Of
//! those, the ones still to come out of the decoder are tracked by PTS (VideoToolbox decodes
//! asynchronously: a frame can come out well after its packet went in, or not at all). A frame
//! goes out once its PTS is at most the highest DTS pushed and no packet still to come out shows
//! a frame before it. That waits only as many packets as the stream reorders frames by (2 with
//! B-pyramids, none without B-frames), where a fixed-size buffer waited for its size.

use log::warn;

/// How many frames wait for their turn at most when the packets' DTS can't be used (missing, or
/// going down). Without it, nothing bounds what later packets show, so this is a guess: the
/// fixed buffer this replaced held 8, and real streams reorder by 4 at most.
const FALLBACK_DEPTH: usize = 8;

/// What the DTS of the packets pushed since the last clear say about the ones to come
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeTimes {
    /// Nothing pushed yet
    NonePushed,
    /// The highest DTS pushed: no packet still to come shows a frame before it
    UpTo(i64),
    /// A packet had no DTS, or one lower than an earlier packet's: they say nothing
    Unusable,
}

/// Frames out of a decoder, put in display order. See the module's documentation.
///
/// Timestamps are in the decoder's time base. `F` is the frame (a `VideoFrame`; a plain value in
/// tests).
pub(crate) struct FrameOrder<F> {
    /// The PTS of each pushed packet whose frame hasn't come out of the decoder yet, nor word
    /// that it won't
    pending: Vec<i64>,
    /// Frames out of the decoder, waiting for their turn, with their PTS
    ready: Vec<(i64, F)>,
    decode_times: DecodeTimes,
    /// Whether the decoder gives frames in display order (FFmpeg's decoders reorder frames
    /// themselves; VideoToolbox gives them as it decodes them). Then a frame also means none
    /// before it is still to come: a packet whose frame the decoder dropped without a word
    /// doesn't hold the frames after it.
    in_display_order: bool,
    /// No packets are coming (the decoder was flushed at the end): every frame goes out
    draining: bool,
    logged_unusable: bool,
}

impl<F> FrameOrder<F> {
    pub fn new(in_display_order: bool) -> Self {
        Self {
            pending: Vec::new(),
            ready: Vec::new(),
            decode_times: DecodeTimes::NonePushed,
            in_display_order,
            draining: false,
            logged_unusable: false,
        }
    }

    /// A packet went into the decoder. `shows_a_frame` is false for a packet whose frame isn't to
    /// be output (decoded for the frames that depend on it only): its frame isn't waited for.
    /// `pts` and `dts` are `None` when the packet has none.
    pub fn pushed(&mut self, pts: Option<i64>, dts: Option<i64>, shows_a_frame: bool) {
        self.draining = false;
        if shows_a_frame {
            if let Some(pts) = pts {
                self.pending.push(pts);
            }
        }

        self.decode_times = match (self.decode_times, dts) {
            (DecodeTimes::NonePushed, Some(dts)) => DecodeTimes::UpTo(dts),
            (DecodeTimes::UpTo(last), Some(dts)) if dts >= last => DecodeTimes::UpTo(dts),
            (DecodeTimes::Unusable, _) => DecodeTimes::Unusable,
            (_, dts) => {
                if !self.logged_unusable {
                    self.logged_unusable = true;
                    warn!(
                        "Frame order: a packet's DTS is {dts:?} after {:?}: holding up to {FALLBACK_DEPTH} frames to order them",
                        self.decode_times
                    );
                }
                DecodeTimes::Unusable
            }
        };
    }

    /// The decoder gave the frame of the packet with PTS `pts`
    pub fn decoded(&mut self, pts: i64, frame: F) {
        self.forget_pending(pts);
        if self.in_display_order {
            self.pending.retain(|&pending| pending > pts);
        }
        self.ready.push((pts, frame));
    }

    /// The decoder won't give a frame for the packet with PTS `pts` (it skipped it, or couldn't
    /// decode it)
    pub fn not_decoded(&mut self, pts: i64) {
        self.forget_pending(pts);
    }

    /// No more packets are coming: every frame can go out, in order, once the decoder has given
    /// them all
    pub fn flushed(&mut self) {
        self.draining = true;
    }

    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// Forget every frame and packet, as after a seek
    pub fn clear(&mut self) {
        self.pending.clear();
        self.ready.clear();
        self.decode_times = DecodeTimes::NonePushed;
        self.draining = false;
    }

    /// The next frame in display order, if no frame before it can still come. While draining,
    /// the next one of those the decoder gave (call it only once the decoder has given all).
    pub fn pop(&mut self) -> Option<F> {
        let (index, pts) = self
            .ready
            .iter()
            .enumerate()
            .min_by_key(|(_, (pts, _))| *pts)
            .map(|(index, (pts, _))| (index, *pts))?;

        if !self.draining && !self.is_next(pts) {
            return None;
        }
        Some(self.ready.swap_remove(index).1)
    }

    /// Whether the next frame to go out is one the decoder still has to give, and no more packets
    /// are needed for it to go out: its packet went in, and so did a DTS that reaches it. Then
    /// it goes out as soon as the decoder gives it (and the ones before it that are still
    /// coming); pushing more packets doesn't bring it sooner.
    pub fn next_is_decoding(&self) -> bool {
        let DecodeTimes::UpTo(dts) = self.decode_times else {
            return false;
        };
        let Some(lowest_pending) = self.pending.iter().min().copied() else {
            return false;
        };
        let lowest_ready = self.ready.iter().map(|(pts, _)| *pts).min();
        let pending_is_next = lowest_ready.map_or(true, |ready| lowest_pending <= ready);
        pending_is_next && lowest_pending <= dts
    }

    /// Whether the frame with PTS `pts`, the lowest of the ready ones, is the next to show
    fn is_next(&self, pts: i64) -> bool {
        let later_packets_show_it_later = match self.decode_times {
            DecodeTimes::UpTo(dts) => pts <= dts,
            DecodeTimes::NonePushed | DecodeTimes::Unusable => {
                self.in_display_order || self.ready.len() >= FALLBACK_DEPTH
            }
        };
        let pushed_packets_show_it_later = self.pending.iter().all(|&pending| pending >= pts);
        later_packets_show_it_later && pushed_packets_show_it_later
    }

    fn forget_pending(&mut self, pts: i64) {
        if let Some(index) = self.pending.iter().position(|&pending| pending == pts) {
            self.pending.swap_remove(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushes packets (pts, dts) that show a frame
    fn push(order: &mut FrameOrder<i64>, packets: &[(i64, i64)]) {
        for &(pts, dts) in packets {
            order.pushed(Some(pts), Some(dts), true);
        }
    }

    fn decode(order: &mut FrameOrder<i64>, pts: &[i64]) {
        for &pts in pts {
            order.decoded(pts, pts);
        }
    }

    fn pop_all(order: &mut FrameOrder<i64>) -> Vec<i64> {
        std::iter::from_fn(|| order.pop()).collect()
    }

    /// I P B B, as decoded (VideoToolbox's order): the P frame waits for the B frames shown
    /// before it, each frame goes out once a DTS reaches it, and no sooner
    #[test]
    fn frames_decoded_out_of_order_go_out_in_display_order() {
        let mut order = FrameOrder::new(false);
        // pts 0, 3, 1, 2 with dts -1, 0, 1, 2 (one frame of reorder delay)
        push(&mut order, &[(0, -1)]);
        decode(&mut order, &[0]);
        // no DTS has reached 0 yet: a packet still to come could show a frame before it
        assert_eq!(order.pop(), None);

        push(&mut order, &[(3, 0)]);
        assert_eq!(order.pop(), Some(0));
        decode(&mut order, &[3]);
        push(&mut order, &[(1, 1)]);
        decode(&mut order, &[1]);
        assert_eq!(pop_all(&mut order), [1]);
        push(&mut order, &[(2, 2)]);
        decode(&mut order, &[2]);
        assert_eq!(pop_all(&mut order), [2]);

        // 3 waits until a DTS reaches it
        push(&mut order, &[(6, 3)]);
        assert_eq!(pop_all(&mut order), [3]);
    }

    /// Without B-frames (DTS = PTS) each frame goes out as soon as it's decoded
    #[test]
    fn frames_without_reordering_go_out_right_away() {
        let mut order = FrameOrder::new(false);
        for pts in 0..5 {
            push(&mut order, &[(pts, pts)]);
            decode(&mut order, &[pts]);
            assert_eq!(order.pop(), Some(pts));
        }
    }

    /// VideoToolbox decodes asynchronously: a frame before the decoded one that isn't out of the
    /// decoder yet holds it, until it comes
    #[test]
    fn a_frame_waits_for_an_earlier_one_still_decoding() {
        let mut order = FrameOrder::new(false);
        push(&mut order, &[(0, -2), (4, -1), (2, 0), (1, 1), (3, 2)]);
        decode(&mut order, &[0, 4, 2]);
        assert_eq!(pop_all(&mut order), [0]);
        // 1 isn't out yet, so 2 waits
        assert_eq!(order.pop(), None);
        decode(&mut order, &[3]);
        assert_eq!(order.pop(), None);
        decode(&mut order, &[1]);
        assert_eq!(pop_all(&mut order), [1, 2]);
        // 3 is pushed but no DTS reached 4 yet
        push(&mut order, &[(8, 3)]);
        assert_eq!(pop_all(&mut order), [3]);
    }

    /// After a seek, the target's frame needs a DTS that reaches it before it can go out; from
    /// then on it's only waiting for the decoder
    #[test]
    fn the_next_frame_is_decoding_once_a_dts_reaches_it() {
        let mut order = FrameOrder::new(false);
        assert!(!order.next_is_decoding());

        // I B B P B B, from a keyframe at 3 with two leading frames before it: 1 and 2 aren't
        // shown (before the target)
        push(&mut order, &[(3, 1)]);
        assert!(!order.next_is_decoding());
        order.pushed(Some(1), Some(2), false);
        assert!(!order.next_is_decoding());
        order.pushed(Some(2), Some(3), false);
        assert!(order.next_is_decoding());
        push(&mut order, &[(6, 4)]);
        assert!(order.next_is_decoding());

        // a frame after it out first doesn't change that
        decode(&mut order, &[6]);
        assert!(order.next_is_decoding());
        decode(&mut order, &[3]);
        assert!(!order.next_is_decoding());
        assert_eq!(order.pop(), Some(3));

        // 6 is out of the decoder, but no DTS reached it yet: a packet to come may show a frame
        // before it
        assert!(!order.next_is_decoding());
        assert_eq!(order.pop(), None);
        // and one does
        push(&mut order, &[(4, 5)]);
        assert!(order.next_is_decoding());
        decode(&mut order, &[4]);
        assert_eq!(order.pop(), Some(4));
        assert!(!order.next_is_decoding());
    }

    /// Without usable DTS nothing says when a frame is next: more packets are always wanted
    #[test]
    fn without_dts_the_next_frame_is_never_only_decoding() {
        let mut order = FrameOrder::<i64>::new(false);
        order.pushed(Some(0), None, true);
        order.pushed(Some(1), None, true);
        assert!(!order.next_is_decoding());
    }

    /// A packet whose frame isn't shown (decoded only for others: before a seek's target)
    /// isn't waited for, nor is one the decoder skipped (VideoToolbox's DoNotDisplay, a leading
    /// frame it can't decode) or failed to decode
    #[test]
    fn frames_that_wont_come_dont_hold_the_others() {
        let mut order = FrameOrder::new(false);
        order.pushed(Some(1), Some(-1), false);
        push(&mut order, &[(3, 0), (2, 1), (6, 3)]);
        decode(&mut order, &[3]);
        // 2 still to come
        assert_eq!(order.pop(), None);
        order.not_decoded(2);
        assert_eq!(order.pop(), Some(3));

        // and an error, the same way
        push(&mut order, &[(5, 4), (4, 5)]);
        decode(&mut order, &[5]);
        assert_eq!(order.pop(), None);
        order.not_decoded(4);
        assert_eq!(order.pop(), Some(5));
    }

    /// FFmpeg's decoders give frames in display order, and drop some without a word (a leading
    /// frame without its references after a seek): a later frame means the earlier ones are
    /// either out or not coming
    #[test]
    fn a_decoder_in_display_order_drops_frames_without_holding_the_rest() {
        let mut order = FrameOrder::new(true);
        // 0 and 1 are dropped
        push(&mut order, &[(2, 0), (0, 1), (1, 2), (3, 3)]);
        decode(&mut order, &[2]);
        assert_eq!(pop_all(&mut order), [2]);
        decode(&mut order, &[3]);
        assert_eq!(pop_all(&mut order), [3]);
    }

    /// At the end, after the flush, the frames that no DTS reached go out too, in order
    #[test]
    fn the_last_frames_go_out_at_the_end() {
        let mut order = FrameOrder::new(false);
        push(&mut order, &[(0, -1), (2, 0), (1, 1)]);
        decode(&mut order, &[0, 2, 1]);
        assert_eq!(pop_all(&mut order), [0, 1]);
        order.flushed();
        assert!(order.is_draining());
        assert_eq!(pop_all(&mut order), [2]);

        // pushing again ends the drain
        push(&mut order, &[(4, 2)]);
        assert!(!order.is_draining());
    }

    /// A seek forgets the frames and packets, and the DTS reached
    #[test]
    fn a_clear_forgets_everything() {
        let mut order = FrameOrder::new(false);
        push(&mut order, &[(10, 9), (12, 10), (11, 11)]);
        decode(&mut order, &[10, 12]);
        order.clear();
        assert_eq!(order.pop(), None);

        // earlier in the file: neither the old DTS nor the old pending packet counts
        push(&mut order, &[(0, -1), (1, 0)]);
        decode(&mut order, &[0]);
        assert_eq!(pop_all(&mut order), [0]);
        assert_eq!(order.decode_times, DecodeTimes::UpTo(0));
    }

    /// Without DTS, nothing says what later packets show: frames wait until 8 are decoded,
    /// unless the decoder orders them itself
    #[test]
    fn without_dts_frames_wait_for_a_fixed_number() {
        let mut order = FrameOrder::new(false);
        for pts in [0, 2, 1, 4, 3, 6, 5] {
            order.pushed(Some(pts), None, true);
            decode(&mut order, &[pts]);
        }
        assert_eq!(order.pop(), None);
        order.pushed(Some(8), None, true);
        decode(&mut order, &[8]);
        assert_eq!(pop_all(&mut order), [0]);
        order.flushed();
        assert_eq!(pop_all(&mut order), [1, 2, 3, 4, 5, 6, 8]);

        let mut in_order = FrameOrder::new(true);
        in_order.pushed(Some(0), None, true);
        decode(&mut in_order, &[0]);
        assert_eq!(in_order.pop(), Some(0));
    }

    /// A DTS lower than one before (a broken file) makes the DTS unusable until the next clear
    #[test]
    fn a_dts_going_down_falls_back_to_the_fixed_number() {
        let mut order = FrameOrder::new(false);
        push(&mut order, &[(5, 5), (6, 3)]);
        decode(&mut order, &[5]);
        assert_eq!(order.decode_times, DecodeTimes::Unusable);
        assert_eq!(order.pop(), None);
        order.clear();
        push(&mut order, &[(5, 5)]);
        decode(&mut order, &[5]);
        assert_eq!(order.pop(), Some(5));
    }
}
