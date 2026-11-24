//! Shatter the painted track label and cover, then assemble the new ones.

use egui::epaint::{ClippedShape, Mesh, Primitive, Vertex};
use egui::layers::ShapeIdx;
use egui::{Pos2, Rect, Shape, TextureId, Ui, Vec2, pos2, vec2};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECS: f64 = 0.68;
const ART_HANDOFF: f64 = 0.16;
const PART_ORDER: usize = 1024;
pub(super) const PARTS: usize = 5;

#[derive(PartialEq, Eq)]
pub(super) struct TrackKey {
    epoch: u64,
    listed: Option<String>,
    heard: String,
}

impl TrackKey {
    pub(super) fn new(epoch: u64, listed: Option<&str>, heard: &str) -> Self {
        Self { epoch, listed: listed.map(str::to_owned), heard: heard.to_owned() }
    }
}

type Span = (ShapeIdx, ShapeIdx, Rect);
type Snapshots = [Option<Snapshot>; PARTS];

pub(super) fn mark(ui: &Ui) -> ShapeIdx {
    ui.ctx().graphics_mut(|g| g.entry(ui.layer_id()).next_idx())
}

pub(super) fn paint_clock(ui: &Ui, at: Pos2, text: Arc<egui::Galley>, color: egui::Color32) -> Span {
    let start = mark(ui);
    let rect = Rect::from_min_size(at, text.size());
    ui.painter().galley(at, text, color);
    (start, mark(ui), rect)
}

fn state_id() -> egui::Id {
    // The same bottom bar can be drawn in a different scope over Now Playing.
    egui::Id::unique("main-player-track-particles")
}

pub(in crate::ui) fn clear(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove_temp::<Option<Arc<State>>>(state_id()));
}

#[derive(Clone)]
struct Snapshot {
    shapes: Vec<ClippedShape>,
    rect: Rect,
}

fn cover_stamp(cover: &Snapshot) -> Vec<(TextureId, u8)> {
    fn stamp(shape: &Shape, to: &mut Vec<(TextureId, u8)>) {
        match shape {
            Shape::Rect(r) => {
                if let Some(brush) = &r.brush { to.push((brush.fill_texture_id, r.fill.a())); }
            }
            Shape::Mesh(m) if m.texture_id != TextureId::default() => {
                to.push((m.texture_id, m.vertices.iter().map(|v| v.color.a()).max().unwrap_or(0)));
            }
            Shape::Vec(shapes) => {
                for shape in shapes { stamp(shape, to); }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for shape in &cover.shapes { stamp(&shape.shape, &mut out); }
    out
}

#[derive(Clone)]
struct Tile {
    center: Pos2,
    meshes: Vec<Fragment>,
}

#[derive(Clone)]
struct Fragment {
    order: usize,
    mesh: Mesh,
    rounded: Vec<Pos2>,
}

struct Carrier {
    from: Tile,
    via: Pos2,
    to: Tile,
    delay: f32,
    arrive: f32,
    drift: Vec2,
    spin: f32,
}

struct Flight {
    at: f64,
    carriers: Vec<Carrier>,
    bounds: Rect,
    assembled_cover: Vec<(TextureId, u8)>,
    assembled_labels: (String, String, [bool; 2]),
    handoff: Option<(f64, [bool; PARTS])>,
}

struct Pose<'a> {
    tile: &'a Tile,
    center: Pos2,
    scale: f32,
    rounded: f32,
    sin: f32,
    cos: f32,
}

impl Pose<'_> {
    fn transform(&self, point: Pos2) -> Pos2 {
        let p = (point - self.tile.center) * self.scale;
        self.center + vec2(p.x * self.cos - p.y * self.sin, p.x * self.sin + p.y * self.cos)
    }

    fn vertex(&self, point: Pos2, rounded: Pos2) -> Pos2 {
        self.transform(point.lerp(rounded, self.rounded))
    }
}

impl Flight {
    fn progress(&self, now: f64) -> f32 {
        ((now - self.at) / SECS).clamp(0.0, 1.0) as f32
    }

    fn pose(carrier: &Carrier, t: f32) -> Pose<'_> {
        let t = ((t - carrier.delay) / (carrier.arrive - carrier.delay)).clamp(0.0, 1.0);
        let smooth = |v: f32| v * v * (3.0 - 2.0 * v);
        let arc = |a: Pos2, b: Pos2, k: f32, bend: f32| {
            if k <= 0.0 { return a; }
            if k >= 1.0 { return b; }
            a.lerp(b, k) + carrier.drift * (bend * (std::f32::consts::PI * k).sin())
        };
        let (tile, center, spread) = if t < 0.5 {
            let k = smooth(t * 2.0);
            (&carrier.from, arc(carrier.from.center, carrier.via, k, 1.0), k)
        } else {
            let k = smooth(t * 2.0 - 1.0);
            (&carrier.to, arc(carrier.via, carrier.to.center, k, -0.7), 1.0 - k)
        };
        let (sin, cos) = (carrier.spin * spread).sin_cos();
        Pose { tile, center, scale: egui::lerp(1.0..=0.32, spread), rounded: spread, sin, cos }
    }

    fn current(&self, now: f64) -> Vec<Tile> {
        let t = self.progress(now);
        self.carriers.iter().map(|carrier| {
            let pose = Self::pose(carrier, t);
            let mut tile = pose.tile.clone();
            tile.center = pose.center;
            for fragment in &mut tile.meshes {
                for (v, rounded) in fragment.mesh.vertices.iter_mut().zip(&mut fragment.rounded) {
                    v.pos = pose.vertex(v.pos, *rounded);
                    *rounded = pose.transform(*rounded);
                }
            }
            tile
        }).collect()
    }
}

struct State {
    track: TrackKey,
    viewport: Vec2,
    ppp: f32,
    last_draw: Instant,
    latest: Snapshots,
    flight: Option<Flight>,
}

pub(super) fn show(ui: &Ui, track: TrackKey, labels: (&str, &str), spans: [Option<Span>; PARTS], bar: Rect, now: f64) {
    let ctx = ui.ctx();
    let layer = ui.layer_id();
    let latest = ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        spans.map(|span| span.map(|(start,end,rect)| Snapshot {
            shapes: list.all_entries().skip(start.0).take(end.0 - start.0).cloned().collect(), rect,
        }))
    });
    let cover = || latest[0].as_ref().map_or_else(Vec::new, cover_stamp);
    let clocks = [latest[3].is_some(), latest[4].is_some()];
    let ppp = ctx.pixels_per_point();
    let viewport = ctx.content_rect().size();
    let draw_time = Instant::now();
    let id = state_id();
    // Use an Arc only to make the egui temporary value cloneable. This view is the sole owner.
    let state = ctx.data_mut(|d| d.remove_temp::<Option<Arc<State>>>(id)).flatten().and_then(|s| Arc::try_unwrap(s).ok());
    let mut flight = None;
    if let Some(old) = state.filter(|s| s.viewport == viewport && s.ppp == ppp) {
        // A visible panel can legitimately be idle while paused. Keep its settled
        // snapshot, but never resume an in-flight animation after a long gap.
        let recent_flight = draw_time.duration_since(old.last_draw) < Duration::from_secs_f64(SECS);
        if old.track != track {
            let active = old.flight.as_ref().filter(|f| recent_flight && f.progress(now) < 1.0);
            let ongoing = active.map_or(Rect::NOTHING, |f| f.bounds);
            let bounds = old.latest.iter().chain(&latest).flatten().fold(Rect::NOTHING, |r, s| r.union(s.rect)).expand(4.0).union(ongoing).intersect(bar);
            let from = active.map_or_else(|| tiles(ctx, old.latest, ppp), |f| f.current(now));
            let to = tiles(ctx, latest.clone(), ppp);
            flight = join(from, to, bounds, now, cover(), (labels.0.to_owned(), labels.1.to_owned(), clocks));
        } else if let Some(mut active) = old.flight.filter(|_| recent_flight) {
            if active.progress(now) < 1.0 {
                flight = Some(active);
            } else {
                // Late art or metadata enters in place. Time ticks use the ordinary
                // label again at arrival, without retriggering or extending the flight.
                if active.handoff.is_none() {
                    let (title, artist, previous_clocks) = &active.assembled_labels;
                    let changed = [
                        active.assembled_cover != cover(),
                        title != labels.0,
                        artist != labels.1,
                        previous_clocks[0] != clocks[0],
                        previous_clocks[1] != clocks[1],
                    ];
                    if changed.iter().any(|c| *c) { active.handoff = Some((now, changed)); }
                }
                if active.handoff.is_some_and(|(at,_)| now - at < ART_HANDOFF) {
                    flight = Some(active);
                }
            }
        }
    }
    if let Some(flight) = &flight {
        let mix = flight.handoff.map(|(at, parts)| (parts, ((now - at) / ART_HANDOFF) as f32));
        ctx.graphics_mut(|g| {
            let list = g.entry(layer);
            for (part, span) in spans.iter().enumerate() {
                if let Some((start,end,_)) = span && mix.is_none_or(|(parts,_)| parts[part]) {
                    for i in start.0..end.0 { list.reset_shape(ShapeIdx(i)); }
                }
            }
        });
        paint(ui, flight, now, mix.map(|(parts,k)| (parts,1.0-k)));
        if let Some((parts,k)) = mix {
            for shape in latest.iter().zip(parts).filter(|(_,show)| *show).filter_map(|(part,_)| part.as_ref()).flat_map(|part| &part.shapes) {
                let mut painter = ui.painter().with_clip_rect(shape.clip_rect);
                painter.multiply_opacity(k);
                painter.add(shape.shape.clone());
            }
        }
        ctx.request_repaint_after(Duration::from_millis(16));
    }
    ctx.data_mut(|d| d.insert_temp(id, Some(Arc::new(State { track, viewport, ppp, last_draw: draw_time, latest, flight }))));
}

fn join(mut a: Vec<Tile>, mut b: Vec<Tile>, bounds: Rect, at: f64, assembled_cover: Vec<(TextureId, u8)>, assembled_labels: (String, String, [bool; 2])) -> Option<Flight> {
    let n = a.len().min(b.len());
    if n == 0 || !bounds.is_positive() { return None; }
    let (a, b) = (group(&mut a, n), group(&mut b, n));
    let carriers = a.into_iter().zip(b).enumerate().map(|(i, (from, to))| {
        let phase = |k: u32| {
            let bits = ((i as u32 + 1).wrapping_mul(0x9e3779b9) ^ k).wrapping_mul(0x85ebca6b);
            ((bits ^ (bits >> 13)) & 1023) as f32 / 1023.0
        };
        let via = bounds.min + vec2(bounds.width() * (0.07 + 0.86 * phase(47)), bounds.height() * (0.11 + 0.78 * phase(83)));
        let drift = vec2((phase(161)-0.5) * bounds.width() * 0.23, (phase(221)-0.5) * bounds.height() * 0.42);
        Carrier { from, via, to, delay: phase(111)*0.12, arrive: 0.86 + phase(177)*0.14, drift, spin: (phase(139)-0.5)*2.5 }
    }).collect();
    Some(Flight { at, carriers, bounds, assembled_cover, assembled_labels, handoff: None })
}

fn group(tiles: &mut Vec<Tile>, n: usize) -> Vec<Tile> {
    tiles.sort_by(|a,b| a.center.x.total_cmp(&b.center.x).then(a.center.y.total_cmp(&b.center.y)));
    let count = tiles.len();
    let mut tiles = std::mem::take(tiles).into_iter();
    (0..n).map(|i| {
        let mut tile = tiles.next().expect("one tile per carrier");
        let extras = (i + 1) * count / n - i * count / n - 1;
        for part in tiles.by_ref().take(extras) {
            tile.meshes.extend(part.meshes);
        }
        tile
    }).collect()
}

fn paint(ui: &Ui, flight: &Flight, now: f64, fade: Option<([bool; PARTS], f32)>) {
    let t = flight.progress(now);
    let [w, h] = ui.ctx().fonts(|f| f.font_image_size());
    let atlas = vec2(w as f32, h as f32);
    let mut batches: Vec<(usize, Mesh)> = Vec::new();
    for carrier in &flight.carriers {
        let pose = Flight::pose(carrier, t);
        for fragment in &pose.tile.meshes {
            let src = &fragment.mesh;
            if fade.is_some_and(|(show,_)| !show[fragment.order / PART_ORDER]) { continue; }
            let mesh = batch(&mut batches, fragment.order, src.texture_id);
            let offset = mesh.vertices.len() as u32;
            mesh.indices.extend(src.indices.iter().map(|i| offset + *i));
            mesh.vertices.extend(src.vertices.iter().zip(&fragment.rounded).map(|(v, rounded)| {
                let uv = if src.texture_id == TextureId::default() { pos2(v.uv.x / atlas.x, v.uv.y / atlas.y) } else { v.uv };
                Vertex { pos: pose.vertex(v.pos, *rounded), uv, color: v.color.gamma_multiply(fade.map_or(1.0, |(_,k)| k)) }
            }));
        }
    }
    batches.sort_by_key(|(order, _)| *order);
    let painter = ui.painter().with_clip_rect(flight.bounds.intersect(ui.clip_rect()));
    for (_, mesh) in batches { painter.add(Shape::mesh(mesh)); }
}

fn batch(batches: &mut Vec<(usize, Mesh)>, order: usize, tex: TextureId) -> &mut Mesh {
    let i = batches.iter().position(|(at,m)| *at == order && m.texture_id == tex).unwrap_or_else(|| {
        batches.push((order, Mesh::with_texture(tex)));
        batches.len() - 1
    });
    &mut batches[i].1
}

fn tiles(ctx: &egui::Context, snapshots: Snapshots, ppp: f32) -> Vec<Tile> {
    let mut tiles = Vec::new();
    for (span, snapshot) in snapshots.into_iter().enumerate() {
        let Some(snapshot) = snapshot else { continue };
        let rect = snapshot.rect;
        if !rect.is_positive() { continue; }
        let step = ((rect.width() * rect.height()) / 96.0).sqrt().max(6.0);
        let (nx, ny) = ((rect.width()/step).ceil() as usize, (rect.height()/step).ceil() as usize);
        let mut cells: Vec<Tile> = (0..nx*ny).map(|i| {
            let offset = vec2((i%nx) as f32, (i/nx) as f32) * step;
            Tile { center: (rect.min + offset + Vec2::splat(step/2.0)).min(rect.max), meshes: Vec::new() }
        }).collect();
        let primitives = ctx.tessellate(snapshot.shapes, ppp);
        let [aw, ah] = ctx.fonts(|f| f.font_image_size());
        let mut polygon = Vec::with_capacity(8);
        let mut scratch = Vec::with_capacity(8);
        for (i, draw) in primitives.into_iter().enumerate() {
            let Primitive::Mesh(mesh) = draw.primitive else { continue };
            for triangle in mesh.indices.as_chunks::<3>().0 {
                let points = triangle.map(|k| mesh.vertices[k as usize]);
                let ink = points[0].pos;
                let mut r = Rect::from_two_pos(ink, points[1].pos);
                r.extend_with(points[2].pos);
                r = r.intersect(rect).intersect(draw.clip_rect);
                if !r.is_positive() { continue; }
                let column = |x: f32| (((x-rect.left()) / step) as usize).min(nx-1);
                let row = |y: f32| (((y-rect.top()) / step) as usize).min(ny-1);
                for cy in row(r.top())..=row(r.bottom()) {
                    for cx in column(r.left())..=column(r.right()) {
                        let at = rect.min + vec2(cx as f32, cy as f32)*step;
                        let clip = Rect::from_min_size(at, Vec2::splat(step)).intersect(r);
                        polygon.clear();
                        polygon.extend(points);
                        cut(&mut polygon, &mut scratch, |p| p.x - clip.left());
                        cut(&mut polygon, &mut scratch, |p| clip.right() - p.x);
                        cut(&mut polygon, &mut scratch, |p| p.y - clip.top());
                        cut(&mut polygon, &mut scratch, |p| clip.bottom() - p.y);
                        if polygon.len() < 3 { continue; }
                        let tile = &mut cells[cy*nx+cx];
                        let order = span*PART_ORDER+i;
                        let slot = tile.meshes.iter().position(|part| part.order == order && part.mesh.texture_id == mesh.texture_id).unwrap_or_else(|| {
                            tile.meshes.push(Fragment { order, mesh: Mesh::with_texture(mesh.texture_id), rounded: Vec::new() });
                            tile.meshes.len() - 1
                        });
                        let part = &mut tile.meshes[slot];
                        let n = part.mesh.vertices.len() as u32;
                        for j in 1..(2 * polygon.len() as u32 - 1) { part.mesh.indices.extend([n,n+j,n+j+1]); }
                        // Retain font UVs in texels when the atlas grows during an animation.
                        // Edge midpoints let each rectangle ease into a round fragment in
                        // flight, without losing any source pixels at either endpoint.
                        for j in 0..polygon.len() {
                            let a = polygon[j];
                            let b = polygon[(j+1) % polygon.len()];
                            for v in [a, interpolate(a,b,0.5)] {
                                let uv = if mesh.texture_id == TextureId::default() { pos2(v.uv.x * aw as f32, v.uv.y * ah as f32) } else { v.uv };
                                let offset = v.pos - tile.center;
                                let radius = step * 0.54;
                                let rounded = tile.center + offset * (radius / offset.length().max(radius));
                                part.mesh.vertices.push(Vertex { uv, ..v });
                                part.rounded.push(rounded);
                            }
                        }
                    }
                }
            }
        }
        tiles.extend(cells.into_iter().filter(|tile| !tile.meshes.is_empty()));
    }
    tiles
}

fn cut(poly: &mut Vec<Vertex>, scratch: &mut Vec<Vertex>, distance: impl Fn(Pos2)->f32) {
    let Some(&last) = poly.last() else { return };
    scratch.clear();
    let mut a = last;
    for &b in poly.iter() {
        let (da, db) = (distance(a.pos), distance(b.pos));
        if (da >= 0.0) != (db >= 0.0) {
            let k = da / (da - db);
            scratch.push(interpolate(a,b,k));
        }
        if db >= 0.0 { scratch.push(b); }
        a = b;
    }
    std::mem::swap(poly, scratch);
}

fn interpolate(a: Vertex, b: Vertex, k: f32) -> Vertex {
    Vertex { pos: a.pos.lerp(b.pos,k), uv: a.uv.lerp(b.uv,k), color: crate::theme::lerp_color(a.color,b.color,k) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_identity_distinguishes_restart_variant_and_replacement() {
        let a = TrackKey::new(9, Some("queue-a"), "song-a");
        assert!(a == TrackKey::new(9, Some("queue-a"), "song-a"));
        assert!(a != TrackKey::new(10, Some("queue-a"), "song-a"));
        assert!(a != TrackKey::new(9, Some("queue-a"), "song-a-video"));
        assert!(a != TrackKey::new(9, Some("queue-b"), "song-a"));
    }

    #[test]
    fn staggered_curves_reach_exact_endpoints_and_join_at_the_payload_swap() {
        let tile = |x, y| Tile { center: pos2(x,y), meshes: vec![] };
        let bounds = Rect::from_min_size(Pos2::ZERO, vec2(180.0,48.0));
        let flight = join(vec![tile(5.0,10.0),tile(30.0,20.0)], vec![tile(20.0,30.0),tile(50.0,22.0)], bounds, 0.0, vec![], (String::new(), String::new(), [true, false])).unwrap();
        assert_ne!(flight.carriers[0].delay, flight.carriers[1].delay);
        for c in &flight.carriers {
            let begin = Flight::pose(c,0.0);
            let end = Flight::pose(c,1.0);
            assert_eq!((begin.center,begin.scale,begin.rounded), (c.from.center,1.0,0.0));
            assert_eq!((end.center,end.scale,end.rounded), (c.to.center,1.0,0.0));
            let mid = (c.delay + c.arrive) / 2.0;
            let left = Flight::pose(c,mid-0.0001);
            let right = Flight::pose(c,mid+0.0001);
            assert!(left.center.distance(right.center)<0.01 && left.rounded>0.99);
        }
    }
}
