use std::iter;
use std::num::NonZeroU32;

use hltas::types::{AutoMovement, FrameBulk, Line, StrafeDir, StrafeSettings};
use hltas::HLTAS;
use itertools::Itertools;

/// Helper methods for `FrameBulk`.
pub trait FrameBulkExt {
    /// Returns a reference to the yaw stored in the frame bulk, if any.
    fn yaw(&self) -> Option<&f32>;

    /// Returns a mutable reference to the yaw stored in the frame bulk, if any.
    fn yaw_mut(&mut self) -> Option<&mut f32>;

    /// Returns a reference to the left-right count stored in the frame bulk, if any.
    fn left_right_count(&self) -> Option<&NonZeroU32>;

    /// Returns a mutable reference to the left-right count stored in the frame bulk, if any.
    fn left_right_count_mut(&mut self) -> Option<&mut NonZeroU32>;
}

impl FrameBulkExt for FrameBulk {
    fn yaw(&self) -> Option<&f32> {
        match &self.auto_actions.movement {
            Some(AutoMovement::SetYaw(yaw)) => Some(yaw),
            Some(AutoMovement::Strafe(StrafeSettings {
                dir: StrafeDir::Yaw(yaw) | StrafeDir::Line { yaw },
                ..
            })) => Some(yaw),
            _ => None,
        }
    }

    fn yaw_mut(&mut self) -> Option<&mut f32> {
        match &mut self.auto_actions.movement {
            Some(AutoMovement::SetYaw(yaw)) => Some(yaw),
            Some(AutoMovement::Strafe(StrafeSettings {
                dir: StrafeDir::Yaw(yaw) | StrafeDir::Line { yaw },
                ..
            })) => Some(yaw),
            _ => None,
        }
    }

    fn left_right_count(&self) -> Option<&NonZeroU32> {
        match &self.auto_actions.movement {
            Some(AutoMovement::Strafe(StrafeSettings {
                dir: StrafeDir::LeftRight(count) | StrafeDir::RightLeft(count),
                ..
            })) => Some(count),
            _ => None,
        }
    }

    fn left_right_count_mut(&mut self) -> Option<&mut NonZeroU32> {
        match &mut self.auto_actions.movement {
            Some(AutoMovement::Strafe(StrafeSettings {
                dir: StrafeDir::LeftRight(count) | StrafeDir::RightLeft(count),
                ..
            })) => Some(count),
            _ => None,
        }
    }
}

/// Returns, for every simulated frame, the index of the frame bulk that was used for simulating
/// that frame, the frame bulk, and whether the frame is the last frame in the frame bulk.
pub fn bulk_idx_and_is_last(
    lines: &[Line],
) -> impl Iterator<Item = (usize, &FrameBulk, bool)> + '_ {
    // Returns the index of the frame bulk that was used for simulating that frame.
    let bulk_idx = lines
        .iter()
        // Take only frame bulk lines.
        .filter_map(Line::frame_bulk)
        // Get their indices.
        .enumerate()
        // Repeat index for frame bulk frame count.
        .flat_map(|(idx, bulk)| iter::repeat((idx, bulk)).take(bulk.frame_count.get() as usize));

    bulk_idx.peekable().batching(|it| {
        let (curr_idx, curr_bulk) = it.next()?;

        // Peek at the next frame's bulk index.
        match it.peek() {
            // Last frame is last in its frame bulk.
            None => Some((curr_idx, curr_bulk, true)),
            // Frame is last in its bulk if the next bulk index is different.
            Some((next_idx, _)) => Some((curr_idx, curr_bulk, curr_idx != *next_idx)),
        }
    })
}

/// Returns frame bulk and index of first frame simulated by it.
///
/// The index starts at `1` because the very first frame is always the initial frame, which is not
/// simulated by any frame bulk.
pub fn bulk_and_first_frame_idx(
    hltas: &mut HLTAS,
) -> impl Iterator<Item = (&mut FrameBulk, usize)> {
    hltas.frame_bulks_mut().scan(1, |frame_idx, bulk| {
        let first_frame_idx = *frame_idx;
        *frame_idx += bulk.frame_count.get() as usize;
        Some((bulk, first_frame_idx))
    })
}

/// Returns index of first frame affected by every line.
///
/// The index starts at `1` because the very first frame is always the initial frame, which is not
/// affected by any line.
pub fn line_first_frame_idx(hltas: &HLTAS) -> impl Iterator<Item = usize> + '_ {
    hltas.lines.iter().scan(1, |frame_idx, line| {
        let first_frame_idx = *frame_idx;

        if let Some(bulk) = line.frame_bulk() {
            *frame_idx += bulk.frame_count.get() as usize;
        }

        Some(first_frame_idx)
    })
}

pub fn line_idx_and_repeat_at_frame(lines: &[Line], frame_idx: usize) -> Option<(usize, u32)> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(line_idx, line)| line.frame_bulk().map(|bulk| (line_idx, bulk)))
        .flat_map(|(line_idx, bulk)| {
            (0..bulk.frame_count.get()).map(move |repeat| (line_idx, repeat))
        })
        .nth(frame_idx)
}
