//! The Map tab: a squarified treemap of the current folder. Folders are drawn as
//! tiles with their contents nested inside; the top-level tiles use the same colours
//! as the Folder tab's usage strip, and nested tiles are lighter shades of their parent.

use disklens_core::util::human;
use disklens_core::{Tree, NONE};
use gtk::cairo::Context;
use gtk::pango;

/// GNOME palette. The first four match `.accent-N` in style.css and the Folder tab.
const PALETTE: [(f64, f64, f64); 8] = [
    (0.208, 0.518, 0.894), // blue
    (0.180, 0.761, 0.494), // green
    (1.000, 0.471, 0.000), // orange
    (0.569, 0.255, 0.675), // purple
    (0.902, 0.647, 0.039), // yellow
    (0.878, 0.106, 0.141), // red
    (0.129, 0.565, 0.643), // teal
    (0.596, 0.416, 0.267), // brown
];
const GREY: (f64, f64, f64) = (0.55, 0.55, 0.58);

/// Below this size a tile isn't split further / doesn't get a label.
const NEST_MIN_W: f64 = 60.0;
const NEST_MIN_H: f64 = 46.0;
const HEADER_H: f64 = 18.0;
const GAP: f64 = 2.0;
const MAX_DEPTH: u32 = 6;

/// One drawn tile, for hit-testing (later tiles are drawn on top of earlier ones).
#[derive(Clone, Debug)]
pub struct Hit {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// The folder this tile is, or NONE for a file / "smaller items" tile.
    pub dir: u32,
    /// The folder the tile lives in.
    pub parent: u32,
    /// For file tiles: index into `parent`'s files.
    pub file: Option<u32>,
    pub value: u64,
    pub label: String,
}

impl Hit {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Index of the topmost tile under (x, y).
pub fn hit(hits: &[Hit], x: f64, y: f64) -> Option<usize> {
    hits.iter().rposition(|h| h.contains(x, y))
}

fn worst(sum: f64, min: f64, max: f64, short: f64) -> f64 {
    let (s2, w2) = (sum * sum, short * short);
    (w2 * max / s2).max(s2 / (w2 * min))
}

/// Squarified treemap layout (Bruls et al.). `values` must be > 0 and sorted largest first.
pub fn squarify(values: &[f64], mut x: f64, mut y: f64, mut w: f64, mut h: f64) -> Vec<(f64, f64, f64, f64)> {
    let total: f64 = values.iter().sum();
    if total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return vec![(x, y, 0.0, 0.0); values.len()];
    }
    let scale = w * h / total;
    let vals: Vec<f64> = values.iter().map(|v| v * scale).collect();
    let mut out = Vec::with_capacity(vals.len());
    let mut i = 0;
    while i < vals.len() {
        let short = w.min(h).max(1e-9);
        let (mut sum, mut min, mut max) = (vals[i], vals[i], vals[i]);
        let mut j = i + 1;
        while j < vals.len() {
            let v = vals[j];
            if worst(sum + v, min.min(v), max.max(v), short) > worst(sum, min, max, short) {
                break;
            }
            sum += v;
            min = min.min(v);
            max = max.max(v);
            j += 1;
        }
        if w >= h {
            let cw = if h > 0.0 { sum / h } else { 0.0 };
            let mut yy = y;
            for v in &vals[i..j] {
                let rh = if cw > 0.0 { v / cw } else { 0.0 };
                out.push((x, yy, cw, rh));
                yy += rh;
            }
            x += cw;
            w -= cw;
        } else {
            let rh = if w > 0.0 { sum / w } else { 0.0 };
            let mut xx = x;
            for v in &vals[i..j] {
                let cw = if rh > 0.0 { v / rh } else { 0.0 };
                out.push((xx, y, cw, rh));
                xx += cw;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
    out
}

enum Item {
    Dir(u32),
    File(u32),
    /// (count) of items too small to draw on their own
    Rest(usize),
}

fn items(tree: &Tree, dir: u32, ondisk: bool, show_hidden: bool, limit: usize) -> Vec<(u64, Item)> {
    let d = tree.get(dir);
    let vis = |n: &[u8]| show_hidden || n.first() != Some(&b'.');
    let mut v: Vec<(u64, Item)> = d
        .dirs
        .iter()
        .filter(|&&c| vis(&tree.get(c).name))
        .map(|&c| (tree.get(c).value(ondisk), Item::Dir(c)))
        .chain(
            d.files
                .iter()
                .enumerate()
                .filter(|(_, f)| f.counted && !f.removed && vis(d.file_name_bytes(f)))
                .map(|(i, f)| (f.value(ondisk), Item::File(i as u32))),
        )
        .filter(|(s, _)| *s > 0)
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    if v.len() > limit {
        let rest: Vec<_> = v.drain(limit..).collect();
        v.push((rest.iter().map(|r| r.0).sum(), Item::Rest(rest.len())));
    }
    v
}

fn mix(c: (f64, f64, f64), t: f64) -> (f64, f64, f64) {
    (c.0 + (1.0 - c.0) * t, c.1 + (1.0 - c.1) * t, c.2 + (1.0 - c.2) * t)
}

fn luminance(c: (f64, f64, f64)) -> f64 {
    0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2
}

fn rounded(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

pub struct Painter<'a> {
    pub cr: &'a Context,
    pub layout: pango::Layout,
    pub tree: &'a Tree,
    pub ondisk: bool,
    pub show_hidden: bool,
    pub hits: Vec<Hit>,
}

impl Painter<'_> {
    /// Name and size, each on one line and shortened with "…" if needed (size goes on the
    /// same line when the tile is too short for two). `two_lines=false` for folder headers.
    #[allow(clippy::too_many_arguments)]
    fn label(&self, name: &str, size: u64, x: f64, y: f64, w: f64, h: f64, bg: (f64, f64, f64), bold: bool, two_lines: bool) {
        if w < 40.0 || h < 14.0 {
            return;
        }
        let l = &self.layout;
        let mut fd = l.context().font_description().unwrap_or_default();
        fd.set_size((8.5 * pango::SCALE as f64) as i32);
        fd.set_weight(if bold { pango::Weight::Bold } else { pango::Weight::Normal });
        l.set_font_description(Some(&fd));
        // tiles too narrow for the size and a few letters of the name stay unlabelled
        let size_text = human(size);
        l.set_width(-1);
        l.set_text(&size_text);
        if (l.pixel_size().0 as f64) > w - 8.0 {
            return;
        }
        let text = if two_lines && h >= 30.0 { format!("{name}\n{size_text}") } else { format!("{name}  {size_text}") };
        l.set_text(&text);
        l.set_width(((w - 8.0).max(1.0) * pango::SCALE as f64) as i32);
        l.set_height(-1); // every line ellipsized on its own
        l.set_ellipsize(pango::EllipsizeMode::End);
        if luminance(bg) > 0.6 {
            self.cr.set_source_rgba(0.0, 0.0, 0.0, 0.85);
        } else {
            self.cr.set_source_rgb(1.0, 1.0, 1.0);
        }
        self.cr.move_to(x + 4.0, y + 2.0);
        pangocairo::functions::show_layout(self.cr, l);
    }

    /// Lay out `dir`'s children in the box and recurse into folders big enough to show contents.
    pub fn level(&mut self, dir: u32, x: f64, y: f64, w: f64, h: f64, depth: u32, base: Option<(f64, f64, f64)>) {
        let list = items(self.tree, dir, self.ondisk, self.show_hidden, if depth == 0 { 300 } else { 120 });
        let vals: Vec<f64> = list.iter().map(|i| i.0 as f64).collect();
        let rects = squarify(&vals, x, y, w, h);
        let radius = (8.0 - depth as f64 * 2.0).max(2.0);
        for (idx, ((value, item), (rx, ry, rw, rh))) in list.iter().zip(rects).enumerate() {
            // inset by half the gap on each side so neighbours are separated
            let (tx, ty, tw, th) = (rx + GAP / 2.0, ry + GAP / 2.0, rw - GAP, rh - GAP);
            if tw < 1.0 || th < 1.0 {
                continue;
            }
            let own = base.unwrap_or(PALETTE[idx % PALETTE.len()]);
            let (colour, name, hit_dir, file) = match item {
                Item::Rest(n) => (mix(GREY, 0.15 * depth as f64), format!("{n} smaller items"), NONE, None),
                Item::File(i) => {
                    let d = self.tree.get(dir);
                    let f = &d.files[*i as usize];
                    (mix(own, (0.40 + 0.08 * depth as f64).min(0.80)), d.file_name(f).into_owned(), NONE, Some(*i))
                }
                Item::Dir(c) => (mix(own, (0.14 * depth as f64).min(0.70)), self.tree.get(*c).name_str().into_owned(), *c, None),
            };
            self.cr.set_source_rgb(colour.0, colour.1, colour.2);
            rounded(self.cr, tx, ty, tw, th, radius);
            let _ = self.cr.fill();
            self.hits.push(Hit { x: tx, y: ty, w: tw, h: th, dir: hit_dir, parent: dir, file, value: *value, label: name.clone() });
            match item {
                Item::Dir(c) if tw > NEST_MIN_W && th > NEST_MIN_H && depth < MAX_DEPTH => {
                    self.label(&name, *value, tx, ty, tw, HEADER_H, colour, true, false);
                    self.level(*c, tx + 2.0, ty + HEADER_H, tw - 4.0, th - HEADER_H - 2.0, depth + 1, Some(own));
                }
                Item::Dir(_) => self.label(&name, *value, tx, ty, tw, th, colour, true, true),
                _ => self.label(&name, *value, tx, ty, tw, th, colour, false, true),
            }
        }
    }
}

/// Outline the hovered tile.
pub fn draw_hover(cr: &Context, h: &Hit, fg: (f64, f64, f64)) {
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.9);
    cr.set_line_width(2.0);
    rounded(cr, h.x + 1.0, h.y + 1.0, h.w - 2.0, h.h - 2.0, 4.0);
    let _ = cr.stroke();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
    rounded(cr, h.x + 1.0, h.y + 1.0, h.w - 2.0, h.h - 2.0, 4.0);
    let _ = cr.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_fills_the_box_exactly() {
        let vals = [600.0, 300.0, 50.0, 30.0, 15.0, 5.0];
        let r = squarify(&vals, 0.0, 0.0, 400.0, 250.0);
        let area: f64 = r.iter().map(|t| t.2 * t.3).sum();
        assert!((area - 400.0 * 250.0).abs() < 1e-6);
        for ((_, _, w, h), v) in r.iter().zip(vals) {
            assert!((w * h - v / 1000.0 * 100_000.0).abs() < 1e-6, "area proportional to value");
        }
        for (x, y, w, h) in &r {
            assert!(*x >= -1e-9 && *y >= -1e-9 && x + w <= 400.0 + 1e-6 && y + h <= 250.0 + 1e-6);
        }
    }

    #[test]
    fn squarify_empty_and_degenerate() {
        assert!(squarify(&[], 0.0, 0.0, 10.0, 10.0).is_empty());
        assert_eq!(squarify(&[1.0], 0.0, 0.0, 0.0, 10.0), vec![(0.0, 0.0, 0.0, 0.0)]);
    }
}
