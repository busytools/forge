use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

static CACHE_ACCESS_TICK: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_cache_access_tick() -> u64 {
    CACHE_ACCESS_TICK.fetch_add(1, Ordering::Relaxed)
}

/// How many previously-rendered widths to keep around alongside the
/// live slot. With N=2, a block can cache 3 widths total. On
/// resize-back to a width still in the LRU, `get_for_width` hits and
/// the caller skips the expensive `tc::render_body` work. Memory cost
/// is bounded per block; the workspace render budget continues to
/// enforce total bytes via `evict_cached_render`.
const MAX_STALE_WIDTHS: usize = 2;

/// Cached rendered lines for a block. Stores a version counter so the cache
/// is only recomputed when the block content actually changes.
///
/// The width-keyed path keeps a small LRU of previously-rendered widths
/// in `stale_widths` so resize cycles don't force a mass re-render
/// (see #125). Same-width re-stores overwrite the live slot in place
/// (no rotation). `invalidate()` clears the LRU because stale lines
/// belong to old content.
///
/// Fields are private - use `invalidate()` to mark stale, `is_stale()` to check,
/// `get()` to read cached lines, and `store()` to populate.
#[derive(Default)]
pub struct BlockCache {
    version: u64,
    lines: Option<Vec<ratatui::text::Line<'static>>>,
    render_width: Option<u16>,
    /// Segmentation metadata for KB-sized cache chunks shared across message/tool caches.
    segments: Vec<CacheLineSegment>,
    /// Approximate UTF-8 byte size of cached rendered lines.
    cached_bytes: usize,
    /// Previously-rendered widths kept around for resize-back recovery.
    /// FIFO bounded by `MAX_STALE_WIDTHS`: push_back on rotation,
    /// pop_front when capacity is exceeded.
    stale_widths: VecDeque<StaleSlot>,
    /// Wrapped line count of the cached lines at `wrapped_width`.
    /// Computed via `Paragraph::line_count(width)` on the same lines stored in `lines`.
    wrapped_height: usize,
    /// The viewport width used to compute `wrapped_height`.
    wrapped_width: u16,
    wrapped_height_valid: bool,
    last_access_tick: Cell<u64>,
}

/// A previously-rendered width's lines kept in the LRU. Carries
/// segments + a snapshot of the height-at-this-width so that a
/// `get_for_width` hit can swap with the live slot and still have
/// the measure path land on the right lines / segments / height.
struct StaleSlot {
    width: u16,
    lines: Vec<ratatui::text::Line<'static>>,
    segments: Vec<CacheLineSegment>,
    cached_bytes: usize,
    /// Snapshot of `BlockCache.wrapped_height` at rotation time, if the
    /// height was already measured for this width. `None` means the
    /// caller will need to re-measure via segments after promotion.
    wrapped_height: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheLineSegment {
    start: usize,
    end: usize,
    wrapped_height: usize,
    wrapped_width: u16,
    wrapped_height_valid: bool,
}

impl CacheLineSegment {
    fn new(start: usize, end: usize) -> Self {
        Self { start, end, wrapped_height: 0, wrapped_width: 0, wrapped_height_valid: false }
    }
}

impl BlockCache {
    fn touch(&self) {
        self.last_access_tick.set(next_cache_access_tick());
    }

    /// Bump the version to invalidate cached lines and height. Also
    /// drops any LRU-cached stale widths, whose lines refer to the
    /// pre-invalidate content and would resurrect under the next
    /// store's `version = 0` reset.
    pub fn invalidate(&mut self) {
        self.version += 1;
        self.wrapped_height_valid = false;
        self.stale_widths.clear();
    }

    /// Get a reference to the cached lines, if fresh.
    pub fn get(&self) -> Option<&Vec<ratatui::text::Line<'static>>> {
        if self.version == 0 && self.render_width.is_none() {
            let lines = self.lines.as_ref();
            if lines.is_some() {
                self.touch();
            }
            lines
        } else {
            None
        }
    }

    pub fn get_for_width(&mut self, width: u16) -> Option<&Vec<ratatui::text::Line<'static>>> {
        if self.version != 0 {
            return None;
        }
        if self.render_width == Some(width) {
            let lines = self.lines.as_ref();
            if lines.is_some() {
                self.touch();
            }
            return lines;
        }
        // Resize-back recovery: promote a matching stale slot to live
        // so the subsequent `measure_and_set_height(width)` operates
        // on the right lines + segments. Without promotion the
        // measure path would re-measure the LIVE slot's lines at
        // `width`, which is wrong for width-dependent content (diff
        // bodies). The current live slot rotates into stale.
        let idx = self.stale_widths.iter().position(|slot| slot.width == width)?;
        let promoted = self.stale_widths.remove(idx)?;
        if let Some(old_width) = self.render_width.take()
            && let Some(old_lines) = self.lines.take()
        {
            self.rotate_live_into_stale(old_width, old_lines);
        }
        self.render_width = Some(promoted.width);
        self.lines = Some(promoted.lines);
        self.segments = promoted.segments;
        self.cached_bytes = promoted.cached_bytes;
        if let Some(h) = promoted.wrapped_height {
            self.wrapped_height = h;
            self.wrapped_width = promoted.width;
            self.wrapped_height_valid = true;
        } else {
            self.wrapped_height = 0;
            self.wrapped_width = 0;
            self.wrapped_height_valid = false;
        }
        self.touch();
        self.lines.as_ref()
    }

    /// Store freshly rendered lines, marking the cache as clean.
    /// Height is set separately via `set_height()` after measurement.
    pub fn store(&mut self, lines: Vec<ratatui::text::Line<'static>>) {
        self.store_with_policy(lines, *super::super::default_cache_split_policy());
    }

    /// Store freshly rendered lines using a shared KB split policy.
    /// Defensively drops any width-LRU entries since no-width blocks
    /// don't participate in the LRU - if anything's there, it's stale
    /// from a prior misuse and would leak memory.
    pub fn store_with_policy(
        &mut self,
        lines: Vec<ratatui::text::Line<'static>>,
        policy: super::super::CacheSplitPolicy,
    ) {
        self.render_width = None;
        self.stale_widths.clear();
        self.store_with_policy_and_width(lines, policy);
    }

    /// Store rendered lines keyed by `width`. When the new width
    /// differs from the live one, the current slot rotates into
    /// `stale_widths` so resize-back hits the cache instead of forcing
    /// a re-render. Same-width re-stores overwrite in place.
    pub fn store_for_width(&mut self, lines: Vec<ratatui::text::Line<'static>>, width: u16) {
        let should_rotate =
            self.render_width.is_some_and(|live| live != width) && self.lines.is_some();
        if should_rotate
            && let Some(old_width) = self.render_width.take()
            && let Some(old_lines) = self.lines.take()
        {
            self.rotate_live_into_stale(old_width, old_lines);
        }
        self.render_width = Some(width);
        self.store_with_policy_and_width(lines, *super::super::default_cache_split_policy());
    }

    /// Move the current live slot's lines + segments + measured height
    /// (when valid) into a fresh `StaleSlot`, evicting the oldest stale
    /// entry if the LRU is at capacity. The caller is responsible for
    /// already having taken `lines` and `render_width`; this helper
    /// drains the remaining live state.
    fn rotate_live_into_stale(
        &mut self,
        old_width: u16,
        old_lines: Vec<ratatui::text::Line<'static>>,
    ) {
        let old_segments = std::mem::take(&mut self.segments);
        let old_bytes = std::mem::replace(&mut self.cached_bytes, 0);
        let old_wrapped_height = if self.wrapped_height_valid && self.wrapped_width == old_width {
            Some(self.wrapped_height)
        } else {
            None
        };
        self.wrapped_height = 0;
        self.wrapped_width = 0;
        self.wrapped_height_valid = false;
        self.stale_widths.push_back(StaleSlot {
            width: old_width,
            lines: old_lines,
            segments: old_segments,
            cached_bytes: old_bytes,
            wrapped_height: old_wrapped_height,
        });
        while self.stale_widths.len() > MAX_STALE_WIDTHS {
            self.stale_widths.pop_front();
        }
    }

    fn store_with_policy_and_width(
        &mut self,
        lines: Vec<ratatui::text::Line<'static>>,
        policy: super::super::CacheSplitPolicy,
    ) {
        let segment_limit = policy.hard_limit_bytes.max(1);
        let (segments, cached_bytes) = build_line_segments(&lines, segment_limit);
        self.lines = Some(lines);
        self.segments = segments;
        self.cached_bytes = cached_bytes;
        self.version = 0;
        self.wrapped_height = 0;
        self.wrapped_width = 0;
        self.wrapped_height_valid = false;
        self.touch();
    }

    /// Set the wrapped height for the cached lines at the given width.
    /// Called by the viewport/chat layer after `Paragraph::line_count(width)`.
    /// Separate from `store()` so height measurement is the viewport's job.
    pub fn set_height(&mut self, height: usize, width: u16) {
        self.wrapped_height = height;
        self.wrapped_width = width;
        self.wrapped_height_valid = true;
        self.touch();
    }

    /// Get the cached wrapped height if cache is valid and was computed at the given width.
    pub fn height_at(&self, width: u16) -> Option<usize> {
        if self.version == 0 && self.wrapped_height_valid && self.wrapped_width == width {
            self.touch();
            Some(self.wrapped_height)
        } else {
            None
        }
    }

    /// Recompute wrapped height from cached segments and memoize it at `width`.
    /// Returns `None` when the render cache is stale.
    pub fn measure_and_set_height(&mut self, width: u16) -> Option<usize> {
        if self.version != 0 {
            return None;
        }
        if let Some(h) = self.height_at(width) {
            return Some(h);
        }

        let lines = self.lines.as_ref()?;

        if self.segments.is_empty() {
            self.set_height(0, width);
            return Some(0);
        }

        let mut total_height = 0usize;
        for segment in &mut self.segments {
            if segment.wrapped_height_valid && segment.wrapped_width == width {
                total_height = total_height.saturating_add(segment.wrapped_height);
                continue;
            }
            let segment_lines = lines[segment.start..segment.end].to_vec();
            let h = ratatui::widgets::Paragraph::new(ratatui::text::Text::from(segment_lines))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .line_count(width);
            segment.wrapped_height = h;
            segment.wrapped_width = width;
            segment.wrapped_height_valid = true;
            total_height = total_height.saturating_add(h);
        }

        self.set_height(total_height, width);
        Some(total_height)
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    pub fn cached_bytes(&self) -> usize {
        let stale: usize = self.stale_widths.iter().map(|slot| slot.cached_bytes).sum();
        self.cached_bytes.saturating_add(stale)
    }

    pub fn last_access_tick(&self) -> u64 {
        self.last_access_tick.get()
    }

    pub fn evict_cached_render(&mut self) -> usize {
        let stale_bytes: usize = self.stale_widths.iter().map(|slot| slot.cached_bytes).sum();
        let total = self.cached_bytes.saturating_add(stale_bytes);
        if total == 0 {
            return 0;
        }
        self.lines = None;
        self.render_width = None;
        self.segments.clear();
        self.cached_bytes = 0;
        self.stale_widths.clear();
        self.wrapped_height = 0;
        self.wrapped_width = 0;
        self.wrapped_height_valid = false;
        self.version = self.version.wrapping_add(1);
        total
    }
}

fn build_line_segments(
    lines: &[ratatui::text::Line<'static>],
    segment_limit_bytes: usize,
) -> (Vec<CacheLineSegment>, usize) {
    if lines.is_empty() {
        return (Vec::new(), 0);
    }

    let limit = segment_limit_bytes.max(1);
    let mut segments = Vec::new();
    let mut total_bytes = 0usize;
    let mut start = 0usize;
    let mut acc = 0usize;

    for (idx, line) in lines.iter().enumerate() {
        let line_bytes = line_utf8_bytes(line).max(1);
        total_bytes = total_bytes.saturating_add(line_bytes);

        if idx > start && acc.saturating_add(line_bytes) > limit {
            segments.push(CacheLineSegment::new(start, idx));
            start = idx;
            acc = 0;
        }
        acc = acc.saturating_add(line_bytes);
    }

    segments.push(CacheLineSegment::new(start, lines.len()));
    (segments, total_bytes)
}

fn line_utf8_bytes(line: &ratatui::text::Line<'static>) -> usize {
    let span_bytes =
        line.spans.iter().fold(0usize, |acc, span| acc.saturating_add(span.content.len()));
    span_bytes.saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    fn make_lines(text: &str) -> Vec<Line<'static>> {
        vec![Line::from(text.to_owned())]
    }

    /// Resize-back recovery: storing at width A, then B, must leave A's
    /// lines reachable via `get_for_width(A)` instead of forcing a
    /// full re-render. This is the bug fix for #125's mass-cache-miss
    /// on resize.
    #[test]
    fn width_lru_restores_lines_on_resize_back() {
        let mut cache = BlockCache::default();
        cache.store_for_width(make_lines("rendered at 80"), 80);
        cache.store_for_width(make_lines("rendered at 120"), 120);

        let restored = cache.get_for_width(80).expect("80 cached in stale lru");
        assert_eq!(restored[0].spans[0].content, "rendered at 80");
        let live = cache.get_for_width(120).expect("120 is live");
        assert_eq!(live[0].spans[0].content, "rendered at 120");
    }

    /// LRU capacity bound: with the live slot + 2 stale slots, the
    /// 4th distinct width must evict the oldest stale entry.
    #[test]
    fn width_lru_evicts_oldest_on_capacity() {
        let mut cache = BlockCache::default();
        cache.store_for_width(make_lines("at-80"), 80);
        cache.store_for_width(make_lines("at-100"), 100);
        cache.store_for_width(make_lines("at-120"), 120);
        cache.store_for_width(make_lines("at-140"), 140);

        assert!(cache.get_for_width(80).is_none(), "oldest width 80 should be evicted");
        assert!(cache.get_for_width(100).is_some(), "100 stays in stale lru");
        assert!(cache.get_for_width(120).is_some(), "120 stays in stale lru");
        assert!(cache.get_for_width(140).is_some(), "140 is live");
    }

    /// `invalidate()` must drop stale slots too - their lines refer to
    /// pre-invalidate content and would resurrect under the next
    /// store's `version = 0` reset.
    #[test]
    fn invalidate_clears_stale_widths() {
        let mut cache = BlockCache::default();
        cache.store_for_width(make_lines("at-80"), 80);
        cache.store_for_width(make_lines("at-120"), 120);
        assert!(cache.get_for_width(80).is_some());
        assert!(cache.get_for_width(120).is_some());

        cache.invalidate();
        assert!(cache.get_for_width(80).is_none(), "stale cleared on invalidate");
        assert!(cache.get_for_width(120).is_none(), "live invalidated by version bump");
    }

    /// Budget accounting: `cached_bytes` must include stale slot bytes
    /// so `render_budget::enforce_render_cache_budget` sees the real
    /// memory cost of the LRU. Uses identical content at two widths so
    /// the byte math depends on slot count, not content length.
    #[test]
    fn cached_bytes_sums_live_and_stale_slots() {
        let mut cache = BlockCache::default();
        let content = make_lines("identical-content-for-byte-math");
        cache.store_for_width(content.clone(), 80);
        let bytes_one_slot = cache.cached_bytes();
        assert!(bytes_one_slot > 0);
        cache.store_for_width(content, 120);
        let bytes_two_slots = cache.cached_bytes();
        assert_eq!(
            bytes_two_slots,
            bytes_one_slot * 2,
            "cached_bytes should sum live + stale slot bytes exactly (each holds identical content)",
        );
    }

    /// `evict_cached_render` must return the total freed bytes (live +
    /// stale) and leave the cache fully empty. Uses identical content
    /// at three widths so the byte math depends on slot count.
    #[test]
    fn evict_cached_render_returns_total_bytes_and_clears_all() {
        let mut cache = BlockCache::default();
        let content = make_lines("identical-content-for-byte-math");
        cache.store_for_width(content.clone(), 80);
        let per_slot = cache.cached_bytes();
        cache.store_for_width(content.clone(), 100);
        cache.store_for_width(content, 120);
        let total = cache.cached_bytes();
        assert_eq!(total, per_slot * 3, "three identical slots sum to 3x per-slot bytes");
        let evicted = cache.evict_cached_render();
        assert_eq!(evicted, total, "evict should return total cached bytes (live + stale)");
        assert_eq!(cache.cached_bytes(), 0);
        assert!(cache.get_for_width(80).is_none());
        assert!(cache.get_for_width(100).is_none());
        assert!(cache.get_for_width(120).is_none());
    }

    /// Regression for #125: on a stale-LRU hit at width W,
    /// the subsequent `measure_and_set_height(W)` must read the
    /// promoted slot's segments, not the LIVE slot's. Otherwise
    /// width-dependent bodies (diff content) memoize a height
    /// computed from the wrong line set. Uses line counts that
    /// disambiguate which slot the measure path saw.
    #[test]
    fn measure_after_stale_hit_uses_promoted_slot() {
        let mut cache = BlockCache::default();
        let three_lines: Vec<Line<'static>> =
            vec![Line::from("a"), Line::from("b"), Line::from("c")];
        cache.store_for_width(three_lines, 80);
        let h_80 = cache.measure_and_set_height(80).expect("measure at 80");
        assert_eq!(h_80, 3, "three single-char lines measure to 3 at width 80");

        cache.store_for_width(vec![Line::from("one")], 120);
        let h_120 = cache.measure_and_set_height(120).expect("measure at 120");
        assert_eq!(h_120, 1, "one-line body measures to 1 at width 120");

        let restored = cache.get_for_width(80).expect("80 still in stale lru");
        assert_eq!(restored.len(), 3, "promoted slot exposes the 3-line body");

        let h_80_again = cache.measure_and_set_height(80).expect("re-measure at 80");
        assert_eq!(
            h_80_again, 3,
            "height after stale hit must match the promoted slot, not the prior live (120) body",
        );
    }

    /// On stale-LRU hit, the prior live slot rotates into stale so
    /// both widths remain reachable. Verifies the swap: after
    /// promoting 80, the previously-live 120 must still be in stale.
    #[test]
    fn stale_hit_rotates_prior_live_into_stale() {
        let mut cache = BlockCache::default();
        cache.store_for_width(make_lines("at-80"), 80);
        cache.store_for_width(make_lines("at-120"), 120);
        // Promote 80.
        let promoted = cache.get_for_width(80).expect("80 promotes");
        assert_eq!(promoted[0].spans[0].content, "at-80");
        // 120 must now be reachable via stale.
        let stale_120 = cache.get_for_width(120).expect("120 rotated into stale");
        assert_eq!(stale_120[0].spans[0].content, "at-120");
    }

    /// Same-width re-store overwrites in place without rotating stale
    /// slots - otherwise repeated renders at one width would push old
    /// widths out unnecessarily.
    #[test]
    fn same_width_restore_does_not_rotate_stale() {
        let mut cache = BlockCache::default();
        cache.store_for_width(make_lines("at-80-v1"), 80);
        cache.store_for_width(make_lines("at-120"), 120);
        // Re-store at 120: stale should still contain 80.
        cache.store_for_width(make_lines("at-120-v2"), 120);
        assert!(
            cache.get_for_width(80).is_some(),
            "80 should not be pushed out by same-width re-store"
        );
        assert_eq!(
            cache.get_for_width(120).expect("120 live")[0].spans[0].content,
            "at-120-v2",
            "live slot should hold the most recent v2 content",
        );
    }

    // BlockCache

    #[test]
    fn cache_lifecycle_covers_default_store_invalidate_and_restore() {
        let mut cache = BlockCache::default();
        assert!(cache.get().is_none());

        cache.store(vec![Line::from("old")]);
        assert_eq!(cache.get().unwrap().len(), 1);

        cache.invalidate();
        cache.invalidate();
        cache.invalidate();
        assert!(cache.get().is_none());

        cache.store(vec![Line::from("new")]);
        let lines = cache.get().unwrap();
        assert_eq!(lines.len(), 1);
        let span_content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(span_content, "new");
    }

    #[test]
    fn cache_store_empty_lines() {
        let mut cache = BlockCache::default();
        cache.store(Vec::new());
        let lines = cache.get().unwrap();
        assert!(lines.is_empty());
    }

    /// Store twice without invalidating - second store overwrites first.
    #[test]
    fn cache_store_overwrite_without_invalidate() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("first")]);
        cache.store(vec![Line::from("second"), Line::from("line2")]);
        let lines = cache.get().unwrap();
        assert_eq!(lines.len(), 2);
        let content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(content, "second");
    }

    /// `get()` called twice returns consistent data.
    #[test]
    fn cache_get_twice_consistent() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("stable")]);
        let first = cache.get().unwrap().len();
        let second = cache.get().unwrap().len();
        assert_eq!(first, second);
    }

    // BlockCache

    #[test]
    fn cache_store_many_lines() {
        let mut cache = BlockCache::default();
        let lines: Vec<Line<'static>> =
            (0..1000).map(|i| Line::from(Span::raw(format!("line {i}")))).collect();
        cache.store(lines);
        assert_eq!(cache.get().unwrap().len(), 1000);
    }

    #[test]
    fn cache_store_splits_into_kb_segments() {
        let mut cache = BlockCache::default();
        let long = "x".repeat(800);
        let lines: Vec<Line<'static>> = (0..12).map(|_| Line::from(long.clone())).collect();
        cache.store(lines);
        assert!(cache.segment_count() > 1);
        assert!(cache.cached_bytes() > 0);
    }

    #[test]
    fn cache_invalidate_without_store() {
        let mut cache = BlockCache::default();
        cache.invalidate();
        assert!(cache.get().is_none());
    }

    #[test]
    fn cache_rapid_store_invalidate_cycle() {
        let mut cache = BlockCache::default();
        for i in 0..50 {
            cache.store(vec![Line::from(format!("v{i}"))]);
            assert!(cache.get().is_some());
            cache.invalidate();
            assert!(cache.get().is_none());
        }
        cache.store(vec![Line::from("final")]);
        assert!(cache.get().is_some());
    }

    /// Store styled lines with multiple spans per line.
    #[test]
    fn cache_store_styled_lines() {
        let mut cache = BlockCache::default();
        let line = Line::from(vec![
            Span::styled("bold", Style::default().fg(Color::Red)),
            Span::raw(" normal "),
            Span::styled("blue", Style::default().fg(Color::Blue)),
        ]);
        cache.store(vec![line]);
        let lines = cache.get().unwrap();
        assert_eq!(lines[0].spans.len(), 3);
    }

    /// Version counter after many invalidations - verify it doesn't
    /// accidentally wrap to 0 (which would make stale data appear fresh).
    /// With u64, 10K invalidations is nowhere near overflow.
    #[test]
    fn cache_version_no_false_fresh_after_many_invalidations() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("data")]);
        for _ in 0..10_000 {
            cache.invalidate();
        }
        // Cache was invalidated 10K times without re-storing - must be stale
        assert!(cache.get().is_none());
    }

    /// Invalidate, store, invalidate, store - alternating pattern.
    #[test]
    fn cache_alternating_invalidate_store() {
        let mut cache = BlockCache::default();
        for i in 0..100 {
            cache.invalidate();
            assert!(cache.get().is_none(), "stale after invalidate at iter {i}");
            cache.store(vec![Line::from(format!("v{i}"))]);
            assert!(cache.get().is_some(), "fresh after store at iter {i}");
        }
    }

    // BlockCache height

    #[test]
    fn cache_height_default_returns_none() {
        let cache = BlockCache::default();
        assert!(cache.height_at(80).is_none());
    }

    #[test]
    fn cache_store_with_height_then_height_at() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello")]);
        cache.set_height(1, 80);
        assert_eq!(cache.height_at(80), Some(1));
        assert!(cache.get().is_some());
    }

    #[test]
    fn cache_height_at_wrong_width_returns_none() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello")]);
        cache.set_height(1, 80);
        assert!(cache.height_at(120).is_none());
    }

    #[test]
    fn cache_height_invalidated_returns_none() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello")]);
        cache.set_height(1, 80);
        cache.invalidate();
        assert!(cache.height_at(80).is_none());
    }

    #[test]
    fn cache_store_without_height_has_no_height() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello")]);
        // store() without height leaves wrapped_width at 0
        assert!(cache.height_at(80).is_none());
    }

    #[test]
    fn cache_store_with_height_overwrite() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("old")]);
        cache.set_height(1, 80);
        cache.invalidate();
        cache.store(vec![Line::from("new long line")]);
        cache.set_height(3, 120);
        assert_eq!(cache.height_at(120), Some(3));
        assert!(cache.height_at(80).is_none());
    }

    // BlockCache set_height (separate from store)

    #[test]
    fn cache_set_height_after_store() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello")]);
        assert!(cache.height_at(80).is_none()); // no height yet
        cache.set_height(1, 80);
        assert_eq!(cache.height_at(80), Some(1));
        assert!(cache.get().is_some()); // lines still valid
    }

    #[test]
    fn cache_set_height_update_width() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello world")]);
        cache.set_height(1, 80);
        assert_eq!(cache.height_at(80), Some(1));
        // Re-measure at new width
        cache.set_height(2, 40);
        assert_eq!(cache.height_at(40), Some(2));
        assert!(cache.height_at(80).is_none()); // old width no longer valid
    }

    #[test]
    fn cache_set_height_invalidate_clears_height() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("data")]);
        cache.set_height(3, 80);
        cache.invalidate();
        assert!(cache.height_at(80).is_none()); // version mismatch
    }

    #[test]
    fn cache_set_height_on_invalidated_cache_returns_none() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("data")]);
        cache.invalidate(); // version != 0
        cache.set_height(5, 80);
        // height_at returns None because cache is stale (version != 0)
        assert!(cache.height_at(80).is_none());
    }

    #[test]
    fn cache_store_then_set_height_is_consistent() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("test")]);
        cache.set_height(2, 100);

        assert_eq!(cache.height_at(100), Some(2));
        assert_eq!(cache.get().unwrap().len(), 1);
    }

    #[test]
    fn cache_measure_and_set_height_from_segments() {
        let mut cache = BlockCache::default();
        let lines = vec![
            Line::from("alpha beta gamma delta epsilon"),
            Line::from("zeta eta theta iota kappa lambda"),
            Line::from("mu nu xi omicron pi rho sigma"),
        ];
        cache.store(lines.clone());
        let measured = cache.measure_and_set_height(16).expect("expected measured height");
        let expected = ratatui::widgets::Paragraph::new(ratatui::text::Text::from(lines))
            .wrap(ratatui::widgets::Wrap { trim: false })
            .line_count(16);
        assert_eq!(measured, expected);
        assert_eq!(cache.height_at(16), Some(expected));
    }

    #[test]
    fn cache_get_updates_last_access_tick() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("tick")]);
        let before = cache.last_access_tick();
        let _ = cache.get();
        let after = cache.last_access_tick();
        assert!(after > before);
    }
}
