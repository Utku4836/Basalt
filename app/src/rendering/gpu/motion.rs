//! Motion between frames: the grid gliding to a new scroll position, and the cursor
//! moving with its leading corners ahead of its trailing ones, which leaves a short
//! trail. Positions are in pixels; time comes from the caller.

use std::time::Instant;

const SCROLL_SECONDS: f32 = 0.15;
/// Corners facing the movement arrive first; those facing away arrive last.
const LEADING_SECONDS: f32 = 0.07;
const TRAILING_SECONDS: f32 = 0.13;

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

fn seconds(since: Instant, now: Instant) -> f32 {
    now.saturating_duration_since(since).as_secs_f32()
}

/// Where the newest grid content sits while it glides into place, and where the
/// content before it sits relative to it, filling what the newest has not reached.
#[derive(Clone, Copy, Debug)]
pub struct Scroll {
    start: f32,
    link: f32,
    began: Instant,
}

impl Scroll {
    pub fn new(now: Instant) -> Self {
        Self { start: 0.0, link: 0.0, began: now }
    }

    /// Vertical offset of the newest content.
    pub fn position(&self, now: Instant) -> f32 {
        self.start * (1.0 - ease_out(seconds(self.began, now) / SCROLL_SECONDS))
    }

    /// Offset of the previous content from the newest.
    pub fn link(&self) -> f32 {
        self.link
    }

    /// Pixels per second, for motion blur.
    pub fn speed(&self, now: Instant) -> f32 {
        let t = (seconds(self.began, now) / SCROLL_SECONDS).clamp(0.0, 1.0);
        (self.start * 3.0 * (1.0 - t).powi(2) / SCROLL_SECONDS).abs()
    }

    pub fn moving(&self, now: Instant) -> bool {
        self.start != 0.0 && seconds(self.began, now) < SCROLL_SECONDS
    }

    /// New content arrived moved down by `shift` pixels (up when negative) relative to
    /// the previous content: it starts where that content is now and glides to rest.
    pub fn shifted(&mut self, shift: f32, now: Instant) {
        let current = self.position(now);
        *self = Self { start: current - shift, link: shift, began: now };
    }

    /// New content with no relation to the previous one.
    pub fn reset(&mut self, now: Instant) {
        *self = Self::new(now);
    }
}

/// A rectangle's corners: top left, top right, bottom left, bottom right.
pub type Corners = [[f32; 2]; 4];

const CORNER_DIRECTIONS: [[f32; 2]; 4] = [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]];

fn corners(rect: [f32; 4]) -> Corners {
    let [x, y, width, height] = rect;
    [[x, y], [x + width, y], [x, y + height], [x + width, y + height]]
}

#[derive(Clone, Copy, Debug)]
struct Corner {
    from: [f32; 2],
    to: [f32; 2],
    seconds: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    target: Option<[f32; 4]>,
    corners: [Corner; 4],
    began: Instant,
}

impl Cursor {
    pub fn new(now: Instant) -> Self {
        let corner = Corner { from: [0.0; 2], to: [0.0; 2], seconds: 0.0 };
        Self { target: None, corners: [corner; 4], began: now }
    }

    /// Move to `rect` (x, y, width, height). The first position and `jump` land at once.
    pub fn move_to(&mut self, rect: [f32; 4], jump: bool, now: Instant) {
        if self.target == Some(rect) {
            return;
        }
        let current = self.corners(now);
        let target = corners(rect);
        let first = self.target.is_none();
        self.target = Some(rect);
        self.began = now;
        let center = |points: &Corners| {
            [points.iter().map(|p| p[0]).sum::<f32>() / 4.0, points.iter().map(|p| p[1]).sum::<f32>() / 4.0]
        };
        let (from, to) = (center(&current), center(&target));
        let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
        let length = (dx * dx + dy * dy).sqrt();
        for (index, corner) in self.corners.iter_mut().enumerate() {
            let [cx, cy] = CORNER_DIRECTIONS[index];
            // +1 when the corner faces the movement, -1 when it faces away.
            let facing = if length > 0.0 { (cx * dx + cy * dy) / (length * std::f32::consts::SQRT_2) } else { 0.0 };
            *corner = Corner {
                from: if first || jump { target[index] } else { current[index] },
                to: target[index],
                seconds: TRAILING_SECONDS + (LEADING_SECONDS - TRAILING_SECONDS) * (facing + 1.0) / 2.0,
            };
        }
    }

    pub fn corners(&self, now: Instant) -> Corners {
        let elapsed = seconds(self.began, now);
        self.corners.map(|corner| {
            let t = ease_out(elapsed / corner.seconds.max(f32::EPSILON));
            [corner.from[0] + (corner.to[0] - corner.from[0]) * t, corner.from[1] + (corner.to[1] - corner.from[1]) * t]
        })
    }

    pub fn moving(&self, now: Instant) -> bool {
        seconds(self.began, now) < TRAILING_SECONDS && self.corners.iter().any(|corner| corner.from != corner.to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn scrolling_glides_from_the_old_position_and_settles() {
        let start = Instant::now();
        let mut scroll = Scroll::new(start);
        scroll.shifted(60.0, start);
        assert_eq!(scroll.position(start), -60.0, "starts where the old content was");
        assert_eq!(scroll.link(), 60.0);
        let halfway = scroll.position(start + Duration::from_millis(50));
        assert!(-60.0 < halfway && halfway < 0.0);
        assert!(scroll.moving(start + Duration::from_millis(50)));
        assert_eq!(scroll.position(start + Duration::from_millis(200)), 0.0);
        assert!(!scroll.moving(start + Duration::from_millis(200)));
    }

    #[test]
    fn a_second_scroll_continues_from_the_current_position() {
        let start = Instant::now();
        let mut scroll = Scroll::new(start);
        scroll.shifted(60.0, start);
        let later = start + Duration::from_millis(40);
        let now = scroll.position(later);
        scroll.shifted(20.0, later);
        assert_eq!(scroll.position(later), now - 20.0);
    }

    #[test]
    fn cursor_leading_corners_arrive_first_and_all_settle() {
        let start = Instant::now();
        let mut cursor = Cursor::new(start);
        cursor.move_to([0.0, 18.0, 10.0, 2.0], false, start);
        assert!(!cursor.moving(start), "the first position lands at once");
        cursor.move_to([10.0, 18.0, 10.0, 2.0], false, start);
        let mid = cursor.corners(start + Duration::from_millis(50));
        // Moving right: the right edge has travelled further than the left.
        assert!(mid[1][0] - 20.0 > mid[0][0] - 10.0);
        assert!(mid[1][0] - mid[0][0] > 10.0, "stretched into a trail");
        let end = cursor.corners(start + Duration::from_millis(200));
        assert_eq!(end, corners([10.0, 18.0, 10.0, 2.0]));
        assert!(!cursor.moving(start + Duration::from_millis(200)));
    }
}
